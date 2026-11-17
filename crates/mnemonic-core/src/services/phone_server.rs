//! The HTTP side of the phone page: the owner's phone, over Tailscale, reads
//! every workspace and acts on its chats. Only the Mac's own Tailscale login
//! is let in, never from the Mac itself, and never on behalf of another web
//! page.

use std::io::Read as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::thread::JoinHandle;

use tiny_http::{Header, Method, Request, Response, Server};

use crate::services::team_share_server::{
    DiffSource, accept_requests, chat_update_response, has_json_body, header_value,
    is_allowed_host, json_response, respond, text_response, truncate_diff,
};
use crate::services::team_share_service::{
    CallerIdentity, TeamActionError, TeamShareHub, TeamWorkspaceList, is_plain_id,
};

/// Sent by the page on every POST. A page on another site can't add it
/// without a CORS preflight, which this server never answers.
pub const PHONE_REQUEST_HEADER: &str = "X-Mnemonic-Phone";

const PAGE: &str = include_str!("phone_page.html");

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
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct PhoneActionOutcome {
    /// The new chat's id, for `PhoneAction::NewChat`.
    pub chat_id: Option<String>,
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

struct Shared {
    hub: TeamShareHub,
    identity: Arc<dyn CallerIdentity>,
    diff_source: DiffSource,
    agent_source: PhoneAgentSource,
    action_handler: PhoneActionHandler,
    owner_login: String,
    own_addresses: Vec<IpAddr>,
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
        diff_source: DiffSource,
        agent_source: PhoneAgentSource,
        action_handler: PhoneActionHandler,
    ) -> Result<Self, String> {
        let server = Server::http((config.bind, config.port))
            .map_err(|err| format!("Couldn't listen on {}:{}: {err}", config.bind, config.port))?;
        let address = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| "The phone server has no IP address".to_string())?;
        let shared = Arc::new(Shared {
            hub,
            identity,
            diff_source,
            agent_source,
            action_handler,
            owner_login: config.owner_login.trim().to_lowercase(),
            own_addresses: config
                .own_addresses
                .iter()
                .map(IpAddr::to_canonical)
                .collect(),
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
    if let Err((status, reason)) = check_request(&request) {
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
        ["v1", "agents"] => {
            let list = PhoneAgentList {
                agents: (shared.agent_source)(),
            };
            match serde_json::to_string(&list) {
                Ok(body) => json_response(200, body),
                Err(err) => text_response(500, &err.to_string()),
            }
        }
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
         connect-src 'self'; frame-ancestors 'none'",
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

/// The page is served by this server, so its requests are same-origin.
/// These refuse what another page can send: a foreign `Host` means DNS
/// rebinding, a foreign `Origin` means another site, and a POST must carry
/// a JSON body and the phone header, which make a browser ask first with a
/// CORS preflight that this server never answers.
fn check_request(request: &Request) -> Result<(), (u16, &'static str)> {
    let Some(host) = header_value(request, "Host").filter(|host| is_allowed_host(host)) else {
        return Err((403, "Unexpected host name"));
    };
    let is_same_origin = |origin: &str| origin == format!("http://{host}");
    let is_post = *request.method() == Method::Post;
    match header_value(request, "Origin") {
        Some(origin) if !is_same_origin(origin) => {
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
            if message.text.trim().is_empty() {
                return Err((400, "The message is empty".to_string()));
            }
            if message.text.len() > MAX_MESSAGE_BYTES {
                return Err((413, "The message is too long".to_string()));
            }
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
        _ => Err((404, "Not found".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::team_share_service::{TeamCaller, TeamShareHub};
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

    fn start(
        login: &'static str,
        own_addresses: Vec<IpAddr>,
        hub: TeamShareHub,
    ) -> Result<(PhoneServer, Recorded), String> {
        let recorded = Recorded::default();
        let handler: PhoneActionHandler = Arc::new({
            let recorded = recorded.clone();
            move |action| {
                recorded
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(action);
                Ok(PhoneActionOutcome { chat_id: None })
            }
        });
        let server = PhoneServer::start(
            PhoneServerConfig {
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                owner_login: "Jay@Example.com".to_string(),
                own_addresses,
            },
            hub,
            Arc::new(FixedLogin(login)),
            Arc::new(|workspace_id: &str| Ok(format!("diff of {workspace_id}"))),
            Arc::new(Vec::new),
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
    fn unknown_workspace_diff_is_404() -> Result<(), String> {
        let (server, _recorded) = start("jay@example.com", Vec::new(), TeamShareHub::new())?;
        let address = server.address();

        assert_eq!(
            raw_status(address, &get(address, "/v1/workspaces/ws-x/diff"))?,
            404
        );
        Ok(())
    }
}
