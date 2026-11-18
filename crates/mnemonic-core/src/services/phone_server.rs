//! The HTTP side of the phone page: the owner's phone, over Tailscale, reads
//! every workspace and acts on its chats. Only the Mac's own Tailscale login
//! is let in, never from the Mac itself, and never on behalf of another web
//! page.

use std::io::Read as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::thread::JoinHandle;

use tiny_http::{Header, Method, Request, Response, Server, SslConfig};

use crate::services::team_share_server::{
    DiffSource, accept_requests, chat_update_response, has_json_body, header_value,
    is_allowed_host, json_response, respond, text_response, truncate_diff,
};
use crate::services::team_share_service::{
    CallerIdentity, TeamActionError, TeamShareHub, TeamWorkspaceList, TlsFiles, is_plain_id,
};
use crate::services::web_push_service::{PushDevice, PushSubscription};

/// Sent by the page on every POST. A page on another site can't add it
/// without a CORS preflight, which this server never answers.
pub const PHONE_REQUEST_HEADER: &str = "X-Mnemonic-Phone";

const PAGE: &str = include_str!("phone_page.html");
const SERVICE_WORKER: &str = include_str!("phone_sw.js");
const MANIFEST: &str = include_str!("phone_manifest.json");
const ICON: &[u8] = include_bytes!("../../../zed/resources/app-icon@2x.png");

const MAX_MESSAGE_BYTES: usize = 16 * 1024;
/// Room for a message of `MAX_MESSAGE_BYTES` whose characters JSON escapes.
const MAX_ACTION_BODY_BYTES: usize = MAX_MESSAGE_BYTES * 4;

pub struct PhoneServerConfig {
    pub bind: IpAddr,
    pub port: u16,
    /// The Mac's own Tailscale login, compared case-insensitively.
    pub owner_login: String,
    /// Requests from these are refused, so a program on the Mac (an agent's
    /// shell command included) can't pass as the phone.
    pub own_addresses: Vec<IpAddr>,
    /// Serves HTTPS only when set, plain HTTP otherwise.
    pub tls: Option<TlsFiles>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PhoneAction {
    Stop {
        chat_id: String,
    },
    Permission {
        chat_id: String,
        tool_call_id: String,
        option_id: String,
    },
    Message {
        chat_id: String,
        text: String,
    },
    NewChat {
        workspace_id: String,
        agent: String,
    },
    /// Creates a workspace in the repository and sends `text` to its chat
    /// as the user's first message.
    StartTask {
        repository_id: String,
        agent: String,
        text: String,
    },
    CreatePr {
        workspace_id: String,
    },
    /// Merges the workspace's PR at `head`, the commit the phone saw.
    Merge {
        workspace_id: String,
        head: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct PhoneActionOutcome {
    /// The new chat's id, for `PhoneAction::NewChat` and
    /// `PhoneAction::StartTask`.
    pub chat_id: Option<String>,
    /// The new workspace's id, for `PhoneAction::StartTask`.
    pub workspace_id: Option<String>,
    /// What GitHub said, for `PhoneAction::Merge`, or, for a task or merge
    /// that outlasted its timeout, that it is still going on the Mac.
    pub message: Option<String>,
}

pub type PhoneActionHandler =
    Arc<dyn Fn(PhoneAction) -> Result<PhoneActionOutcome, TeamActionError> + Send + Sync>;

/// An agent the Mac lets new chats use.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PhoneAgent {
    /// What a new chat request names it by.
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PhoneAgentList {
    pub agents: Vec<PhoneAgent>,
}

pub type PhoneAgentSource = Arc<dyn Fn() -> Vec<PhoneAgent> + Send + Sync>;

/// A repository a new workspace can be started in.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PhoneRepository {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PhoneRepositoryList {
    pub repositories: Vec<PhoneRepository>,
}

/// A workspace's pull request as the phone shows it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct PhonePrStatus {
    pub found: bool,
    pub number: Option<i64>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub state: Option<String>,
    pub is_draft: bool,
    pub merge_state_status: Option<String>,
    pub head_ref_oid: Option<String>,
    pub checks_summary: String,
    pub can_merge: bool,
    pub merge_method: String,
}

pub type PhoneRepositorySource =
    Arc<dyn Fn() -> Result<Vec<PhoneRepository>, String> + Send + Sync>;
pub type PhonePrStatusSource = Arc<dyn Fn(&str) -> Result<PhonePrStatus, String> + Send + Sync>;

/// What the Mac reads for the phone beyond the hub's workspaces and chats.
pub struct PhoneSources {
    pub diff: DiffSource,
    pub agents: PhoneAgentSource,
    /// Most recently used first.
    pub repositories: PhoneRepositorySource,
    pub pr_status: PhonePrStatusSource,
    /// The public VAPID key, base64url, that browsers subscribe with.
    pub push_key: Arc<dyn Fn() -> Result<String, String> + Send + Sync>,
    pub save_push_subscription: Arc<dyn Fn(PushSubscription) -> Result<(), String> + Send + Sync>,
    pub save_push_device: Arc<dyn Fn(PushDevice) -> Result<(), String> + Send + Sync>,
    pub send_test_alert: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
}

struct Shared {
    hub: TeamShareHub,
    identity: Arc<dyn CallerIdentity>,
    sources: PhoneSources,
    action_handler: PhoneActionHandler,
    owner_login: String,
    own_addresses: Vec<IpAddr>,
    https: bool,
}

/// A running phone server. Dropping it stops the server.
pub struct PhoneServer {
    server: Arc<Server>,
    address: SocketAddr,
    accept_thread: Option<JoinHandle<()>>,
}

impl PhoneServer {
    pub fn start(
        config: PhoneServerConfig,
        hub: TeamShareHub,
        identity: Arc<dyn CallerIdentity>,
        sources: PhoneSources,
        action_handler: PhoneActionHandler,
    ) -> Result<Self, String> {
        let https = config.tls.is_some();
        let server = match config.tls {
            Some(tls) => Server::https(
                (config.bind, config.port),
                SslConfig {
                    certificate: tls.certificate,
                    private_key: pkcs8_private_key(&tls.private_key)?,
                },
            ),
            None => Server::http((config.bind, config.port)),
        }
        .map_err(|err| format!("Couldn't listen on {}:{}: {err}", config.bind, config.port))?;
        let address = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| "The phone server has no IP address".to_string())?;
        let shared = Arc::new(Shared {
            hub,
            identity,
            sources,
            action_handler,
            owner_login: config.owner_login.trim().to_lowercase(),
            own_addresses: config
                .own_addresses
                .iter()
                .map(IpAddr::to_canonical)
                .collect(),
            https,
        });
        let server = Arc::new(server);
        let accept_thread = std::thread::Builder::new()
            .name("mnemonic-phone".into())
            .spawn({
                let server = server.clone();
                move || accept_requests(&server, &shared, "mnemonic-phone-request", handle)
            })
            .map_err(|err| format!("Couldn't start the phone server: {err}"))?;
        log::info!(target: "mnemonic_lib", "Serving the phone page on {address}");
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

impl Drop for PhoneServer {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(thread) = self.accept_thread.take() {
            if thread.join().is_err() {
                log::error!(target: "mnemonic_lib", "The phone server thread panicked");
            }
        }
        log::info!(target: "mnemonic_lib", "Stopped serving the phone page on {}", self.address);
    }
}

fn handle(mut request: Request, shared: &Shared) {
    if let Err((status, reason)) = check_request(&request, shared.https) {
        respond(request, text_response(status, reason));
        return;
    }
    let Some(address) = request
        .remote_addr()
        .map(|address| address.ip().to_canonical())
    else {
        respond(request, text_response(403, "Unknown caller"));
        return;
    };
    if shared.own_addresses.contains(&address) {
        respond(
            request,
            text_response(403, "Requests from this Mac are refused"),
        );
        return;
    }
    match shared.identity.caller(address) {
        Ok(Some(caller))
            if !shared.owner_login.is_empty()
                && caller.login.trim().to_lowercase() == shared.owner_login => {}
        Ok(Some(caller)) => {
            log::info!(
                target: "mnemonic_lib",
                "Refused a phone request from {} ({address})",
                caller.login
            );
            respond(
                request,
                text_response(403, "Only this Mac's owner may connect"),
            );
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
    }
    shared.hub.note_request();

    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let method = request.method().clone();
    if method == Method::Post {
        let response = handle_action(&mut request, shared, segments.as_slice());
        respond(request, response);
        return;
    }
    if method != Method::Get {
        respond(
            request,
            text_response(405, "Only GET and POST are supported"),
        );
        return;
    }
    let response = match segments.as_slice() {
        [""] => page_response(),
        ["manifest.webmanifest"] => static_response(
            MANIFEST.as_bytes().to_vec(),
            "application/manifest+json",
            "no-cache",
        ),
        ["sw.js"] => static_response(
            SERVICE_WORKER.as_bytes().to_vec(),
            "text/javascript",
            "no-store",
        ),
        ["icon.png"] => static_response(ICON.to_vec(), "image/png", "max-age=86400"),
        ["v1", "agents"] => {
            let list = PhoneAgentList {
                agents: (shared.sources.agents)(),
            };
            match serde_json::to_string(&list) {
                Ok(body) => json_response(200, body),
                Err(err) => text_response(500, &err.to_string()),
            }
        }
        ["v1", "push", "key"] => match (shared.sources.push_key)() {
            Ok(key) => json_response(200, serde_json::json!({ "key": key }).to_string()),
            Err(err) => text_response(500, &err),
        },
        ["v1", "repositories"] => match (shared.sources.repositories)() {
            Ok(repositories) => {
                match serde_json::to_string(&PhoneRepositoryList { repositories }) {
                    Ok(body) => json_response(200, body),
                    Err(err) => text_response(500, &err.to_string()),
                }
            }
            Err(err) => text_response(500, &err),
        },
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
                match (shared.sources.diff)(workspace_id) {
                    Ok(diff) => text_response(200, &truncate_diff(diff)),
                    Err(err) => text_response(500, &err),
                }
            } else {
                text_response(404, "No such workspace")
            }
        }
        ["v1", "workspaces", workspace_id, "pr"] if is_plain_id(workspace_id) => {
            if shared.hub.has_workspace(workspace_id) {
                match (shared.sources.pr_status)(workspace_id) {
                    Ok(status) => match serde_json::to_string(&status) {
                        Ok(body) => json_response(200, body),
                        Err(err) => text_response(500, &err.to_string()),
                    },
                    Err(err) => text_response(500, &err),
                }
            } else {
                text_response(404, "No such workspace")
            }
        }
        ["v1", "chats", chat_id] if is_plain_id(chat_id) => {
            chat_update_response(&shared.hub, chat_id, query)
        }
        _ => text_response(404, "Not found"),
    };
    respond(request, response);
}

/// The frame headers stop another site showing the page in a frame and
/// tricking a tap on its buttons, since the framed page's own requests
/// would pass as the phone's.
const PAGE_HEADERS: [(&str, &str); 4] = [
    ("Content-Type", "text/html; charset=utf-8"),
    ("Cache-Control", "no-store"),
    ("X-Frame-Options", "DENY"),
    (
        "Content-Security-Policy",
        "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
         connect-src 'self'; img-src 'self'; manifest-src 'self'; \
         worker-src 'self'; frame-ancestors 'none'",
    ),
];

fn page_response() -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_string(PAGE);
    for (name, value) in PAGE_HEADERS {
        match Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            Ok(header) => response.add_header(header),
            Err(()) => {
                log::error!(target: "mnemonic_lib", "The phone page's {name} header is invalid");
                return text_response(500, "The page couldn't be served");
            }
        }
    }
    response
}

fn static_response(
    body: Vec<u8>,
    content_type: &str,
    cache_control: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_data(body);
    for (name, value) in [
        ("Content-Type", content_type),
        ("Cache-Control", cache_control),
    ] {
        match Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            Ok(header) => response.add_header(header),
            Err(()) => return text_response(500, "The file couldn't be served"),
        }
    }
    response
}

/// The page is served by this server, so its requests are same-origin.
/// These refuse what another page can send: a foreign `Host` means DNS
/// rebinding, a foreign `Origin` means another site, and a POST must carry
/// a JSON body and the phone header, which make a browser ask first with a
/// CORS preflight that this server never answers.
fn check_request(request: &Request, https: bool) -> Result<(), (u16, &'static str)> {
    let Some(host) = header_value(request, "Host").filter(|host| is_allowed_host(host)) else {
        return Err((403, "Unexpected host name"));
    };
    let is_post = *request.method() == Method::Post;
    match header_value(request, "Origin") {
        Some(origin) if !is_same_origin(origin, host, https) => {
            return Err((403, "Requests from other web pages are refused"));
        }
        None if is_post => return Err((403, "Requests from other web pages are refused")),
        _ => {}
    }
    if is_post {
        if !has_json_body(request) {
            return Err((400, "Expected a JSON body"));
        }
        if header_value(request, PHONE_REQUEST_HEADER).is_none_or(|value| value.trim().is_empty()) {
            return Err((400, "Not a request from the phone page"));
        }
    }
    Ok(())
}

/// The 'id-ecPublicKey with prime256v1' algorithm, DER encoded: the only
/// kind of key `tailscale cert` makes.
const P256_KEY_ALGORITHM: &[u8] = &[
    0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48,
    0xce, 0x3d, 0x03, 0x01, 0x07,
];

/// The private key as a PKCS#8 PEM block. tiny_http's TLS reads only PKCS#8
/// and RSA keys, and panics on anything else, while `tailscale cert` writes
/// a SEC1 "EC PRIVATE KEY". Re-encoding it here also means tiny_http never
/// sees a PEM block it could fail to parse.
fn pkcs8_private_key(pem: &[u8]) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let unreadable = || "The HTTPS private key couldn't be read".to_string();
    let text = std::str::from_utf8(pem).map_err(|_| unreadable())?;
    let (_, block) = text.split_once("-----BEGIN ").ok_or_else(unreadable)?;
    let (label, block) = block.split_once("-----").ok_or_else(unreadable)?;
    let (body, _) = block
        .split_once(&format!("-----END {label}-----"))
        .ok_or_else(unreadable)?;
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.split_whitespace().collect::<String>())
        .map_err(|_| unreadable())?;
    let pkcs8 = match label {
        "PRIVATE KEY" => der,
        "EC PRIVATE KEY" => {
            let mut info = vec![0x02, 0x01, 0x00];
            info.extend_from_slice(P256_KEY_ALGORITHM);
            info.extend(der_element(0x04, &der)?);
            der_element(0x30, &info)?
        }
        _ => return Err(format!("The HTTPS private key is an unsupported {label}")),
    };
    Ok(format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        base64::engine::general_purpose::STANDARD.encode(pkcs8)
    )
    .into_bytes())
}

fn der_element(tag: u8, contents: &[u8]) -> Result<Vec<u8>, String> {
    let mut element = vec![tag];
    match u8::try_from(contents.len()) {
        Ok(length) if length < 0x80 => element.push(length),
        Ok(length) => element.extend_from_slice(&[0x81, length]),
        Err(_) => {
            let length = u16::try_from(contents.len())
                .map_err(|_| "The HTTPS private key is too long".to_string())?;
            element.push(0x82);
            element.extend_from_slice(&length.to_be_bytes());
        }
    }
    element.extend_from_slice(contents);
    Ok(element)
}

fn is_same_origin(origin: &str, host: &str, https: bool) -> bool {
    let scheme = if https { "https" } else { "http" };
    origin == format!("{scheme}://{host}")
}

fn handle_action(
    request: &mut Request,
    shared: &Shared,
    segments: &[&str],
) -> Response<std::io::Cursor<Vec<u8>>> {
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
    if let ["v1", "push", "subscriptions"] = segments {
        return match parse_push_subscription(&body) {
            Ok(subscription) => match (shared.sources.save_push_subscription)(subscription) {
                Ok(()) => json_response(200, "{}".to_string()),
                Err(err) => text_response(500, &err),
            },
            Err(reason) => text_response(400, &reason),
        };
    }
    if let ["v1", "push", "devices"] = segments {
        return match parse_push_device(&body) {
            Ok(device) => match (shared.sources.save_push_device)(device) {
                Ok(()) => json_response(200, "{}".to_string()),
                Err(err) => text_response(500, &err),
            },
            Err(reason) => text_response(400, &reason),
        };
    }
    if let ["v1", "push", "test"] = segments {
        return match (shared.sources.send_test_alert)() {
            Ok(()) => json_response(200, "{}".to_string()),
            Err(err) => text_response(500, &err),
        };
    }
    let action = match parse_action(segments, &body) {
        Ok(action) => action,
        Err((status, reason)) => return text_response(status, &reason),
    };
    match (shared.action_handler)(action) {
        Ok(outcome) => match serde_json::to_string(&outcome) {
            Ok(body) => json_response(200, body),
            Err(err) => text_response(500, &err.to_string()),
        },
        Err(TeamActionError::Invalid(reason)) => text_response(400, &reason),
        Err(TeamActionError::NotAllowed(reason)) => text_response(403, &reason),
        Err(TeamActionError::NotLoaded) => {
            text_response(409, "Open this workspace on the Mac first")
        }
        Err(TeamActionError::NothingToDo(reason)) => text_response(409, &reason),
        Err(TeamActionError::Failed(reason)) => text_response(500, &reason),
    }
}

/// Browsers nest the keys, as `PushSubscription.toJSON()` writes them.
fn parse_push_subscription(body: &[u8]) -> Result<PushSubscription, String> {
    #[derive(serde::Deserialize)]
    struct Keys {
        p256dh: String,
        auth: String,
    }
    #[derive(serde::Deserialize)]
    struct Browser {
        endpoint: String,
        keys: Keys,
    }
    let browser: Browser =
        serde_json::from_slice(body).map_err(|err| format!("Couldn't read the request: {err}"))?;
    if !browser.endpoint.starts_with("https://") {
        return Err("The push endpoint must be an https address".to_string());
    }
    let subscription = PushSubscription {
        endpoint: browser.endpoint,
        p256dh: browser.keys.p256dh,
        auth: browser.keys.auth,
    };
    subscription.check_keys()?;
    Ok(subscription)
}

fn parse_push_device(body: &[u8]) -> Result<PushDevice, String> {
    let device: PushDevice =
        serde_json::from_slice(body).map_err(|err| format!("Couldn't read the request: {err}"))?;
    device.check()?;
    Ok(device)
}

fn parse_action(segments: &[&str], body: &[u8]) -> Result<PhoneAction, (u16, String)> {
    #[derive(serde::Deserialize)]
    struct PermissionBody {
        tool_call_id: String,
        option_id: String,
    }
    #[derive(serde::Deserialize)]
    struct MessageBody {
        text: String,
    }
    #[derive(serde::Deserialize)]
    struct NewChatBody {
        agent: String,
    }
    #[derive(serde::Deserialize)]
    struct TaskBody {
        repository_id: String,
        agent: String,
        text: String,
    }
    #[derive(serde::Deserialize)]
    struct MergeBody {
        head: String,
    }
    fn check_message(text: &str) -> Result<(), (u16, String)> {
        if text.trim().is_empty() {
            return Err((400, "The message is empty".to_string()));
        }
        if text.len() > MAX_MESSAGE_BYTES {
            return Err((413, "The message is too long".to_string()));
        }
        Ok(())
    }
    fn read<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, (u16, String)> {
        serde_json::from_slice(body)
            .map_err(|err| (400, format!("Couldn't read the request: {err}")))
    }
    match segments {
        ["v1", "chats", chat_id, "stop"] if is_plain_id(chat_id) => Ok(PhoneAction::Stop {
            chat_id: chat_id.to_string(),
        }),
        ["v1", "chats", chat_id, "permission"] if is_plain_id(chat_id) => {
            let answer: PermissionBody = read(body)?;
            Ok(PhoneAction::Permission {
                chat_id: chat_id.to_string(),
                tool_call_id: answer.tool_call_id,
                option_id: answer.option_id,
            })
        }
        ["v1", "chats", chat_id, "message"] if is_plain_id(chat_id) => {
            let message: MessageBody = read(body)?;
            check_message(&message.text)?;
            Ok(PhoneAction::Message {
                chat_id: chat_id.to_string(),
                text: message.text,
            })
        }
        ["v1", "workspaces", workspace_id, "chats"] if is_plain_id(workspace_id) => {
            let new_chat: NewChatBody = read(body)?;
            let agent = new_chat.agent.trim();
            if agent.is_empty() {
                return Err((400, "No agent given".to_string()));
            }
            Ok(PhoneAction::NewChat {
                workspace_id: workspace_id.to_string(),
                agent: agent.to_string(),
            })
        }
        ["v1", "tasks"] => {
            let task: TaskBody = read(body)?;
            if !is_plain_id(&task.repository_id) {
                return Err((400, "No repository given".to_string()));
            }
            let agent = task.agent.trim();
            if agent.is_empty() {
                return Err((400, "No agent given".to_string()));
            }
            check_message(&task.text)?;
            Ok(PhoneAction::StartTask {
                repository_id: task.repository_id,
                agent: agent.to_string(),
                text: task.text,
            })
        }
        ["v1", "workspaces", workspace_id, "pr"] if is_plain_id(workspace_id) => {
            Ok(PhoneAction::CreatePr {
                workspace_id: workspace_id.to_string(),
            })
        }
        ["v1", "workspaces", workspace_id, "merge"] if is_plain_id(workspace_id) => {
            let merge: MergeBody = read(body)?;
            let head = merge.head.trim();
            if head.is_empty() {
                return Err((400, "No head commit given".to_string()));
            }
            Ok(PhoneAction::Merge {
                workspace_id: workspace_id.to_string(),
                head: head.to_string(),
            })
        }
        _ => Err((404, "Not found".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::team_share_service::{
        TeamAgentState, TeamCaller, TeamChat, TeamChatUpdate, TeamEntry, TeamEntryChange,
        TeamPermissionRequest, TeamShareHub, TeamWorkspace,
    };
    use std::io::{BufRead as _, Write as _};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct FixedLogin(&'static str);

    impl CallerIdentity for FixedLogin {
        fn caller(&self, _address: IpAddr) -> Result<Option<TeamCaller>, String> {
            Ok(Some(TeamCaller {
                login: self.0.to_string(),
                name: "Jay".to_string(),
            }))
        }
    }

    type Recorded = Arc<Mutex<Vec<PhoneAction>>>;

    #[derive(Default)]
    struct PushProbe {
        devices: Mutex<Vec<PushDevice>>,
        alerts: std::sync::atomic::AtomicUsize,
    }

    fn start(
        login: &'static str,
        own_addresses: Vec<IpAddr>,
        hub: TeamShareHub,
    ) -> Result<(PhoneServer, Recorded), String> {
        start_with_tls(login, own_addresses, hub, None)
    }

    fn start_with_tls(
        login: &'static str,
        own_addresses: Vec<IpAddr>,
        hub: TeamShareHub,
        tls: Option<TlsFiles>,
    ) -> Result<(PhoneServer, Recorded), String> {
        start_with_push(login, own_addresses, hub, tls, Arc::default())
    }

    fn start_with_push(
        login: &'static str,
        own_addresses: Vec<IpAddr>,
        hub: TeamShareHub,
        tls: Option<TlsFiles>,
        probe: Arc<PushProbe>,
    ) -> Result<(PhoneServer, Recorded), String> {
        let recorded = Recorded::default();
        let handler: PhoneActionHandler = Arc::new({
            let recorded = recorded.clone();
            move |action| {
                recorded
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(action);
                Ok(PhoneActionOutcome::default())
            }
        });
        let server = PhoneServer::start(
            PhoneServerConfig {
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                owner_login: "Jay@Example.com".to_string(),
                own_addresses,
                tls,
            },
            hub,
            Arc::new(FixedLogin(login)),
            PhoneSources {
                diff: Arc::new(|workspace_id: &str| Ok(format!("diff of {workspace_id}"))),
                agents: Arc::new(Vec::new),
                repositories: Arc::new(|| {
                    Ok(vec![
                        PhoneRepository {
                            id: "repo-1".to_string(),
                            name: "alpha-repo".to_string(),
                        },
                        PhoneRepository {
                            id: "repo-2".to_string(),
                            name: "beta-repo".to_string(),
                        },
                    ])
                }),
                pr_status: Arc::new(|_| Ok(PhonePrStatus::default())),
                push_key: Arc::new(|| Ok("key".to_string())),
                save_push_subscription: Arc::new(|_| Ok(())),
                save_push_device: Arc::new({
                    let probe = probe.clone();
                    move |device| {
                        probe
                            .devices
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(device);
                        Ok(())
                    }
                }),
                send_test_alert: Arc::new(move || {
                    probe
                        .alerts
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }),
            },
            handler,
        )?;
        Ok((server, recorded))
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

    fn get(address: SocketAddr, path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
    }

    fn post_hi_message(address: SocketAddr, headers: &str) -> String {
        let body = r#"{"text":"hi"}"#;
        format!(
            "POST /v1/chats/chat-1/message HTTP/1.1\r\nHost: {address}\r\n{headers}\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn refuses_another_login() -> Result<(), String> {
        let (server, _recorded) = start("bob@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();

        assert_eq!(raw_status(address, &get(address, "/v1/workspaces"))?, 403);
        Ok(())
    }

    #[test]
    fn refuses_the_macs_own_address() -> Result<(), String> {
        let (server, _recorded) = start(
            "jay@example.com",
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            TeamShareHub::new(),
        )?;
        let address = server.address();

        assert_eq!(raw_status(address, &get(address, "/v1/workspaces"))?, 403);
        Ok(())
    }

    #[test]
    fn refuses_a_post_from_another_origin() -> Result<(), String> {
        let (server, recorded) = start("jay@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();

        let request = post_hi_message(
            address,
            &format!(
                "Origin: http://evil.example\r\nContent-Type: application/json\r\n\
                 {PHONE_REQUEST_HEADER}: 1\r\n"
            ),
        );
        assert_eq!(raw_status(address, &request)?, 403);
        assert!(recorded.lock().map_err(|err| err.to_string())?.is_empty());
        Ok(())
    }

    #[test]
    fn refuses_a_post_without_an_origin() -> Result<(), String> {
        let (server, recorded) = start("jay@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();

        let request = post_hi_message(
            address,
            &format!("Content-Type: application/json\r\n{PHONE_REQUEST_HEADER}: 1\r\n"),
        );
        assert_eq!(raw_status(address, &request)?, 403);
        assert!(recorded.lock().map_err(|err| err.to_string())?.is_empty());
        Ok(())
    }

    #[test]
    fn refuses_a_post_without_the_phone_header() -> Result<(), String> {
        let (server, recorded) = start("jay@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();

        let request = post_hi_message(
            address,
            &format!("Origin: http://{address}\r\nContent-Type: application/json\r\n"),
        );
        assert_eq!(raw_status(address, &request)?, 400);
        assert!(recorded.lock().map_err(|err| err.to_string())?.is_empty());
        Ok(())
    }

    #[test]
    fn accepts_a_same_origin_message() -> Result<(), String> {
        let (server, recorded) = start("jay@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();

        let request = post_hi_message(
            address,
            &format!(
                "Origin: http://{address}\r\nContent-Type: application/json\r\n\
                 {PHONE_REQUEST_HEADER}: 1\r\n"
            ),
        );
        assert_eq!(raw_status(address, &request)?, 200);
        assert_eq!(
            *recorded.lock().map_err(|err| err.to_string())?,
            vec![PhoneAction::Message {
                chat_id: "chat-1".to_string(),
                text: "hi".to_string(),
            }]
        );
        Ok(())
    }

    #[test]
    fn refuses_a_non_https_push_endpoint() -> Result<(), String> {
        let (server, _recorded) = start("jay@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();
        let body = r#"{"endpoint":"http://push.example/x","keys":{"p256dh":"a","auth":"b"}}"#;
        let request = format!(
            "POST /v1/push/subscriptions HTTP/1.1\r\nHost: {address}\r\n\
             Origin: http://{address}\r\nContent-Type: application/json\r\n\
             {PHONE_REQUEST_HEADER}: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        assert_eq!(raw_status(address, &request)?, 400);
        Ok(())
    }

    #[test]
    fn refuses_malformed_push_keys() -> Result<(), String> {
        let (server, _recorded) = start("jay@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();
        let subscribe = |p256dh: &str, auth: &str| {
            let body = format!(
                r#"{{"endpoint":"https://push.example/x","keys":{{"p256dh":"{p256dh}","auth":"{auth}"}}}}"#
            );
            raw_status(
                address,
                &format!(
                    "POST /v1/push/subscriptions HTTP/1.1\r\nHost: {address}\r\n\
                     Origin: http://{address}\r\nContent-Type: application/json\r\n\
                     {PHONE_REQUEST_HEADER}: 1\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                ),
            )
        };
        let p256dh = concat!(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx",
            "aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"
        );
        let compressed = "AiVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx";
        let auth = "BTBZMqHH6r4Tts7J_aSIgg";

        assert_eq!(subscribe(p256dh, auth)?, 200);
        assert_eq!(subscribe("not a key", auth)?, 400);
        assert_eq!(subscribe(compressed, auth)?, 400);
        assert_eq!(subscribe(p256dh, "BTBZMqHH6r4Tts7J_aSI")?, 400);
        Ok(())
    }

    const DEVICE_P256DH: &str = concat!(
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx",
        "aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"
    );

    fn post_app(address: SocketAddr, path: &str, body: &str) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\n\
             Content-Type: application/json\r\n{PHONE_REQUEST_HEADER}: app\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn device_body(p256dh: &str) -> String {
        format!(
            r#"{{"install_id":"install-1","token":"fcm-token","p256dh":"{p256dh}","auth":"BTBZMqHH6r4Tts7J_aSIgg","name":"Pixel"}}"#
        )
    }

    #[test]
    fn push_device_route_saves_device() -> Result<(), String> {
        let probe = Arc::new(PushProbe::default());
        let (server, _recorded) = start_with_push(
            "jay@example.com",
            Vec::new(),
            TeamShareHub::new(),
            None,
            probe.clone(),
        )?;
        let address = server.address();

        let request = post_app(address, "/v1/push/devices", &device_body(DEVICE_P256DH));
        assert_eq!(raw_status(address, &request)?, 200);
        assert_eq!(
            *probe.devices.lock().map_err(|err| err.to_string())?,
            [PushDevice {
                install_id: "install-1".to_string(),
                token: "fcm-token".to_string(),
                p256dh: DEVICE_P256DH.to_string(),
                auth: "BTBZMqHH6r4Tts7J_aSIgg".to_string(),
                name: "Pixel".to_string(),
            }]
        );
        Ok(())
    }

    #[test]
    fn push_device_route_refuses_bad_keys() -> Result<(), String> {
        let probe = Arc::new(PushProbe::default());
        let (server, _recorded) = start_with_push(
            "jay@example.com",
            Vec::new(),
            TeamShareHub::new(),
            None,
            probe.clone(),
        )?;
        let address = server.address();

        let request = post_app(address, "/v1/push/devices", &device_body("abc"));
        assert_eq!(raw_status(address, &request)?, 400);
        assert!(
            probe
                .devices
                .lock()
                .map_err(|err| err.to_string())?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn app_post_with_matching_origin_is_accepted() -> Result<(), String> {
        let probe = Arc::new(PushProbe::default());
        let (server, _recorded) = start_with_push(
            "jay@example.com",
            Vec::new(),
            TeamShareHub::new(),
            None,
            probe.clone(),
        )?;
        let address = server.address();

        let request = post_app(address, "/v1/push/test", "{}");
        assert_eq!(raw_status(address, &request)?, 200);
        assert_eq!(probe.alerts.load(std::sync::atomic::Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn serves_the_page() -> Result<(), String> {
        let (server, _recorded) = start("jay@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();

        let mut stream = TcpStream::connect(address).map_err(|err| err.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|err| err.to_string())?;
        stream
            .write_all(get(address, "/").as_bytes())
            .map_err(|err| err.to_string())?;
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).map_err(|err| err.to_string())?;
        let (head, body) = response
            .split_once("\r\n\r\n")
            .ok_or_else(|| format!("No end of headers in {response:?}"))?;
        let head = head.to_ascii_lowercase();

        assert!(head.starts_with("http/1.1 200"), "answered {head}");
        assert!(head.contains("content-type: text/html"), "answered {head}");
        assert!(head.contains("cache-control: no-store"), "answered {head}");
        assert!(head.contains("x-frame-options: deny"), "answered {head}");
        assert!(head.contains("frame-ancestors 'none'"), "answered {head}");
        assert!(body.contains(PHONE_REQUEST_HEADER));
        Ok(())
    }

    #[test]
    fn serves_the_service_worker_as_javascript() -> Result<(), String> {
        let (server, _recorded) = start("jay@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();

        let mut stream = TcpStream::connect(address).map_err(|err| err.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|err| err.to_string())?;
        stream
            .write_all(get(address, "/sw.js").as_bytes())
            .map_err(|err| err.to_string())?;
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).map_err(|err| err.to_string())?;
        let head = response.to_ascii_lowercase();

        assert!(head.starts_with("http/1.1 200"), "answered {head}");
        assert!(
            head.contains("content-type: text/javascript"),
            "answered {head}"
        );
        assert!(head.contains("cache-control: no-store"), "answered {head}");
        Ok(())
    }

    #[test]
    fn unknown_workspace_diff_is_404() -> Result<(), String> {
        let (server, _recorded) = start("jay@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();

        assert_eq!(
            raw_status(address, &get(address, "/v1/workspaces/ws-x/diff"))?,
            404
        );
        Ok(())
    }

    #[test]
    fn lists_repositories() -> Result<(), String> {
        let (server, _recorded) = start("jay@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();

        let mut stream = TcpStream::connect(address).map_err(|err| err.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|err| err.to_string())?;
        stream
            .write_all(get(address, "/v1/repositories").as_bytes())
            .map_err(|err| err.to_string())?;
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response).map_err(|err| err.to_string())?;

        assert!(response.starts_with("HTTP/1.1 200"), "answered {response}");
        assert!(response.contains("alpha-repo"), "answered {response}");
        assert!(response.contains("beta-repo"), "answered {response}");
        Ok(())
    }

    #[test]
    fn refuses_a_task_from_another_origin() -> Result<(), String> {
        let (server, recorded) = start("jay@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();

        let body = r#"{"repository_id":"repo-1","agent":"claude","text":"Fix the login page"}"#;
        let request = format!(
            "POST /v1/tasks HTTP/1.1\r\nHost: {address}\r\nOrigin: http://evil.example\r\n\
             Content-Type: application/json\r\n{PHONE_REQUEST_HEADER}: 1\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        assert_eq!(raw_status(address, &request)?, 403);
        assert!(recorded.lock().map_err(|err| err.to_string())?.is_empty());
        Ok(())
    }

    #[test]
    fn parses_new_actions() {
        assert_eq!(
            parse_action(
                &["v1", "tasks"],
                br#"{"repository_id":"repo-1","agent":"codex","text":"Fix the login page"}"#
            ),
            Ok(PhoneAction::StartTask {
                repository_id: "repo-1".to_string(),
                agent: "codex".to_string(),
                text: "Fix the login page".to_string(),
            })
        );
        assert_eq!(
            parse_action(&["v1", "workspaces", "ws-1", "pr"], b"{}"),
            Ok(PhoneAction::CreatePr {
                workspace_id: "ws-1".to_string(),
            })
        );
        assert_eq!(
            parse_action(
                &["v1", "workspaces", "ws-1", "merge"],
                br#"{"head":"0123abc"}"#
            ),
            Ok(PhoneAction::Merge {
                workspace_id: "ws-1".to_string(),
                head: "0123abc".to_string(),
            })
        );
        let status = |segments: &[&str], body: &[u8]| {
            parse_action(segments, body).map_err(|(status, _reason)| status)
        };
        assert_eq!(
            status(
                &["v1", "tasks"],
                br#"{"repository_id":"repo-1","agent":"codex","text":"  "}"#
            ),
            Err(400)
        );
        assert_eq!(
            status(&["v1", "workspaces", "ws-1", "merge"], br#"{"head":""}"#),
            Err(400)
        );
    }

    #[test]
    fn unknown_workspace_pr_is_404() -> Result<(), String> {
        let (server, _recorded) = start("jay@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();

        assert_eq!(
            raw_status(address, &get(address, "/v1/workspaces/ws-x/pr"))?,
            404
        );
        Ok(())
    }

    #[test]
    fn origin_must_match_the_scheme() {
        let host = "mac.example.ts.net:47232";
        assert!(is_same_origin(
            "https://mac.example.ts.net:47232",
            host,
            true
        ));
        assert!(!is_same_origin(
            "http://mac.example.ts.net:47232",
            host,
            true
        ));
        assert!(is_same_origin(
            "http://mac.example.ts.net:47232",
            host,
            false
        ));
        assert!(!is_same_origin(
            "https://mac.example.ts.net:47232",
            host,
            false
        ));
    }

    const TEST_ONLY_CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----
MIIBcDCCARagAwIBAgIUQ6yTV6qISGnc9CH+UfTTsP7fChcwCgYIKoZIzj0EAwIw
HTEbMBkGA1UEAwwSbWFjLmV4YW1wbGUudHMubmV0MCAXDTI2MTExNzIyMTQzOFoY
DzIxMjYxMDI0MjIxNDM4WjAdMRswGQYDVQQDDBJtYWMuZXhhbXBsZS50cy5uZXQw
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAR1/rdlSJPyi502yA5RhYs8hzUSjh9F
vqKM1y3nIqhcH3hj5xb1yHWIE0YZjV/rQPaX+8VoZgjy2w44B62B4nytozIwMDAd
BgNVHQ4EFgQUpMNeRtP7OS7eCjHcLPQOvjWSHLcwDwYDVR0TAQH/BAUwAwEB/zAK
BggqhkjOPQQDAgNIADBFAiEAiYqmwDSyNeHydMvTvePsYRpwS4mutHnOFhYwi9SE
m8QCIDzNuasX+UoiDg6f2Ye75bRYtCXUav2z0T9pivbEFjlG
-----END CERTIFICATE-----
";

    /// In the SEC1 "EC PRIVATE KEY" form `tailscale cert` writes.
    const TEST_ONLY_PRIVATE_KEY: &str = "-----BEGIN EC PRIVATE KEY-----
MHcCAQEEIDhUdshKgImfYeIMfnGOJ5ZYEuR5P7ghCqyMM14rwsHEoAoGCCqGSM49
AwEHoUQDQgAEdf63ZUiT8oudNsgOUYWLPIc1Eo4fRb6ijNct5yKoXB94Y+cW9ch1
iBNGGY1f60D2l/vFaGYI8tsOOAetgeJ8rQ==
-----END EC PRIVATE KEY-----
";

    #[test]
    fn serves_https_with_a_tailscale_key() -> Result<(), String> {
        let (server, _recorded) = start_with_tls(
            "jay@example.com",
            Vec::new(),
            TeamShareHub::new(),
            Some(TlsFiles {
                certificate: TEST_ONLY_CERTIFICATE.as_bytes().to_vec(),
                private_key: TEST_ONLY_PRIVATE_KEY.as_bytes().to_vec(),
            }),
        )?;
        assert_ne!(server.address().port(), 0);
        Ok(())
    }

    #[test]
    fn refuses_a_key_it_cannot_read() {
        let started = start_with_tls(
            "jay@example.com",
            Vec::new(),
            TeamShareHub::new(),
            Some(TlsFiles {
                certificate: TEST_ONLY_CERTIFICATE.as_bytes().to_vec(),
                private_key:
                    b"-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----\n".to_vec(),
            }),
        );
        assert!(started.is_err());
    }

    fn wire_fixture_chat(id: &str, title: &str) -> TeamChat {
        TeamChat {
            id: id.to_string(),
            title: title.to_string(),
            agent: "claude".to_string(),
            open: true,
            live: true,
            last_turn_ended_at: None,
            last_turn_errored: false,
            waiting: false,
        }
    }

    fn wire_fixture_update() -> TeamChatUpdate {
        use agent_client_protocol::schema as acp;
        let tool_entry = |id: &str, title: &str, kind: acp::ToolKind, status| TeamEntry::Updates {
            updates: vec![
                acp::SessionUpdate::ToolCall(
                    acp::ToolCall::new(acp::ToolCallId::new(id.to_string()), title.to_string())
                        .kind(kind),
                ),
                acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                    acp::ToolCallId::new(id.to_string()),
                    acp::ToolCallUpdateFields::new().status(status),
                )),
            ],
        };
        let message_entry = |update: acp::SessionUpdate| TeamEntry::Updates {
            updates: vec![update],
        };
        let changes = vec![
            message_entry(acp::SessionUpdate::UserMessageChunk(
                acp::ContentChunk::new("Fix the login page".into()),
            )),
            message_entry(acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new("Looking at the form handler".into()),
            )),
            tool_entry(
                "tool-1",
                "Read login.rs",
                acp::ToolKind::Read,
                acp::ToolCallStatus::Completed,
            ),
            tool_entry(
                "tool-2",
                "Run cargo test",
                acp::ToolKind::Execute,
                acp::ToolCallStatus::Pending,
            ),
            tool_entry(
                "tool-3",
                "Edit login.rs",
                acp::ToolKind::Edit,
                acp::ToolCallStatus::InProgress,
            ),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, entry)| TeamEntryChange { index, entry })
        .collect::<Vec<_>>();
        TeamChatUpdate {
            cursor: "cursor-5".to_string(),
            reset: true,
            len: changes.len(),
            changes,
            plan: vec![
                acp::PlanEntry::new(
                    "Reproduce the bug",
                    acp::PlanEntryPriority::High,
                    acp::PlanEntryStatus::Completed,
                ),
                acp::PlanEntry::new(
                    "Fix the handler",
                    acp::PlanEntryPriority::Medium,
                    acp::PlanEntryStatus::InProgress,
                ),
            ],
            generating: true,
            permissions: vec![TeamPermissionRequest {
                tool_call_id: "tool-2".to_string(),
                options: vec![
                    acp::PermissionOption::new(
                        "allow",
                        "Allow",
                        acp::PermissionOptionKind::AllowOnce,
                    ),
                    acp::PermissionOption::new(
                        "reject",
                        "Deny",
                        acp::PermissionOptionKind::RejectOnce,
                    ),
                ],
            }],
        }
    }

    #[test]
    fn android_wire_fixtures_match() -> Result<(), String> {
        let mut waiting_chat = wire_fixture_chat("chat-1", "Fix the login page");
        waiting_chat.waiting = true;
        let mut finished_chat = wire_fixture_chat("chat-2", "Write the docs");
        finished_chat.last_turn_ended_at = Some(1_700_000_100_000);
        let workspaces = TeamWorkspaceList {
            workspaces: vec![TeamWorkspace {
                id: "ws-1".to_string(),
                project: "alpha-repo".to_string(),
                name: "login-fix".to_string(),
                branch: "jay/login-fix".to_string(),
                pr_number: Some(42),
                pr_state: Some("ready".to_string()),
                changed_files: 3,
                additions: 20,
                deletions: 4,
                agent_state: TeamAgentState::WaitingForApproval,
                idle_seconds: Some(12),
                chats: vec![waiting_chat, finished_chat],
                step_in: true,
                share_files: false,
                edit_files: false,
                remote_url: None,
                base_branch: Some("main".to_string()),
                skip_patterns: Vec::new(),
                agents: false,
                root_path: None,
            }],
        };
        let pr_status = PhonePrStatus {
            found: true,
            number: Some(42),
            title: Some("Fix the login page".to_string()),
            url: Some("https://github.com/example/alpha-repo/pull/42".to_string()),
            state: Some("OPEN".to_string()),
            is_draft: false,
            merge_state_status: Some("CLEAN".to_string()),
            head_ref_oid: Some("0123abcd".to_string()),
            checks_summary: "3 of 3 checks passed".to_string(),
            can_merge: true,
            merge_method: "squash".to_string(),
        };
        let agents = PhoneAgentList {
            agents: vec![PhoneAgent {
                id: "claude".to_string(),
                label: "Claude".to_string(),
            }],
        };
        let repositories = PhoneRepositoryList {
            repositories: vec![PhoneRepository {
                id: "repo-1".to_string(),
                name: "alpha-repo".to_string(),
            }],
        };
        let outcome = PhoneActionOutcome {
            chat_id: Some("chat-9".to_string()),
            workspace_id: Some("ws-9".to_string()),
            message: None,
        };
        let fixtures = [
            ("workspaces.json", to_pretty(&workspaces)?),
            ("chat_update.json", to_pretty(&wire_fixture_update())?),
            ("pr_status.json", to_pretty(&pr_status)?),
            ("agents.json", to_pretty(&agents)?),
            ("repositories.json", to_pretty(&repositories)?),
            ("outcome.json", to_pretty(&outcome)?),
        ];
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../android/app/src/test/resources/wire");
        let update = std::env::var("UPDATE_ANDROID_FIXTURES").is_ok_and(|value| value == "1");
        if update {
            std::fs::create_dir_all(&directory).map_err(|err| err.to_string())?;
        }
        for (name, json) in fixtures {
            let path = directory.join(name);
            if update {
                std::fs::write(&path, format!("{json}\n")).map_err(|err| err.to_string())?;
            } else {
                let saved = std::fs::read_to_string(&path)
                    .map_err(|err| format!("{}: {err}", path.display()))?;
                if saved.trim_end() != json {
                    return Err(format!(
                        "{name} is out of date; rerun with UPDATE_ANDROID_FIXTURES=1"
                    ));
                }
            }
        }
        Ok(())
    }

    fn to_pretty<T: serde::Serialize>(value: &T) -> Result<String, String> {
        serde_json::to_string_pretty(value).map_err(|err| err.to_string())
    }
}
