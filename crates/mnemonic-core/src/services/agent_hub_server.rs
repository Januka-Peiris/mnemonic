use std::collections::HashMap;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use tiny_http::{Header, Method, Request, Response, Server};

use crate::services::mcp_http::{
    McpReply, McpTools, accepts_event_stream, handle_mcp_message, mcp_request_method,
    write_event_stream,
};

pub const AGENT_HUB_SERVER_NAME: &str = "mnemonic";
pub const AGENT_HUB_KEY_HEADER: &str = "X-Mnemonic-Chat-Key";

const MCP_SESSION_HEADER: &str = "Mcp-Session-Id";
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_ACTIVE_REQUESTS: usize = 64;
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Maps each chat's secret key to the session it belongs to. A key is minted
/// before its session exists, so it stays unbound until `bind`.
#[derive(Clone, Default)]
pub struct HubKeys {
    sessions: Arc<Mutex<HashMap<String, Option<String>>>>,
}

impl HubKeys {
    pub fn mint(&self) -> String {
        let key = uuid::Uuid::new_v4().to_string();
        lock(&self.sessions).insert(key.clone(), None);
        key
    }

    pub fn bind(&self, key: &str, session_id: &str) {
        if let Some(bound) = lock(&self.sessions).get_mut(key) {
            *bound = Some(session_id.to_string());
        }
    }

    /// `None` means the key is unknown.
    pub fn session_for(&self, key: &str) -> Option<Option<String>> {
        lock(&self.sessions).get(key).cloned()
    }
}

#[derive(Clone, Debug)]
pub struct HubCaller {
    pub session_id: String,
}

pub trait AgentHubHandler: Send + Sync + 'static {
    fn definitions(&self, caller: &HubCaller) -> serde_json::Value;
    /// Blocking; runs on a request thread. `cancelled` turns true when the client cancels the call.
    fn call(
        &self,
        caller: &HubCaller,
        tool: &str,
        arguments: serde_json::Value,
        cancelled: Arc<AtomicBool>,
    ) -> Result<String, String>;
}

type CancelFlags = Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>;

struct Shared {
    keys: HubKeys,
    handler: Arc<dyn AgentHubHandler>,
    cancel_flags: CancelFlags,
    port: u16,
    keep_alive: Duration,
    active_requests: AtomicUsize,
}

pub struct AgentHubServer {
    server: Arc<Server>,
    address: SocketAddr,
    accept_thread: Option<JoinHandle<()>>,
}

impl AgentHubServer {
    pub fn start(keys: HubKeys, handler: Arc<dyn AgentHubHandler>) -> Result<Self, String> {
        Self::start_with_keep_alive(keys, handler, KEEP_ALIVE_INTERVAL)
    }

    fn start_with_keep_alive(
        keys: HubKeys,
        handler: Arc<dyn AgentHubHandler>,
        keep_alive: Duration,
    ) -> Result<Self, String> {
        let server = Server::http((Ipv4Addr::LOCALHOST, 0))
            .map_err(|err| format!("Couldn't start the agent hub: {err}"))?;
        let address = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| "The agent hub has no IP address".to_string())?;
        let shared = Arc::new(Shared {
            keys,
            handler,
            cancel_flags: CancelFlags::default(),
            port: address.port(),
            keep_alive,
            active_requests: AtomicUsize::new(0),
        });
        let server = Arc::new(server);
        let accept_thread = std::thread::Builder::new()
            .name("mnemonic-agent-hub".into())
            .spawn({
                let server = server.clone();
                move || accept_requests(&server, &shared)
            })
            .map_err(|err| format!("Couldn't start the agent hub: {err}"))?;
        log::info!(target: "mnemonic_lib", "Agent hub listening on {address}");
        Ok(Self {
            server,
            address,
            accept_thread: Some(accept_thread),
        })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.address.port())
    }
}

impl Drop for AgentHubServer {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(thread) = self.accept_thread.take() {
            if thread.join().is_err() {
                log::error!(target: "mnemonic_lib", "The agent hub thread panicked");
            }
        }
    }
}

fn accept_requests(server: &Server, shared: &Arc<Shared>) {
    loop {
        let Ok(request) = server.recv() else {
            break;
        };
        if shared.active_requests.fetch_add(1, Ordering::SeqCst) >= MAX_ACTIVE_REQUESTS {
            shared.active_requests.fetch_sub(1, Ordering::SeqCst);
            respond(request, text_response(503, "Too many requests"));
            continue;
        }
        let shared_for_request = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("mnemonic-agent-hub-request".into())
            .spawn(move || {
                handle(request, &shared_for_request);
                shared_for_request
                    .active_requests
                    .fetch_sub(1, Ordering::SeqCst);
            });
        if let Err(err) = spawned {
            shared.active_requests.fetch_sub(1, Ordering::SeqCst);
            log::error!(target: "mnemonic_lib", "Couldn't handle an agent hub request: {err}");
        }
    }
}

struct HubTools {
    handler: Arc<dyn AgentHubHandler>,
    caller: HubCaller,
    cancel_flags: CancelFlags,
}

impl McpTools for HubTools {
    fn server_name(&self) -> &str {
        AGENT_HUB_SERVER_NAME
    }

    fn definitions(&self) -> serde_json::Value {
        self.handler.definitions(&self.caller)
    }

    fn call(
        &self,
        call_key: &str,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, String> {
        let cancelled = Arc::new(AtomicBool::new(false));
        lock(&self.cancel_flags).insert(call_key.to_string(), cancelled.clone());
        let outcome = self
            .handler
            .call(&self.caller, name, arguments.clone(), cancelled);
        lock(&self.cancel_flags).remove(call_key);
        outcome
    }

    fn cancel(&self, call_key: &str) {
        if let Some(cancelled) = lock(&self.cancel_flags).get(call_key) {
            cancelled.store(true, Ordering::SeqCst);
        }
    }
}

fn header_value(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str().to_string())
}

fn handle(mut request: Request, shared: &Shared) {
    let path = request.url().split('?').next().unwrap_or_default();
    if path != "/mcp" {
        respond(request, text_response(404, "Not found"));
        return;
    }
    if *request.method() != Method::Post {
        respond(request, text_response(405, "Only POST is supported"));
        return;
    }
    if header_value(&request, "Origin").is_some() {
        respond(
            request,
            text_response(403, "Requests from web pages are refused"),
        );
        return;
    }
    let port = shared.port;
    let host_allowed = header_value(&request, "Host").is_some_and(|host| {
        let host = host.trim().to_ascii_lowercase();
        host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
    });
    if !host_allowed {
        respond(request, text_response(403, "Unexpected host name"));
        return;
    }
    let Some(key) = header_value(&request, AGENT_HUB_KEY_HEADER) else {
        respond(request, text_response(401, "Missing chat key"));
        return;
    };
    let session_id = match shared.keys.session_for(key.trim()) {
        None => {
            respond(request, text_response(401, "Unknown chat key"));
            return;
        }
        Some(None) => {
            respond(
                request,
                text_response(409, "This chat's session isn't ready yet"),
            );
            return;
        }
        Some(Some(session_id)) => session_id,
    };

    let wants_event_stream = {
        let http_version = request.http_version();
        accepts_event_stream(
            header_value(&request, "Accept").as_deref(),
            (http_version.0, http_version.1),
        )
    };
    let mcp_session = header_value(&request, MCP_SESSION_HEADER).unwrap_or_default();
    if request
        .body_length()
        .is_some_and(|length| length > MAX_BODY_BYTES)
    {
        respond(request, text_response(413, "The request is too long"));
        return;
    }
    let mut body = Vec::new();
    let read = request
        .as_reader()
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body);
    if let Err(err) = read {
        respond(
            request,
            text_response(400, &format!("Couldn't read the request: {err}")),
        );
        return;
    }
    if body.len() > MAX_BODY_BYTES {
        respond(request, text_response(413, "The request is too long"));
        return;
    }

    let (method, has_id) = mcp_request_method(&body);
    let is_initialize = method.as_deref() == Some("initialize");
    let is_tool_call = method.as_deref() == Some("tools/call") && has_id;
    let mcp_session = if is_initialize {
        uuid::Uuid::new_v4().to_string()
    } else {
        mcp_session
    };
    let caller_key = format!("{}:{mcp_session}", key.trim());
    let tools = HubTools {
        handler: shared.handler.clone(),
        caller: HubCaller { session_id },
        cancel_flags: shared.cancel_flags.clone(),
    };

    if is_tool_call && wants_event_stream {
        let (sender, receiver) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("mnemonic-agent-hub-tool".into())
            .spawn(move || {
                if let McpReply::Json(text) = handle_mcp_message(&tools, &caller_key, &body) {
                    if sender.send(text).is_err() {
                        log::debug!(target: "mnemonic_lib", "A hub tool finished after its caller left");
                    }
                }
            });
        if let Err(err) = spawned {
            respond(
                request,
                text_response(503, &format!("Couldn't run the tool: {err}")),
            );
            return;
        }
        let mut writer = request.into_writer();
        if let Err(err) = write_event_stream(&mut writer, &receiver, shared.keep_alive) {
            log::debug!(target: "mnemonic_lib", "An agent stopped listening to the hub: {err}");
        }
        return;
    }

    let mut response = match handle_mcp_message(&tools, &caller_key, &body) {
        McpReply::Json(text) => {
            let mut response = Response::from_string(text).with_status_code(200);
            if let Some(header) = header("Content-Type", "application/json") {
                response.add_header(header);
            }
            response
        }
        McpReply::Accepted => Response::from_string("").with_status_code(202),
    };
    if is_initialize {
        if let Some(header) = header(MCP_SESSION_HEADER, &mcp_session) {
            response.add_header(header);
        }
    }
    respond(request, response);
}

fn header(name: &str, value: &str) -> Option<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
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
        log::debug!(target: "mnemonic_lib", "An agent hub request ended early: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpStream;

    struct FakeHandler {
        sessions_seen: Mutex<Vec<String>>,
    }

    impl AgentHubHandler for FakeHandler {
        fn definitions(&self, caller: &HubCaller) -> serde_json::Value {
            lock(&self.sessions_seen).push(caller.session_id.clone());
            serde_json::json!([{"name": "fake_tool", "inputSchema": {"type": "object"}}])
        }

        fn call(
            &self,
            _caller: &HubCaller,
            tool: &str,
            _arguments: serde_json::Value,
            cancelled: Arc<AtomicBool>,
        ) -> Result<String, String> {
            match tool {
                "slow" => {
                    std::thread::sleep(Duration::from_millis(300));
                    Ok("slow result".to_string())
                }
                "until_cancelled" => {
                    let started = std::time::Instant::now();
                    while !cancelled.load(Ordering::SeqCst) {
                        if started.elapsed() > Duration::from_secs(10) {
                            return Err("never cancelled".to_string());
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Ok("cancelled".to_string())
                }
                other => Err(format!("unknown tool {other}")),
            }
        }
    }

    fn start() -> Result<(AgentHubServer, HubKeys, Arc<FakeHandler>), String> {
        let handler = Arc::new(FakeHandler {
            sessions_seen: Mutex::new(Vec::new()),
        });
        let keys = HubKeys::default();
        let server = AgentHubServer::start_with_keep_alive(
            keys.clone(),
            handler.clone(),
            Duration::from_millis(50),
        )?;
        Ok((server, keys, handler))
    }

    fn post(server: &AgentHubServer, headers: &str, body: &str) -> Result<(u16, String), String> {
        let port = server.address.port();
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
             {headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut stream = TcpStream::connect(server.address).map_err(|err| err.to_string())?;
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

    fn key_header(key: &str) -> String {
        format!("{AGENT_HUB_KEY_HEADER}: {key}\r\n")
    }

    fn call_body(id: u32, tool: &str) -> String {
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":{}}}).to_string()
    }

    const LIST_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

    #[test]
    fn refuses_missing_or_unknown_key() -> Result<(), String> {
        let (server, _keys, _handler) = start()?;

        let (missing, _) = post(&server, "", LIST_BODY)?;
        let (unknown, _) = post(&server, &key_header("not-minted"), LIST_BODY)?;

        assert_eq!(missing, 401);
        assert_eq!(unknown, 401);
        Ok(())
    }

    #[test]
    fn refuses_a_key_whose_session_is_not_bound_yet() -> Result<(), String> {
        let (server, keys, _handler) = start()?;
        let key = keys.mint();

        let (status, _) = post(&server, &key_header(&key), LIST_BODY)?;

        assert_eq!(status, 409);
        Ok(())
    }

    #[test]
    fn refuses_requests_from_a_browser() -> Result<(), String> {
        let (server, keys, _handler) = start()?;
        let key = keys.mint();
        keys.bind(&key, "s1");

        let (status, _) = post(
            &server,
            &format!("{}Origin: http://evil\r\n", key_header(&key)),
            LIST_BODY,
        )?;

        assert_eq!(status, 403);
        Ok(())
    }

    #[test]
    fn refuses_a_rebound_host_name() -> Result<(), String> {
        let (server, keys, _handler) = start()?;
        let key = keys.mint();
        keys.bind(&key, "s1");
        let port = server.address.port();
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: evil.example:{port}\r\nContent-Type: application/json\r\n\
             {}Content-Length: {}\r\nConnection: close\r\n\r\n{LIST_BODY}",
            key_header(&key),
            LIST_BODY.len()
        );
        let mut stream = TcpStream::connect(server.address).map_err(|err| err.to_string())?;
        stream
            .write_all(request.as_bytes())
            .map_err(|err| err.to_string())?;
        let mut answer = String::new();
        stream
            .read_to_string(&mut answer)
            .map_err(|err| err.to_string())?;

        assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
        Ok(())
    }

    #[test]
    fn answers_tools_list_for_a_bound_key() -> Result<(), String> {
        let (server, keys, handler) = start()?;
        let key = keys.mint();
        keys.bind(&key, "s1");

        let (status, answer) = post(&server, &key_header(&key), LIST_BODY)?;

        assert_eq!(status, 200);
        assert!(answer.contains("fake_tool"), "{answer}");
        assert_eq!(*lock(&handler.sessions_seen), vec!["s1".to_string()]);
        Ok(())
    }

    #[test]
    fn long_call_streams_keep_alives() -> Result<(), String> {
        let (server, keys, _handler) = start()?;
        let key = keys.mint();
        keys.bind(&key, "s1");
        let headers = format!(
            "{}Accept: application/json, text/event-stream\r\n",
            key_header(&key)
        );

        let (status, answer) = post(&server, &headers, &call_body(1, "slow"))?;

        assert_eq!(status, 200);
        let keep_alive = answer
            .find(": keep-alive")
            .ok_or_else(|| format!("No keep-alive in {answer:?}"))?;
        let result = answer
            .find("slow result")
            .ok_or_else(|| format!("No result in {answer:?}"))?;
        assert!(keep_alive < result, "{answer}");
        Ok(())
    }

    #[test]
    fn cancelled_call_sets_cancel_flag() -> Result<(), String> {
        let (server, keys, _handler) = start()?;
        let key = keys.mint();
        keys.bind(&key, "s1");
        let headers = key_header(&key);
        let call = std::thread::scope(|scope| -> Result<(u16, String), String> {
            let call = scope.spawn(|| post(&server, &headers, &call_body(7, "until_cancelled")));
            std::thread::sleep(Duration::from_millis(300));
            let cancel = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {"requestId": 7}
            })
            .to_string();
            let (status, _) = post(&server, &headers, &cancel)?;
            assert_eq!(status, 202);
            let started = std::time::Instant::now();
            let outcome = call
                .join()
                .map_err(|_| "The call thread panicked".to_string())??;
            assert!(started.elapsed() < Duration::from_secs(1));
            Ok(outcome)
        })?;

        assert!(call.1.contains("\"isError\":false"), "{}", call.1);
        assert!(call.1.contains("cancelled"), "{}", call.1);
        Ok(())
    }
}
