//! The HTTP side of team sharing. Reads are GETs. Writes are POSTs: stop and
//! permission on a chat, each handed to the app's action handler and only for
//! chats the sharer has opened to step-in; a batch of file changes, only for
//! workspaces that allow editing; and MCP messages from a teammate's agent,
//! which can write files and run commands in the workspace folder, only for
//! workspaces whose agents switch is on.

use std::io::Read as _;
use std::path::PathBuf;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tiny_http::{Header, Method, Request, Response, Server};

use crate::services::team_agent_tools::{
    AgentToolContext, GitAuthor, McpReply, RunningCommands, handle_mcp_message,
};
use crate::services::team_files_service::{SkipList, TeamFileKind, safe_relative_path};
use crate::services::team_share_service::{
    ActionHandler, BatchHandler, CallerIdentity, MAX_CHAT_WAIT, TEAM_ACP_CRATE_VERSION, TEAM_PROTOCOL_VERSION,
    TEAM_GIT_AUTHOR_HEADER, TEAM_REQUEST_HEADER, TeamAction, TeamActionError, TeamActionKind, TeamBatchRequest, TeamCaller, TeamHello,
    TeamShareHub, TeamWorkspaceList, is_plain_id, percent_decode,
};

/// Produces a workspace's diff. Behind a closure so tests don't need a
/// database or a git repository.
pub type DiffSource = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

/// A workspace's root, or None unless its "Share files" switch is on.
pub type FileSource = Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>;

/// A git bundle of a workspace's history without the given commits.
pub type BundleSource = Arc<dyn Fn(&str, &[String]) -> Result<Vec<u8>, String> + Send + Sync>;

/// The files a workspace never serves or accepts.
pub type SkipListSource = Arc<dyn Fn(&str) -> SkipList + Send + Sync>;

/// A workspace's root, or None unless its agents switch is on.
pub type AgentRootSource = Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>;

/// Whether a teammate's agent may write the absolute path in the workspace.
pub type AgentWriteCheck = Arc<dyn Fn(&str, &std::path::Path) -> Result<(), String> + Send + Sync>;

pub struct TeamServerConfig {
    pub bind: IpAddr,
    pub port: u16,
    pub display_name: String,
    /// Tailscale logins allowed to read, compared case-insensitively.
    pub allowed_logins: Vec<String>,
    pub files: FileSource,
    pub bundle_source: BundleSource,
    pub skip_list: SkipListSource,
    pub batch_handler: BatchHandler,
    pub agent_roots: AgentRootSource,
    pub agent_write_check: AgentWriteCheck,
}

/// Long-polls hold a thread each, so this caps what a misbehaving caller
/// can tie up.
const MAX_ACTIVE_REQUESTS: usize = 64;
/// Diffs past this are cut, so a vendored directory can't stall a viewer.
const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;
const MAX_ACTION_BODY_BYTES: usize = 16 * 1024;
/// Write carries whole files.
const MAX_AGENT_BODY_BYTES: usize = MAX_ACTION_BODY_BYTES * 64;
const MCP_SESSION_HEADER: &str = "Mcp-Session-Id";
const MAX_BATCH_BODY_BYTES: usize = 512 * 1024 * 1024;
pub const MAX_BUNDLE_HAVE: usize = 64;

struct Shared {
    hub: TeamShareHub,
    identity: Arc<dyn CallerIdentity>,
    diff_source: DiffSource,
    action_handler: ActionHandler,
    files: FileSource,
    bundle_source: BundleSource,
    skip_list: SkipListSource,
    batch_handler: BatchHandler,
    agent_roots: AgentRootSource,
    agent_write_check: AgentWriteCheck,
    running_commands: RunningCommands,
    hello: String,
    allowed_logins: Vec<String>,
    active_requests: AtomicUsize,
}

/// A running share server. Dropping it stops the server.
pub struct TeamShareServer {
    server: Arc<Server>,
    address: SocketAddr,
    accept_thread: Option<JoinHandle<()>>,
}

impl TeamShareServer {
    pub fn start(
        config: TeamServerConfig,
        hub: TeamShareHub,
        identity: Arc<dyn CallerIdentity>,
        diff_source: DiffSource,
        action_handler: ActionHandler,
    ) -> Result<Self, String> {
        let server = Server::http((config.bind, config.port))
            .map_err(|err| format!("Couldn't listen on {}:{}: {err}", config.bind, config.port))?;
        let address = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| "The share server has no IP address".to_string())?;
        let hello = serde_json::to_string(&TeamHello {
            name: config.display_name,
            protocol: TEAM_PROTOCOL_VERSION,
            acp_crate: TEAM_ACP_CRATE_VERSION.to_string(),
        })
        .map_err(|err| err.to_string())?;
        let shared = Arc::new(Shared {
            hub,
            identity,
            diff_source,
            action_handler,
            files: config.files,
            bundle_source: config.bundle_source,
            skip_list: config.skip_list,
            batch_handler: config.batch_handler,
            agent_roots: config.agent_roots,
            agent_write_check: config.agent_write_check,
            running_commands: RunningCommands::default(),
            hello,
            allowed_logins: config
                .allowed_logins
                .iter()
                .map(|login| login.trim().to_lowercase())
                .filter(|login| !login.is_empty())
                .collect(),
            active_requests: AtomicUsize::new(0),
        });
        let server = Arc::new(server);
        let accept_thread = std::thread::Builder::new()
            .name("mnemonic-team-share".into())
            .spawn({
                let server = server.clone();
                move || accept_requests(&server, &shared)
            })
            .map_err(|err| format!("Couldn't start the share server: {err}"))?;
        log::info!(target: "mnemonic_lib", "Sharing workspaces with the team on {address}");
        Ok(Self {
            server,
            address,
            accept_thread: Some(accept_thread),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for TeamShareServer {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(thread) = self.accept_thread.take() {
            if thread.join().is_err() {
                log::error!(target: "mnemonic_lib", "The share server thread panicked");
            }
        }
        log::info!(target: "mnemonic_lib", "Stopped sharing workspaces on {}", self.address);
    }
}

fn accept_requests(server: &Server, shared: &Arc<Shared>) {
    loop {
        let request = match server.recv() {
            Ok(request) => request,
            // `unblock` makes `recv` fail, which is how the server stops.
            Err(_) => break,
        };
        if shared.active_requests.fetch_add(1, Ordering::SeqCst) >= MAX_ACTIVE_REQUESTS {
            shared.active_requests.fetch_sub(1, Ordering::SeqCst);
            respond(request, text_response(503, "Too many requests"));
            continue;
        }
        let shared_for_request = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("mnemonic-team-request".into())
            .spawn(move || {
                handle(request, &shared_for_request);
                shared_for_request
                    .active_requests
                    .fetch_sub(1, Ordering::SeqCst);
            });
        if let Err(err) = spawned {
            shared.active_requests.fetch_sub(1, Ordering::SeqCst);
            log::error!(target: "mnemonic_lib", "Couldn't handle a team request: {err}");
        }
    }
}

fn handle(mut request: Request, shared: &Shared) {
    if let Err((status, reason)) = check_request(&request) {
        respond(request, text_response(status, reason));
        return;
    }
    let Some(address) = request.remote_addr().map(|address| address.ip()) else {
        respond(request, text_response(403, "Unknown caller"));
        return;
    };
    let caller = match shared.identity.caller(address) {
        Ok(Some(caller))
            if shared
                .allowed_logins
                .contains(&caller.login.trim().to_lowercase()) =>
        {
            caller
        }
        Ok(Some(caller)) => {
            log::info!(
                target: "mnemonic_lib",
                "Refused a team request from {} ({address})",
                caller.login
            );
            respond(request, text_response(403, "Not on the allow list"));
            return;
        }
        Ok(None) => {
            respond(request, text_response(403, "Unknown caller"));
            return;
        }
        Err(err) => {
            log::warn!(target: "mnemonic_lib", "Couldn't identify {address}: {err}");
            respond(request, text_response(503, "Couldn't check who you are"));
            return;
        }
    };
    shared.hub.note_request();

    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let method = request.method().clone();
    if method == Method::Post {
        let response = match segments.as_slice() {
            ["v1", "chats", chat_id, action @ ("stop" | "permission")]
                if is_plain_id(chat_id) =>
            {
                handle_action(&mut request, shared, &caller, chat_id, action)
            }
            ["v1", "workspaces", workspace_id, "changes"] if is_plain_id(workspace_id) => {
                handle_changes(&mut request, shared, &caller, workspace_id)
            }
            ["v1", "workspaces", workspace_id, "agent"] if is_plain_id(workspace_id) => {
                handle_agent(&mut request, shared, &caller, workspace_id)
            }
            _ => text_response(404, "Not found"),
        };
        respond(request, response);
        return;
    }
    if method != Method::Get {
        respond(request, text_response(405, "Only GET and POST are supported"));
        return;
    }
    let response = match segments.as_slice() {
        ["v1", "hello"] => json_response(200, shared.hello.clone()),
        ["v1", "workspaces"] => {
            let list = TeamWorkspaceList {
                workspaces: shared.hub.workspaces(),
            };
            match serde_json::to_string(&list) {
                Ok(body) => json_response(200, body),
                Err(err) => text_response(500, &err.to_string()),
            }
        }
        // Claude's streamable HTTP client takes a 405 to mean there is no
        // server-sent event stream and goes on with plain POSTs.
        ["v1", "workspaces", _, "agent"] => text_response(405, "Use POST"),
        ["v1", "workspaces", workspace_id, "diff"] if is_plain_id(workspace_id) => {
            if shared.hub.has_workspace(workspace_id) {
                match (shared.diff_source)(workspace_id) {
                    Ok(diff) => text_response(200, &truncate_diff(diff)),
                    Err(err) => text_response(500, &err),
                }
            } else {
                text_response(404, "No such workspace")
            }
        }
        ["v1", "workspaces", workspace_id, "files"] if is_plain_id(workspace_id) => {
            if let Err(refusal) = shared_root(shared, workspace_id, false) {
                respond(request, refusal);
                return;
            }
            let cursor = query_value(query, "cursor");
            let wait = query_value(query, "wait")
                .and_then(|wait| wait.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or_default()
                .min(MAX_CHAT_WAIT);
            match shared.hub.wait_file_manifest(workspace_id, cursor, wait) {
                Some(update) => match serde_json::to_string(&update) {
                    Ok(body) => json_response(200, body),
                    Err(err) => text_response(500, &err.to_string()),
                },
                None => text_response(404, "No files yet"),
            }
        }
        ["v1", "workspaces", workspace_id, "file"] if is_plain_id(workspace_id) => {
            match serve_file(shared, workspace_id, query) {
                Ok(response) => {
                    respond(request, response);
                    return;
                }
                Err(refusal) => refusal,
            }
        }
        ["v1", "workspaces", workspace_id, "bundle"] if is_plain_id(workspace_id) => {
            match shared_root(shared, workspace_id, false) {
                Err(refusal) => refusal,
                Ok(_) => serve_bundle(shared, workspace_id, query),
            }
        }
        ["v1", "chats", chat_id] if is_plain_id(chat_id) => {
            let cursor = query_value(query, "cursor");
            let wait = query_value(query, "wait")
                .and_then(|wait| wait.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or_default()
                .min(MAX_CHAT_WAIT);
            match shared.hub.wait_chat_update(chat_id, cursor, wait) {
                Some(update) => match serde_json::to_string(&update) {
                    Ok(body) => json_response(200, body),
                    Err(err) => text_response(500, &err.to_string()),
                },
                None => text_response(404, "Not open"),
            }
        }
        _ => text_response(404, "Not found"),
    };
    respond(request, response);
}

/// Identity is the caller's machine, so any program there acts as its
/// owner, including a web page open in a browser. These refuse what a page
/// can send: browsers add `Origin` to cross-origin requests, a foreign
/// `Host` means DNS rebinding, and a JSON body or a custom header makes a
/// browser ask first with a CORS preflight, which this server never answers.
fn check_request(request: &Request) -> Result<(), (u16, &'static str)> {
    let header_value = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|header| header.field.equiv(name))
            .map(|header| header.value.as_str())
    };
    if header_value("Origin").is_some() {
        return Err((403, "Requests from web pages are refused"));
    }
    if !header_value("Host").is_some_and(is_allowed_host) {
        return Err((403, "Unexpected host name"));
    }
    if *request.method() == Method::Post {
        let is_json = header_value("Content-Type").is_some_and(|value| {
            value.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("application/json")
            })
        });
        if !is_json {
            return Err((400, "Expected a JSON body"));
        }
        if header_value(TEAM_REQUEST_HEADER).is_none_or(|value| value.trim().is_empty()) {
            return Err((400, "Not a Mnemonic request"));
        }
    }
    Ok(())
}

/// The names a teammate reaches this machine by: an address, a Tailscale
/// MagicDNS name, or a bare machine name.
fn is_allowed_host(host: &str) -> bool {
    let host = host.trim();
    if let Some(bracketed) = host.strip_prefix('[') {
        return bracketed
            .split_once(']')
            .is_some_and(|(address, _)| address.parse::<Ipv6Addr>().is_ok());
    }
    let name = host.split_once(':').map_or(host, |(name, _)| name);
    if name.parse::<IpAddr>().is_ok() {
        return true;
    }
    let is_name_character =
        |character: char| character.is_ascii_alphanumeric() || character == '-' || character == '.';
    if name.is_empty() || !name.chars().all(is_name_character) {
        return false;
    }
    !name.contains('.')
        || name
            .to_ascii_lowercase()
            .strip_suffix(".ts.net")
            .is_some_and(|prefix| !prefix.is_empty() && !prefix.ends_with('.'))
}

fn handle_action(
    request: &mut Request,
    shared: &Shared,
    caller: &TeamCaller,
    chat_id: &str,
    action: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    if !shared.hub.allows_step_in(chat_id) {
        return text_response(403, "Stepping in is off for this workspace");
    }
    let mut body = Vec::new();
    let read = request
        .as_reader()
        .take(MAX_ACTION_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body);
    if let Err(err) = read {
        return text_response(400, &format!("Couldn't read the request: {err}"));
    }
    if body.len() > MAX_ACTION_BODY_BYTES {
        return text_response(413, "The request is too long");
    }
    let kind = match parse_action(action, &body) {
        Ok(kind) => kind,
        Err(reason) => return text_response(400, &reason),
    };
    let outcome = (shared.action_handler)(TeamAction {
        chat_id: chat_id.to_string(),
        from: caller.name.clone(),
        login: caller.login.clone(),
        kind,
    });
    outcome_response(outcome)
}

fn handle_agent(
    request: &mut Request,
    shared: &Shared,
    caller: &TeamCaller,
    workspace_id: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let Some(workspace) = shared
        .hub
        .workspaces()
        .into_iter()
        .find(|workspace| workspace.id == workspace_id)
    else {
        return text_response(404, "No such workspace");
    };
    let root = workspace
        .agents
        .then(|| (shared.agent_roots)(workspace_id))
        .flatten();
    let Some(root) = root else {
        return text_response(403, "Teammates' agents can't work in this workspace");
    };
    let header_value = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|header| header.field.equiv(name))
            .map(|header| header.value.as_str().to_string())
    };
    let git_author = header_value(TEAM_GIT_AUTHOR_HEADER)
        .as_deref()
        .and_then(parse_git_author);
    let session = header_value(MCP_SESSION_HEADER).unwrap_or_default();
    if request
        .body_length()
        .is_some_and(|length| length > MAX_AGENT_BODY_BYTES)
    {
        return text_response(413, "The request is too long");
    }
    let mut body = Vec::new();
    let read = request
        .as_reader()
        .take(MAX_AGENT_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body);
    if let Err(err) = read {
        return text_response(400, &format!("Couldn't read the request: {err}"));
    }
    if body.len() > MAX_AGENT_BODY_BYTES {
        return text_response(413, "The request is too long");
    }

    let is_initialize = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .is_some_and(|message| {
            message.get("method").and_then(|method| method.as_str()) == Some("initialize")
        });
    let session = if is_initialize {
        uuid::Uuid::new_v4().to_string()
    } else {
        session
    };

    let context = AgentToolContext {
        root,
        git_author,
        can_write: Arc::new({
            let write_check = shared.agent_write_check.clone();
            let workspace_id = workspace_id.to_string();
            move |path| write_check(&workspace_id, path)
        }),
    };
    let caller_key = format!("{}:{session}", caller.login);
    let mut response =
        match handle_mcp_message(&context, &shared.running_commands, &caller_key, &body) {
            McpReply::Json(text) => json_response(200, text),
            McpReply::Accepted => Response::from_string("").with_status_code(202),
        };
    if is_initialize {
        if let Some(header) = header(MCP_SESSION_HEADER, &session) {
            response.add_header(header);
        }
    }
    response
}

fn parse_git_author(value: &str) -> Option<GitAuthor> {
    let (name, email) = value.trim().strip_suffix('>')?.split_once('<')?;
    let (name, email) = (name.trim(), email.trim());
    (!name.is_empty() && !email.is_empty()).then(|| GitAuthor {
        name: name.to_string(),
        email: email.to_string(),
    })
}

fn outcome_response<T: serde::Serialize>(
    outcome: Result<T, TeamActionError>,
) -> Response<std::io::Cursor<Vec<u8>>> {
    match outcome {
        Ok(outcome) => match serde_json::to_string(&outcome) {
            Ok(body) => json_response(200, body),
            Err(err) => text_response(500, &err.to_string()),
        },
        Err(TeamActionError::Invalid(reason)) => text_response(400, &reason),
        Err(TeamActionError::NotAllowed(reason)) => text_response(403, &reason),
        Err(TeamActionError::NotLoaded) => text_response(404, "Not open"),
        Err(TeamActionError::NothingToDo(reason)) => text_response(409, &reason),
        Err(TeamActionError::Failed(reason)) => text_response(500, &reason),
    }
}

/// The workspace's root when teammates may read its files (and edit them,
/// when `for_edit`), else the response that refuses.
fn shared_root(
    shared: &Shared,
    workspace_id: &str,
    for_edit: bool,
) -> Result<PathBuf, Response<std::io::Cursor<Vec<u8>>>> {
    let Some(workspace) = shared
        .hub
        .workspaces()
        .into_iter()
        .find(|workspace| workspace.id == workspace_id)
    else {
        return Err(text_response(404, "No such workspace"));
    };
    let root = workspace
        .share_files
        .then(|| (shared.files)(workspace_id))
        .flatten();
    let Some(root) = root else {
        return Err(text_response(403, "Sharing files is off for this workspace"));
    };
    if for_edit && !workspace.edit_files {
        return Err(text_response(403, "Editing files is off for this workspace"));
    }
    Ok(root)
}

#[allow(clippy::disallowed_methods)]
fn is_git_tracked(root: &std::path::Path, path: &str) -> bool {
    std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch", "--", path])
        .env("GIT_LITERAL_PATHSPECS", "1")
        .current_dir(root)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn serve_file(
    shared: &Shared,
    workspace_id: &str,
    query: &str,
) -> Result<Response<std::fs::File>, Response<std::io::Cursor<Vec<u8>>>> {
    let root = shared_root(shared, workspace_id, false)?;
    let path = query_value(query, "path")
        .and_then(percent_decode)
        .ok_or_else(|| text_response(400, "Missing path"))?;
    let resolved =
        safe_relative_path(&root, &path).map_err(|reason| text_response(400, &reason))?;
    let not_found = || text_response(404, "No such file");
    if (shared.skip_list)(workspace_id).skips(&path) {
        return Err(not_found());
    }
    let in_manifest = shared
        .hub
        .file_manifest(workspace_id)
        .is_some_and(|manifest| {
            manifest
                .entries
                .iter()
                .any(|entry| entry.path == path && !matches!(entry.kind, TeamFileKind::Deleted))
        });
    if !in_manifest && !is_git_tracked(&root, &path) {
        return Err(not_found());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&resolved).map_err(|_| not_found())?;
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(not_found());
    }
    let mut response = Response::from_file(file);
    if let Some(header) = header("Content-Type", "application/octet-stream") {
        response.add_header(header);
    }
    Ok(response)
}

fn serve_bundle(
    shared: &Shared,
    workspace_id: &str,
    query: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let have: Vec<String> = query_value(query, "have")
        .unwrap_or_default()
        .split(',')
        .filter(|commit| !commit.is_empty())
        .map(str::to_string)
        .collect();
    let is_commit = |commit: &String| {
        (7..=64).contains(&commit.len()) && commit.chars().all(|c| c.is_ascii_hexdigit())
    };
    if have.len() > MAX_BUNDLE_HAVE || !have.iter().all(is_commit) {
        return text_response(400, "Unexpected commits");
    }
    match (shared.bundle_source)(workspace_id, &have) {
        Ok(bytes) => {
            let mut response = Response::from_data(bytes);
            if let Some(header) = header("Content-Type", "application/octet-stream") {
                response.add_header(header);
            }
            response
        }
        Err(reason) => text_response(500, &reason),
    }
}

fn handle_changes(
    request: &mut Request,
    shared: &Shared,
    caller: &TeamCaller,
    workspace_id: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    if let Err(refusal) = shared_root(shared, workspace_id, true) {
        return refusal;
    }
    if request
        .body_length()
        .is_some_and(|length| length > MAX_BATCH_BODY_BYTES)
    {
        return text_response(413, "The request is too long");
    }
    let mut body = Vec::new();
    let read = request
        .as_reader()
        .take(MAX_BATCH_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body);
    if let Err(err) = read {
        return text_response(400, &format!("Couldn't read the request: {err}"));
    }
    if body.len() > MAX_BATCH_BODY_BYTES {
        return text_response(413, "The request is too long");
    }
    let batch = match serde_json::from_slice(&body) {
        Ok(batch) => batch,
        Err(err) => return text_response(400, &format!("Couldn't read the request: {err}")),
    };
    outcome_response((shared.batch_handler)(TeamBatchRequest {
        workspace_id: workspace_id.to_string(),
        from: caller.name.clone(),
        login: caller.login.clone(),
        batch,
    }))
}

fn parse_action(action: &str, body: &[u8]) -> Result<TeamActionKind, String> {
    #[derive(serde::Deserialize)]
    struct PermissionBody {
        tool_call_id: String,
        option_id: String,
    }
    let invalid = |err: serde_json::Error| format!("Couldn't read the request: {err}");
    match action {
        "permission" => {
            let answer: PermissionBody = serde_json::from_slice(body).map_err(invalid)?;
            Ok(TeamActionKind::Permission {
                tool_call_id: answer.tool_call_id,
                option_id: answer.option_id,
            })
        }
        "stop" => Ok(TeamActionKind::Stop),
        _ => Err(format!("Unknown action {action}")),
    }
}

fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value)
}

fn truncate_diff(mut diff: String) -> String {
    if diff.len() <= MAX_DIFF_BYTES {
        return diff;
    }
    let mut cut = MAX_DIFF_BYTES;
    while !diff.is_char_boundary(cut) {
        cut -= 1;
    }
    diff.truncate(cut);
    diff.push_str("\n\n[Diff cut off: it's over 2 MB]\n");
    diff
}

fn header(name: &str, value: &str) -> Option<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
}

fn json_response(status: u16, body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_string(body).with_status_code(status);
    if let Some(header) = header("Content-Type", "application/json") {
        response.add_header(header);
    }
    response
}

fn text_response(status: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_string(body).with_status_code(status);
    if let Some(header) = header("Content-Type", "text/plain; charset=utf-8") {
        response.add_header(header);
    }
    response
}

fn respond<R: std::io::Read>(request: Request, response: Response<R>) {
    if let Err(err) = request.respond(response) {
        log::debug!(target: "mnemonic_lib", "A team request ended early: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::team_agent_tools::AGENT_MCP_SERVER_NAME;
    use agent_client_protocol::schema as acp;
    use crate::services::team_share_service::{
        TeamActionOutcome, TeamCaller, TeamClient, TeamClientError, TeamEntry, TeamWorkspace,
    };
    use crate::services::team_files_service::{
        TeamChangeBatch, TeamFileEntry, TeamFileManifest, file_hash,
    };
    use std::collections::HashSet;
    use std::io::{BufRead as _, Write as _};
    use std::net::{Ipv4Addr, TcpStream};
    use std::sync::Mutex;

    struct FixedLogin(Option<&'static str>);

    impl CallerIdentity for FixedLogin {
        fn caller(&self, _address: IpAddr) -> Result<Option<TeamCaller>, String> {
            Ok(self.0.map(|login| TeamCaller {
                login: login.to_string(),
                name: "Bob".to_string(),
            }))
        }
    }

    type Recorded = Arc<Mutex<Vec<TeamAction>>>;

    fn start(login: Option<&'static str>, hub: TeamShareHub) -> Result<TeamShareServer, String> {
        Ok(start_with_handler(login, hub, accepted())?.0)
    }

    fn start_with_handler(
        login: Option<&'static str>,
        hub: TeamShareHub,
        result: Result<TeamActionOutcome, TeamActionError>,
    ) -> Result<(TeamShareServer, Recorded), String> {
        let recorded = Recorded::default();
        let action_handler: ActionHandler = Arc::new({
            let recorded = recorded.clone();
            move |action| {
                recorded
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(action);
                result.clone()
            }
        });
        let server = TeamShareServer::start(
            TeamServerConfig {
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                display_name: "Alice".to_string(),
                allowed_logins: vec!["Bob@Example.com".to_string()],
                files: Arc::new(|_| None),
                bundle_source: Arc::new(|_, _| Err("No bundles".to_string())),
                skip_list: Arc::new(|_| SkipList::default()),
                batch_handler: Arc::new(|_| Err(TeamActionError::Failed("Unused".to_string()))),
                agent_roots: Arc::new(|_| None),
                agent_write_check: Arc::new(|_, _| Ok(())),
            },
            hub,
            Arc::new(FixedLogin(login)),
            Arc::new(|workspace_id: &str| Ok(format!("diff of {workspace_id}"))),
            action_handler,
        )?;
        Ok((server, recorded))
    }

    fn accepted() -> Result<TeamActionOutcome, TeamActionError> {
        Ok(TeamActionOutcome {})
    }

    fn step_in_hub(on: bool) -> TeamShareHub {
        let hub = TeamShareHub::new();
        hub.replace_chat("chat-1", Vec::new());
        if on {
            hub.set_step_in_chats(HashSet::from(["chat-1".to_string()]));
        }
        hub
    }

    fn refused(status: u16) -> impl Fn(&TeamClientError) -> bool {
        move |err| matches!(err, TeamClientError::Refused { status: got, .. } if *got == status)
    }

    /// Sends `request` exactly as written and returns the status code of
    /// the answer.
    fn raw_status(address: SocketAddr, request: &str) -> Result<u16, String> {
        let mut stream = TcpStream::connect(address).map_err(|err| err.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|err| err.to_string())?;
        stream
            .write_all(request.as_bytes())
            .map_err(|err| err.to_string())?;
        let mut status_line = String::new();
        std::io::BufReader::new(stream)
            .read_line(&mut status_line)
            .map_err(|err| err.to_string())?;
        status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| format!("No status in {status_line:?}"))
    }

    fn post_hello_message(address: SocketAddr, headers: &str) -> String {
        let body = r#"{"text":"hello"}"#;
        format!(
            "POST /v1/chats/chat-1/message HTTP/1.1\r\nHost: {address}\r\n{headers}\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn workspace(id: &str) -> TeamWorkspace {
        TeamWorkspace {
            id: id.to_string(),
            project: "mnemonic".to_string(),
            name: "fix-login".to_string(),
            branch: "alice/fix-login".to_string(),
            pr_number: None,
            pr_state: None,
            changed_files: 0,
            additions: 0,
            deletions: 0,
            agent_state: Default::default(),
            idle_seconds: None,
            chats: Vec::new(),
            step_in: false,
            share_files: false,
            edit_files: false,
            remote_url: None,
            base_branch: None,
            skip_patterns: Vec::new(),
            agents: false,
            root_path: None,
        }
    }

    fn start_sharing_files(
        root: PathBuf,
        share_files: bool,
        edit_files: bool,
        hub: TeamShareHub,
    ) -> Result<TeamShareServer, String> {
        hub.set_workspaces(vec![TeamWorkspace {
            share_files,
            edit_files,
            ..workspace("ws-1")
        }]);
        TeamShareServer::start(
            TeamServerConfig {
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                display_name: "Alice".to_string(),
                allowed_logins: vec!["bob@example.com".to_string()],
                files: Arc::new(move |_| Some(root.clone())),
                bundle_source: Arc::new(|_, _| Ok(b"bundle".to_vec())),
                skip_list: Arc::new(|_| SkipList::default()),
                batch_handler: Arc::new(|_| Err(TeamActionError::Failed("Unused".to_string()))),
                agent_roots: Arc::new(|_| None),
                agent_write_check: Arc::new(|_, _| Ok(())),
            },
            hub,
            Arc::new(FixedLogin(Some("bob@example.com"))),
            Arc::new(|_: &str| Ok(String::new())),
            Arc::new(|_| Ok(TeamActionOutcome {})),
        )
    }

    fn start_agents(
        root: PathBuf,
        agents: bool,
        login: &'static str,
        write_check: AgentWriteCheck,
    ) -> Result<TeamShareServer, String> {
        let hub = TeamShareHub::new();
        hub.set_workspaces(vec![TeamWorkspace {
            agents,
            root_path: agents.then(|| root.to_string_lossy().into_owned()),
            ..workspace("ws-1")
        }]);
        TeamShareServer::start(
            TeamServerConfig {
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                display_name: "Alice".to_string(),
                allowed_logins: vec!["bob@example.com".to_string()],
                files: Arc::new(|_| None),
                bundle_source: Arc::new(|_, _| Err("No bundles".to_string())),
                skip_list: Arc::new(|_| SkipList::default()),
                batch_handler: Arc::new(|_| Err(TeamActionError::Failed("Unused".to_string()))),
                agent_roots: Arc::new(move |_| Some(root.clone())),
                agent_write_check: write_check,
            },
            hub,
            Arc::new(FixedLogin(Some(login))),
            Arc::new(|_: &str| Ok(String::new())),
            Arc::new(|_| Ok(TeamActionOutcome {})),
        )
    }

    /// Posts `body` to the agent endpoint and returns the status and the
    /// whole answer, headers included.
    fn raw_agent_post(
        address: SocketAddr,
        extra_headers: &str,
        body: &str,
    ) -> Result<(u16, String), String> {
        let request = format!(
            "POST /v1/workspaces/ws-1/agent HTTP/1.1\r\nHost: {address}\r\n\
             Content-Type: application/json\r\n{extra_headers}\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut stream = TcpStream::connect(address).map_err(|err| err.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .map_err(|err| err.to_string())?;
        stream
            .write_all(request.as_bytes())
            .map_err(|err| err.to_string())?;
        let mut answer = String::new();
        stream
            .read_to_string(&mut answer)
            .map_err(|err| err.to_string())?;
        let status = answer
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| format!("No status in {answer:?}"))?;
        Ok((status, answer))
    }

    const TEAM_HEADER: &str = "X-Mnemonic-Team: 5\r\n";

    fn call_body(id: u32, name: &str, arguments: serde_json::Value) -> String {
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}).to_string()
    }

    #[test]
    fn agent_endpoint_answers_mcp_when_agents_are_on() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|err| err.to_string())?;
        let server = start_agents(
            root.path().to_path_buf(),
            true,
            "bob@example.com",
            Arc::new(|_, _| Ok(())),
        )?;
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#;

        let (status, answer) = raw_agent_post(server.address(), TEAM_HEADER, body)?;

        assert_eq!(status, 200);
        assert!(answer.contains(AGENT_MCP_SERVER_NAME), "{answer}");
        assert!(
            answer.to_lowercase().contains("mcp-session-id:"),
            "{answer}"
        );
        Ok(())
    }

    #[test]
    fn agent_endpoint_refuses_when_agents_are_off() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|err| err.to_string())?;
        let server = start_agents(
            root.path().to_path_buf(),
            false,
            "bob@example.com",
            Arc::new(|_, _| Ok(())),
        )?;

        let (status, _) = raw_agent_post(
            server.address(),
            TEAM_HEADER,
            &call_body(1, "Bash", serde_json::json!({"command": "echo hi"})),
        )?;

        assert_eq!(status, 403);
        Ok(())
    }

    #[test]
    fn agent_endpoint_refuses_browser_requests() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|err| err.to_string())?;
        let server = start_agents(
            root.path().to_path_buf(),
            true,
            "bob@example.com",
            Arc::new(|_, _| Ok(())),
        )?;
        let body = call_body(1, "Bash", serde_json::json!({"command": "echo hi"}));

        let (with_origin, _) = raw_agent_post(
            server.address(),
            &format!("{TEAM_HEADER}Origin: https://evil.example\r\n"),
            &body,
        )?;
        let (without_header, _) = raw_agent_post(server.address(), "", &body)?;

        assert_eq!(with_origin, 403);
        assert_eq!(without_header, 400);
        Ok(())
    }

    #[test]
    fn agent_endpoint_refuses_callers_off_the_allow_list() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|err| err.to_string())?;
        let server = start_agents(
            root.path().to_path_buf(),
            true,
            "stranger",
            Arc::new(|_, _| Ok(())),
        )?;

        let (status, _) = raw_agent_post(
            server.address(),
            TEAM_HEADER,
            &call_body(1, "Bash", serde_json::json!({"command": "echo hi"})),
        )?;

        assert_eq!(status, 403);
        Ok(())
    }

    #[test]
    fn agent_writes_ask_the_write_check() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|err| err.to_string())?;
        let server = start_agents(
            root.path().to_path_buf(),
            true,
            "bob@example.com",
            Arc::new(|_, _| Err("unsaved".to_string())),
        )?;
        let target = root.path().join("new.txt");
        let body = call_body(
            1,
            "Write",
            serde_json::json!({"file_path": target.to_string_lossy(), "content": "x"}),
        );

        let (status, answer) = raw_agent_post(server.address(), TEAM_HEADER, &body)?;

        assert_eq!(status, 200);
        assert!(answer.contains(r#""isError":true"#), "{answer}");
        assert!(answer.contains("unsaved"), "{answer}");
        assert!(!target.exists());
        Ok(())
    }

    #[test]
    fn agent_get_is_405_and_commands_of_two_sessions_do_not_cancel_each_other() -> Result<(), String>
    {
        let root = tempfile::tempdir().map_err(|err| err.to_string())?;
        let server = start_agents(
            root.path().to_path_buf(),
            true,
            "bob@example.com",
            Arc::new(|_, _| Ok(())),
        )?;
        let address = server.address();
        let get = format!(
            "GET /v1/workspaces/ws-1/agent HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
        );
        assert_eq!(raw_status(address, &get)?, 405);

        let running = std::thread::spawn(move || {
            raw_agent_post(
                address,
                &format!("{TEAM_HEADER}Mcp-Session-Id: one\r\n"),
                &call_body(
                    7,
                    "Bash",
                    serde_json::json!({"command": "sleep 2; echo finished"}),
                ),
            )
        });
        std::thread::sleep(Duration::from_millis(500));
        let cancel =
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}"#;
        let (status, _) = raw_agent_post(
            address,
            &format!("{TEAM_HEADER}Mcp-Session-Id: two\r\n"),
            cancel,
        )?;
        assert_eq!(status, 202);

        let (_, answer) = running
            .join()
            .map_err(|_| "thread panicked".to_string())??;
        assert!(answer.contains(r#""isError":false"#), "{answer}");
        assert!(answer.contains("finished"), "{answer}");
        Ok(())
    }

    #[test]
    fn agent_read_maps_a_missing_file_to_none() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|err| err.to_string())?;
        std::fs::write(root.path().join("a.txt"), "hello\n").map_err(|err| err.to_string())?;
        let server = start_agents(
            root.path().to_path_buf(),
            true,
            "bob@example.com",
            Arc::new(|_, _| Ok(())),
        )?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;
        let present = root.path().join("a.txt");
        let missing = root.path().join("missing.txt");

        let text = client
            .agent_read("ws-1", &present.to_string_lossy())
            .map_err(|err| err.to_string())?
            .ok_or("file should exist")?;
        assert!(text.contains("hello"));
        assert_eq!(
            client.agent_read("ws-1", &missing.to_string_lossy()),
            Ok(None)
        );
        Ok(())
    }

    #[test]
    fn git_author_header_is_parsed() {
        let author = parse_git_author("Bob Smith <bob@example.com>").expect("author");
        assert_eq!(
            (author.name.as_str(), author.email.as_str()),
            ("Bob Smith", "bob@example.com")
        );
        assert!(parse_git_author("nobody").is_none());
    }

    fn manifest_of(path: &str, bytes: &[u8]) -> TeamFileManifest {
        TeamFileManifest {
            head_commit: "abc1234".to_string(),
            entries: vec![TeamFileEntry {
                path: path.to_string(),
                kind: TeamFileKind::File { executable: false },
                size: bytes.len() as u64,
                hash: file_hash(bytes),
            }],
        }
    }

    #[test]
    fn file_routes_are_refused_when_share_files_is_off() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|err| err.to_string())?;
        let server = start_sharing_files(
            root.path().to_path_buf(),
            false,
            false,
            TeamShareHub::new(),
        )?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;

        assert!(
            client
                .files("ws-1", None, Duration::ZERO)
                .is_err_and(|err| err == TeamClientError::Forbidden)
        );
        assert!(
            client
                .file("ws-1", "a.txt")
                .is_err_and(|err| refused(403)(&err))
        );
        assert!(
            client
                .bundle("ws-1", &[])
                .is_err_and(|err| refused(403)(&err))
        );
        Ok(())
    }

    #[test]
    fn a_teammate_reads_the_manifest_and_a_file() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|err| err.to_string())?;
        std::fs::write(root.path().join("a.txt"), b"hello").map_err(|err| err.to_string())?;
        let hub = TeamShareHub::new();
        let manifest = manifest_of("a.txt", b"hello");
        hub.set_file_manifest("ws-1", manifest.clone());
        let server = start_sharing_files(root.path().to_path_buf(), true, false, hub.clone())?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;

        let update = client
            .files("ws-1", None, Duration::ZERO)
            .map_err(|err| err.to_string())?
            .ok_or("manifest should be served")?;
        assert_eq!(update.manifest, Some(manifest));
        assert_eq!(client.file("ws-1", "a.txt"), Ok(b"hello".to_vec()));
        assert_eq!(client.bundle("ws-1", &[]), Ok(b"bundle".to_vec()));

        let waiting = std::thread::spawn({
            let client = client.clone();
            let cursor = update.cursor.clone();
            move || client.files("ws-1", Some(&cursor), Duration::from_secs(10))
        });
        std::thread::sleep(Duration::from_millis(200));
        let newer = manifest_of("b.txt", b"x");
        hub.set_file_manifest("ws-1", newer.clone());
        let next = waiting
            .join()
            .map_err(|_| "poller panicked")?
            .map_err(|err| err.to_string())?
            .ok_or("manifest should be served")?;
        assert_eq!(next.manifest, Some(newer));
        assert_ne!(next.cursor, update.cursor);

        let unchanged = client
            .files("ws-1", Some(&next.cursor), Duration::ZERO)
            .map_err(|err| err.to_string())?
            .ok_or("manifest should be served")?;
        assert_eq!(unchanged.manifest, None);
        assert_eq!(unchanged.cursor, next.cursor);
        Ok(())
    }

    #[test]
    fn a_file_outside_the_manifest_or_on_the_skip_list_is_not_served() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|err| err.to_string())?;
        std::fs::write(root.path().join(".env"), b"SECRET=1").map_err(|err| err.to_string())?;
        std::fs::write(root.path().join("unlisted.txt"), b"x").map_err(|err| err.to_string())?;
        std::fs::create_dir(root.path().join(".claude")).map_err(|err| err.to_string())?;
        std::fs::write(root.path().join(".claude/settings.local.json"), b"{}")
            .map_err(|err| err.to_string())?;
        let hub = TeamShareHub::new();
        hub.set_file_manifest("ws-1", manifest_of(".env", b"SECRET=1"));
        let server = start_sharing_files(root.path().to_path_buf(), true, false, hub)?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;

        assert!(client.file("ws-1", ".env").is_err_and(|err| refused(404)(&err)));
        assert!(client
            .file("ws-1", "unlisted.txt")
            .is_err_and(|err| refused(404)(&err)));
        assert!(client
            .file("ws-1", "../x")
            .is_err_and(|err| refused(400)(&err)));
        assert!(client
            .file("ws-1", ".claude/./settings.local.json")
            .is_err_and(|err| refused(400)(&err)));
        Ok(())
    }

    #[test]
    fn changes_are_refused_when_edit_files_is_off() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|err| err.to_string())?;
        let server = start_sharing_files(
            root.path().to_path_buf(),
            true,
            false,
            TeamShareHub::new(),
        )?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;
        let batch = TeamChangeBatch {
            note: None,
            changes: Vec::new(),
        };

        assert!(
            client
                .send_changes("ws-1", &batch)
                .is_err_and(|err| refused(403)(&err))
        );
        Ok(())
    }

    #[test]
    fn an_allowed_teammate_reads_workspaces_diffs_and_live_chat_changes() -> Result<(), String> {
        let hub = TeamShareHub::new();
        hub.set_workspaces(vec![workspace("ws-1")]);
        hub.replace_chat(
            "chat-1",
            vec![TeamEntry::Updates {
                updates: vec![acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(
                    "Fix the login page".into(),
                ))],
            }],
        );
        let server = start(Some("bob@example.com"), hub.clone())?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;

        assert_eq!(
            client.hello().map(|hello| hello.name),
            Ok("Alice".to_string())
        );
        assert_eq!(client.workspaces().map(|list| list.len()), Ok(1));
        assert_eq!(client.diff("ws-1"), Ok(Some("diff of ws-1".to_string())));
        assert_eq!(client.diff("ws-unshared"), Ok(None));

        let first = client
            .chat("chat-1", None, Duration::ZERO)
            .map_err(|err| err.to_string())?
            .ok_or("chat should be open")?;
        assert_eq!(first.len, 1);

        let waiting = std::thread::spawn({
            let client = client.clone();
            let cursor = first.cursor.clone();
            move || client.chat("chat-1", Some(&cursor), Duration::from_secs(10))
        });
        std::thread::sleep(Duration::from_millis(200));
        hub.set_chat_entry(
            "chat-1",
            1,
            TeamEntry::Updates {
                updates: vec![acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    "On it".into(),
                ))],
            },
        );
        let update = waiting
            .join()
            .map_err(|_| "poller panicked")?
            .map_err(|err| err.to_string())?
            .ok_or("chat should be open")?;
        assert!(!update.reset);
        assert_eq!(update.changes.len(), 1);
        assert_eq!(update.changes[0].index, 1);

        assert_eq!(client.chat("chat-closed", None, Duration::ZERO), Ok(None));
        Ok(())
    }

    #[test]
    fn a_login_not_on_the_allow_list_is_refused() -> Result<(), String> {
        let hub = TeamShareHub::new();
        hub.set_workspaces(vec![workspace("ws-1")]);
        let server = start(Some("mallory@example.com"), hub.clone())?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;
        assert_eq!(client.workspaces(), Err(TeamClientError::Forbidden));
        assert_eq!(client.diff("ws-1"), Err(TeamClientError::Forbidden));
        assert!(!hub.has_viewers_within(Duration::from_secs(60)));
        Ok(())
    }

    #[test]
    fn an_allowed_teammate_can_stop_and_answer_when_step_in_is_on() -> Result<(), String> {
        let (server, recorded) =
            start_with_handler(Some("bob@example.com"), step_in_hub(true), accepted())?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;

        assert_eq!(client.stop("chat-1"), Ok(()));
        assert_eq!(client.answer_permission("chat-1", "call-1", "allow"), Ok(()));

        let kinds: Vec<TeamActionKind> = recorded
            .lock()
            .map_err(|err| err.to_string())?
            .iter()
            .inspect(|action| {
                assert_eq!(action.chat_id, "chat-1");
                assert_eq!(action.from, "Bob");
            })
            .map(|action| action.kind.clone())
            .collect();
        assert_eq!(
            kinds,
            vec![
                TeamActionKind::Stop,
                TeamActionKind::Permission {
                    tool_call_id: "call-1".to_string(),
                    option_id: "allow".to_string()
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn a_message_to_the_owners_agent_is_not_found() -> Result<(), String> {
        let (server, recorded) =
            start_with_handler(Some("bob@example.com"), step_in_hub(true), accepted())?;
        let address = server.address();

        let request = post_hello_message(
            address,
            "Content-Type: application/json\r\nX-Mnemonic-Team: 3\r\n",
        );
        assert_eq!(raw_status(address, &request)?, 404);
        assert!(recorded.lock().map_err(|err| err.to_string())?.is_empty());
        Ok(())
    }

    #[test]
    fn actions_are_refused_when_step_in_is_off() -> Result<(), String> {
        let (server, recorded) =
            start_with_handler(Some("bob@example.com"), step_in_hub(false), accepted())?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;

        assert!(client.stop("chat-1").is_err_and(|err| refused(403)(&err)));
        assert!(
            client
                .answer_permission("chat-1", "call-1", "allow")
                .is_err_and(|err| refused(403)(&err))
        );
        assert!(recorded.lock().map_err(|err| err.to_string())?.is_empty());
        Ok(())
    }

    #[test]
    fn actions_are_refused_for_a_login_not_on_the_allow_list() -> Result<(), String> {
        let (server, recorded) =
            start_with_handler(Some("mallory@example.com"), step_in_hub(true), accepted())?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;

        assert!(client.stop("chat-1").is_err_and(|err| refused(403)(&err)));
        assert!(recorded.lock().map_err(|err| err.to_string())?.is_empty());
        Ok(())
    }

    #[test]
    fn a_browser_style_post_is_refused() -> Result<(), String> {
        let (server, recorded) =
            start_with_handler(Some("bob@example.com"), step_in_hub(true), accepted())?;
        let address = server.address();

        let plain_text = post_hello_message(address, "Content-Type: text/plain\r\n");
        assert_eq!(raw_status(address, &plain_text)?, 400);
        let without_team_header =
            post_hello_message(address, "Content-Type: application/json\r\n");
        assert_eq!(raw_status(address, &without_team_header)?, 400);
        let from_a_web_page = post_hello_message(
            address,
            "Content-Type: application/json\r\nX-Mnemonic-Team: 3\r\n\
             Origin: https://example.com\r\n",
        );
        assert_eq!(raw_status(address, &from_a_web_page)?, 403);
        assert!(recorded.lock().map_err(|err| err.to_string())?.is_empty());
        Ok(())
    }

    #[test]
    fn a_request_for_another_host_name_is_refused() -> Result<(), String> {
        let server = start(Some("bob@example.com"), TeamShareHub::new())?;
        let address = server.address();
        let list_workspaces = |host: &str| {
            format!("GET /v1/workspaces HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
        };

        assert_eq!(
            raw_status(address, &list_workspaces("attacker.example.com"))?,
            403
        );
        assert_eq!(
            raw_status(address, &list_workspaces("jays-mac.tail1234.ts.net:47231"))?,
            200
        );
        Ok(())
    }

    #[test]
    fn a_handler_error_reaches_the_client_with_its_reason() -> Result<(), String> {
        let (server, _recorded) = start_with_handler(
            Some("bob@example.com"),
            step_in_hub(true),
            Err(TeamActionError::NothingToDo("The agent isn't running".into())),
        )?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;

        assert_eq!(
            client.stop("chat-1"),
            Err(TeamClientError::Refused {
                status: 409,
                reason: "The agent isn't running".to_string()
            })
        );
        Ok(())
    }
}
