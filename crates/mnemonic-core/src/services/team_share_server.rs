//! The HTTP side of team sharing. Reads are GETs. The only writes are three
//! POSTs on a chat (stop, message, permission), each handed to the app's
//! action handler and only for chats the sharer has opened to step-in.

use std::io::Read as _;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tiny_http::{Header, Method, Request, Response, Server};

use crate::services::team_share_service::{
    ActionHandler, CallerIdentity, MAX_CHAT_WAIT, TEAM_ACP_CRATE_VERSION, TEAM_PROTOCOL_VERSION,
    TEAM_REQUEST_HEADER, TeamAction, TeamActionError, TeamActionKind, TeamCaller, TeamHello,
    TeamShareHub, TeamWorkspaceList, is_plain_id,
};

/// Produces a workspace's diff. Behind a closure so tests don't need a
/// database or a git repository.
pub type DiffSource = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

pub struct TeamServerConfig {
    pub bind: IpAddr,
    pub port: u16,
    pub display_name: String,
    /// Tailscale logins allowed to read, compared case-insensitively.
    pub allowed_logins: Vec<String>,
}

/// Long-polls hold a thread each, so this caps what a misbehaving caller
/// can tie up.
const MAX_ACTIVE_REQUESTS: usize = 64;
/// Diffs past this are cut, so a vendored directory can't stall a viewer.
const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;
const MAX_MESSAGE_BYTES: usize = 256 * 1024;

struct Shared {
    hub: TeamShareHub,
    identity: Arc<dyn CallerIdentity>,
    diff_source: DiffSource,
    action_handler: ActionHandler,
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
            ["v1", "chats", chat_id, action @ ("stop" | "message" | "permission")]
                if is_plain_id(chat_id) =>
            {
                handle_action(&mut request, shared, &caller, chat_id, action)
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
        .take(MAX_MESSAGE_BYTES as u64 + 1)
        .read_to_end(&mut body);
    if let Err(err) = read {
        return text_response(400, &format!("Couldn't read the request: {err}"));
    }
    if body.len() > MAX_MESSAGE_BYTES {
        return text_response(413, "The message is too long");
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
    match outcome {
        Ok(outcome) => match serde_json::to_string(&outcome) {
            Ok(body) => json_response(200, body),
            Err(err) => text_response(500, &err.to_string()),
        },
        Err(TeamActionError::NotAllowed(reason)) => text_response(403, &reason),
        Err(TeamActionError::NotLoaded) => text_response(404, "Not open"),
        Err(TeamActionError::NothingToDo(reason)) => text_response(409, &reason),
        Err(TeamActionError::Failed(reason)) => text_response(500, &reason),
    }
}

fn parse_action(action: &str, body: &[u8]) -> Result<TeamActionKind, String> {
    #[derive(serde::Deserialize)]
    struct MessageBody {
        text: String,
    }
    #[derive(serde::Deserialize)]
    struct PermissionBody {
        tool_call_id: String,
        option_id: String,
    }
    let invalid = |err: serde_json::Error| format!("Couldn't read the request: {err}");
    match action {
        "message" => {
            let message: MessageBody = serde_json::from_slice(body).map_err(invalid)?;
            if message.text.trim().is_empty() {
                return Err("The message is empty".to_string());
            }
            Ok(TeamActionKind::Message { text: message.text })
        }
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

fn respond(request: Request, response: Response<std::io::Cursor<Vec<u8>>>) {
    if let Err(err) = request.respond(response) {
        log::debug!(target: "mnemonic_lib", "A team request ended early: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema as acp;
    use crate::services::team_share_service::{
        TeamActionOutcome, TeamCaller, TeamClient, TeamClientError, TeamEntry, TeamWorkspace,
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
        Ok(start_with_handler(login, hub, queued())?.0)
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
            },
            hub,
            Arc::new(FixedLogin(login)),
            Arc::new(|workspace_id: &str| Ok(format!("diff of {workspace_id}"))),
            action_handler,
        )?;
        Ok((server, recorded))
    }

    fn queued() -> Result<TeamActionOutcome, TeamActionError> {
        Ok(TeamActionOutcome { queued: true })
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
        }
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
    fn an_allowed_teammate_can_stop_message_and_answer_when_step_in_is_on() -> Result<(), String> {
        let (server, recorded) =
            start_with_handler(Some("bob@example.com"), step_in_hub(true), queued())?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;

        assert_eq!(client.stop("chat-1"), Ok(()));
        assert_eq!(client.send_message("chat-1", "hello"), Ok(true));
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
                TeamActionKind::Message {
                    text: "hello".to_string()
                },
                TeamActionKind::Permission {
                    tool_call_id: "call-1".to_string(),
                    option_id: "allow".to_string()
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn actions_are_refused_when_step_in_is_off() -> Result<(), String> {
        let (server, recorded) =
            start_with_handler(Some("bob@example.com"), step_in_hub(false), queued())?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;

        assert!(client.stop("chat-1").is_err_and(|err| refused(403)(&err)));
        assert!(client
            .send_message("chat-1", "hello")
            .is_err_and(|err| refused(403)(&err)));
        assert!(client
            .answer_permission("chat-1", "call-1", "allow")
            .is_err_and(|err| refused(403)(&err)));
        assert!(recorded.lock().map_err(|err| err.to_string())?.is_empty());
        Ok(())
    }

    #[test]
    fn actions_are_refused_for_a_login_not_on_the_allow_list() -> Result<(), String> {
        let (server, recorded) =
            start_with_handler(Some("mallory@example.com"), step_in_hub(true), queued())?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;

        assert!(client.stop("chat-1").is_err_and(|err| refused(403)(&err)));
        assert!(recorded.lock().map_err(|err| err.to_string())?.is_empty());
        Ok(())
    }

    #[test]
    fn an_empty_or_oversized_message_is_refused() -> Result<(), String> {
        let (server, recorded) =
            start_with_handler(Some("bob@example.com"), step_in_hub(true), queued())?;
        let client =
            TeamClient::new(&server.address().to_string(), 0).map_err(|err| err.to_string())?;

        assert!(client
            .send_message("chat-1", "  ")
            .is_err_and(|err| refused(400)(&err)));
        assert!(client
            .send_message("chat-1", &"a".repeat(300 * 1024))
            .is_err_and(|err| refused(413)(&err)));
        assert!(recorded.lock().map_err(|err| err.to_string())?.is_empty());
        Ok(())
    }

    #[test]
    fn a_browser_style_post_is_refused() -> Result<(), String> {
        let (server, recorded) =
            start_with_handler(Some("bob@example.com"), step_in_hub(true), queued())?;
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
