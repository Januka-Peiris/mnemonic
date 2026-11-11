//! The HTTP side of team sharing. Read-only: every route is a GET, and
//! nothing a caller sends can change this machine's state.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tiny_http::{Header, Method, Request, Response, Server};

use crate::services::team_share_service::{
    CallerIdentity, MAX_CHAT_WAIT, TEAM_PROTOCOL_VERSION, TeamHello, TeamShareHub,
    TeamWorkspaceList, is_plain_id,
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

struct Shared {
    hub: TeamShareHub,
    identity: Arc<dyn CallerIdentity>,
    diff_source: DiffSource,
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
        })
        .map_err(|err| err.to_string())?;
        let shared = Arc::new(Shared {
            hub,
            identity,
            diff_source,
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

fn handle(request: Request, shared: &Shared) {
    if *request.method() != Method::Get {
        respond(request, text_response(405, "Only GET is supported"));
        return;
    }
    let Some(caller) = request.remote_addr().map(|address| address.ip()) else {
        respond(request, text_response(403, "Unknown caller"));
        return;
    };
    match shared.identity.login(caller) {
        Ok(Some(login)) if shared.allowed_logins.contains(&login.trim().to_lowercase()) => {}
        Ok(Some(login)) => {
            log::info!(target: "mnemonic_lib", "Refused a team request from {login} ({caller})");
            respond(request, text_response(403, "Not on the allow list"));
            return;
        }
        Ok(None) => {
            respond(request, text_response(403, "Unknown caller"));
            return;
        }
        Err(err) => {
            log::warn!(target: "mnemonic_lib", "Couldn't identify {caller}: {err}");
            respond(request, text_response(503, "Couldn't check who you are"));
            return;
        }
    }
    shared.hub.note_request();

    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
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
    use crate::services::team_share_service::{
        TeamClient, TeamClientError, TeamEntry, TeamWorkspace,
    };
    use std::net::Ipv4Addr;

    struct FixedLogin(Option<&'static str>);

    impl CallerIdentity for FixedLogin {
        fn login(&self, _address: IpAddr) -> Result<Option<String>, String> {
            Ok(self.0.map(str::to_string))
        }
    }

    fn start(login: Option<&'static str>, hub: TeamShareHub) -> Result<TeamShareServer, String> {
        TeamShareServer::start(
            TeamServerConfig {
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                display_name: "Alice".to_string(),
                allowed_logins: vec!["Bob@Example.com".to_string()],
            },
            hub,
            Arc::new(FixedLogin(login)),
            Arc::new(|workspace_id: &str| Ok(format!("diff of {workspace_id}"))),
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
        }
    }

    #[test]
    fn an_allowed_teammate_reads_workspaces_diffs_and_live_chat_changes() -> Result<(), String> {
        let hub = TeamShareHub::new();
        hub.set_workspaces(vec![workspace("ws-1")]);
        hub.replace_chat(
            "chat-1",
            vec![TeamEntry::User {
                markdown: "Fix the login page".to_string(),
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
            TeamEntry::Assistant {
                markdown: "On it".to_string(),
                thinking: None,
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
}
