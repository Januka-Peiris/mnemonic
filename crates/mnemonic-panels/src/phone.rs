//! Serving the phone page: the owner's phone, over Tailscale, follows every
//! workspace's chats and acts on them. Runs while `phone.enabled` is on,
//! whether or not anything is shared with teammates.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use acp_thread::ThreadStatus;
use agent_client_protocol::schema as acp;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use mnemonic_core::services::phone_server::{
    PhoneAction, PhoneActionHandler, PhoneActionOutcome, PhoneServer, PhoneServerConfig,
};
use mnemonic_core::services::team_share_service::{
    DEFAULT_TEAM_PORT, TailscaleSelf, TailscaleWhois, TeamActionError, TeamShareHub, tailscale_self,
};
use settings::{RegisterSetting, Settings, SettingsStore};
use workspace::Workspace;

use crate::chat_feed::{
    ChatFeed, PermissionScope, SharedRecords, TranscriptToRead, WorkspaceFlags, load_records,
    read_saved_transcripts,
};
use crate::team::{answer_permission, show_step_in_notice, wait_for_reply, workspace_diff_source};
use crate::workspace_view::{ChatAgent, WorkspaceHost, find_open_chat, find_workspace_window};

const DEFAULT_NOTIFY_AFTER_IDLE_MINUTES: u32 = 5;
/// How often what the phone sees is refreshed.
const PUBLISH_INTERVAL: Duration = Duration::from_secs(2);
/// How long to wait before trying to start the server again, as when
/// Tailscale wasn't connected yet.
const RETRY_START_AFTER: Duration = Duration::from_secs(30);
/// How often the running server checks Tailscale is still on the address
/// and account it started with.
const CHECK_TAILSCALE_EVERY: Duration = Duration::from_secs(15);
/// How long the phone's request waits for the app to carry out its action.
const ACTION_TIMEOUT: Duration = Duration::from_secs(10);

/// The `phone` section of the settings file.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub(crate) struct PhoneSettings {
    pub enabled: bool,
    pub port: u16,
    pub ntfy_url: String,
    pub ntfy_token: Option<String>,
    pub notify_after_idle_minutes: u32,
}

impl Settings for PhoneSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let phone = content.phone.clone().unwrap_or_default();
        Self {
            enabled: phone.enabled.unwrap_or(false),
            port: phone.port.unwrap_or(DEFAULT_TEAM_PORT + 1),
            ntfy_url: phone.ntfy_url.unwrap_or_default().trim().to_string(),
            ntfy_token: phone
                .ntfy_token
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty()),
            notify_after_idle_minutes: phone
                .notify_after_idle_minutes
                .unwrap_or(DEFAULT_NOTIFY_AFTER_IDLE_MINUTES),
        }
    }
}

/// A phone action on its way to the main thread, with where to send the
/// answer.
struct PendingPhoneAction {
    action: PhoneAction,
    /// Set by whichever comes first: the main thread starting the action,
    /// or the request giving up on it. An action the phone was told failed
    /// must not run later.
    claimed: Arc<AtomicBool>,
    reply: std::sync::mpsc::Sender<Result<PhoneActionOutcome, TeamActionError>>,
}

pub(crate) struct PhoneShare {
    /// The port the server runs, or is starting, on. None while it is off.
    port: Option<u16>,
    server: Option<PhoneServer>,
    /// The page's address, once the server is running.
    page_url: Option<String>,
    /// Publishes every workspace and its chats into the hub the server
    /// serves. Kept for as long as the app runs, so its events can be
    /// followed across restarts of the server.
    pub(crate) feed: Entity<ChatFeed>,
    start_task: Option<Task<()>>,
    publish_task: Option<Task<()>>,
    /// Restarts the server when this Mac's Tailscale address or account
    /// changes, since it stays bound to the address it started on.
    tailscale_watch: Option<Task<()>>,
    /// Carries out the phone's actions on the main thread.
    action_task: Option<Task<()>>,
}

struct GlobalPhoneShare(Entity<PhoneShare>);

impl Global for GlobalPhoneShare {}

pub(crate) fn init(cx: &mut App) {
    let phone = cx.new(|cx| {
        let mut phone = PhoneShare::new(cx);
        phone.apply_settings(cx);
        cx.on_app_quit(|phone, cx| {
            phone.stop(cx);
            async {}
        })
        .detach();
        phone
    });
    cx.set_global(GlobalPhoneShare(phone.clone()));
    cx.observe_global::<SettingsStore>(move |cx| {
        phone.update(cx, |phone, cx| phone.apply_settings(cx));
    })
    .detach();
}

pub(crate) fn phone_share(cx: &App) -> Option<Entity<PhoneShare>> {
    cx.try_global::<GlobalPhoneShare>()
        .map(|global| global.0.clone())
}

/// The address to open on the phone, while the server is running.
pub(crate) fn phone_page_url(cx: &App) -> Option<String> {
    phone_share(cx)?.read(cx).page_url.clone()
}

impl PhoneShare {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            port: None,
            server: None,
            page_url: None,
            feed: cx.new(|_| ChatFeed::new(TeamShareHub::new(), PermissionScope::All)),
            start_task: None,
            publish_task: None,
            tailscale_watch: None,
            action_task: None,
        }
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let settings = PhoneSettings::get_global(cx);
        let port = settings.enabled.then_some(settings.port);
        if port == self.port {
            return;
        }
        match port {
            Some(port) => self.start(port, cx),
            None => self.stop(cx),
        }
        cx.notify();
    }

    /// Whether Tailscale moved since the server started: the server is bound
    /// to the old address, or lets in the old account.
    fn needs_restart(running: &TailscaleSelf, now: &TailscaleSelf) -> bool {
        running.ip != now.ip || running.login != now.login
    }

    fn start(&mut self, port: u16, cx: &mut Context<Self>) {
        self.stop(cx);
        self.port = Some(port);
        let hub = self.feed.read(cx).hub().clone();
        let (action_sender, mut action_receiver) = mpsc::unbounded::<PendingPhoneAction>();
        self.action_task = Some(cx.spawn(async move |_, cx| {
            while let Some(pending) = action_receiver.next().await {
                if pending.claimed.swap(true, Ordering::SeqCst) {
                    log::info!("Skipped a phone action that timed out while waiting");
                    continue;
                }
                let PendingPhoneAction { action, reply, .. } = pending;
                let outcome = cx.update(|cx| run_phone_action(action, cx));
                if reply.send(outcome).is_err() {
                    log::warn!("A phone action finished after its request gave up");
                }
            }
        }));
        self.start_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let attempt = cx
                    .background_spawn({
                        let hub = hub.clone();
                        let action_sender = action_sender.clone();
                        async move { start_server(port, hub, action_sender) }
                    })
                    .await;
                let started = this.update(cx, |this, cx| {
                    if this.port != Some(port) {
                        return true;
                    }
                    let started = match attempt {
                        Ok((server, started_as)) => {
                            this.page_url = Some(page_url(&started_as, server.address().port()));
                            this.server = Some(server);
                            this.start_publishing(cx);
                            this.tailscale_watch =
                                Some(Self::watch_tailscale(port, started_as, cx));
                            true
                        }
                        Err(err) => {
                            log::warn!("Couldn't start the phone server: {err}");
                            false
                        }
                    };
                    cx.notify();
                    started
                });
                match started {
                    Ok(true) | Err(_) => break,
                    Ok(false) => {}
                }
                cx.background_executor().timer(RETRY_START_AFTER).await;
            }
        }));
    }

    fn watch_tailscale(port: u16, started_as: TailscaleSelf, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CHECK_TAILSCALE_EVERY).await;
                let now = cx.background_spawn(async move { tailscale_self() }).await;
                let moved = match &now {
                    Ok(now) => Self::needs_restart(&started_as, now),
                    Err(_) => true,
                };
                if !moved {
                    continue;
                }
                this.update(cx, |this, cx| {
                    if this.port == Some(port) {
                        log::info!(
                            "Tailscale changed since the phone server started on {}; restarting it",
                            started_as.ip
                        );
                        this.start(port, cx);
                        cx.notify();
                    }
                })
                .ok();
                break;
            }
        })
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.start_task = None;
        self.publish_task = None;
        self.tailscale_watch = None;
        self.action_task = None;
        self.server = None;
        self.port = None;
        self.page_url = None;
        self.feed.update(cx, |feed, _| feed.clear());
    }

    fn start_publishing(&mut self, cx: &mut Context<Self>) {
        self.publish_task = Some(cx.spawn(async move |this, cx| {
            loop {
                if let Some(state) = mnemonic_bridge::mnemonic_state_cloned() {
                    let records = cx
                        .background_spawn(async move { load_records(&state, None) })
                        .await;
                    let published = match records {
                        Ok(records) => this.update(cx, |this, cx| this.publish(records, cx)),
                        Err(err) => {
                            log::error!("Couldn't list workspaces for the phone: {err}");
                            Ok(Vec::new())
                        }
                    };
                    let Ok(to_read) = published else {
                        break;
                    };
                    if !to_read.is_empty() {
                        let saved = cx
                            .background_spawn(async move { read_saved_transcripts(to_read) })
                            .await;
                        let shared = this.update(cx, |this, cx| {
                            if this.server.is_some() {
                                this.feed
                                    .update(cx, |feed, _| feed.share_saved_chats(saved));
                            }
                        });
                        if shared.is_err() {
                            break;
                        }
                    }
                }
                cx.background_executor().timer(PUBLISH_INTERVAL).await;
            }
        }));
    }

    /// Publishes every workspace, with nothing shared beyond its chats, and
    /// returns the saved transcripts to read for closed chats.
    fn publish(&mut self, records: SharedRecords, cx: &mut Context<Self>) -> Vec<TranscriptToRead> {
        if self.server.is_none() {
            return Vec::new();
        }
        self.feed.update(cx, |feed, cx| {
            feed.publish(records, &|_| WorkspaceFlags::default(), cx)
        })
    }
}

fn page_url(me: &TailscaleSelf, port: u16) -> String {
    let host = if me.dns_name.is_empty() {
        me.ip.to_string()
    } else {
        me.dns_name.clone()
    };
    format!("http://{host}:{port}/")
}

fn start_server(
    port: u16,
    hub: TeamShareHub,
    action_sender: mpsc::UnboundedSender<PendingPhoneAction>,
) -> Result<(PhoneServer, TailscaleSelf), String> {
    let me = tailscale_self()?;
    if me.login.trim().is_empty() {
        return Err(
            "Tailscale didn't say which account this Mac is logged in to, so your phone \
             couldn't be told apart from anyone else"
                .into(),
        );
    }
    // Runs on a request thread, which waits here while the main thread
    // carries the action out.
    let action_handler: PhoneActionHandler = Arc::new(move |action| {
        let (reply_sender, reply_receiver) = std::sync::mpsc::channel();
        let claimed = Arc::new(AtomicBool::new(false));
        let pending = PendingPhoneAction {
            action,
            claimed: claimed.clone(),
            reply: reply_sender,
        };
        if action_sender.unbounded_send(pending).is_err() {
            return Err(TeamActionError::Failed("The phone server stopped".into()));
        }
        wait_for_reply(&reply_receiver, &claimed, ACTION_TIMEOUT)
    });
    PhoneServer::start(
        PhoneServerConfig {
            bind: IpAddr::V4(me.ip),
            port,
            owner_login: me.login.clone(),
            own_addresses: vec![
                IpAddr::V4(me.ip),
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ],
        },
        hub,
        Arc::new(TailscaleWhois::default()),
        workspace_diff_source(),
        action_handler,
    )
    .map(|server| (server, me))
}

/// The agent the phone named, by its label or its id.
fn chat_agent(name: &str) -> Option<ChatAgent> {
    let name = name.trim();
    ChatAgent::ALL
        .into_iter()
        .find(|agent| agent.label().eq_ignore_ascii_case(name) || agent.agent_id() == name)
}

/// Carries out the phone's action. Unlike a teammate's, it needs no
/// step-in switch and may give any answer the agent offers.
fn run_phone_action(
    action: PhoneAction,
    cx: &mut App,
) -> Result<PhoneActionOutcome, TeamActionError> {
    match action {
        PhoneAction::Stop { chat_id } => {
            let chat = find_open_chat(&chat_id, cx).ok_or(TeamActionError::NotLoaded)?;
            // None while the chat's agent is still connecting.
            let Some(thread) = &chat.thread else {
                return Err(TeamActionError::NotLoaded);
            };
            if thread.read(cx).status() == ThreadStatus::Idle {
                return Err(TeamActionError::NothingToDo(
                    "The agent isn't running".into(),
                ));
            }
            chat.view
                .update(cx, |view, cx| view.stop_chat(&chat_id, cx));
            show_phone_notice(
                &chat.window,
                format!("stopped the agent in {}", chat.title),
                cx,
            );
            Ok(PhoneActionOutcome::default())
        }
        PhoneAction::Permission {
            chat_id,
            tool_call_id,
            option_id,
        } => {
            let chat = find_open_chat(&chat_id, cx).ok_or(TeamActionError::NotLoaded)?;
            let Some(thread) = &chat.thread else {
                return Err(TeamActionError::NotLoaded);
            };
            let (tool_title, chosen) =
                answer_permission(thread, &tool_call_id, &option_id, PermissionScope::All, cx)?;
            let answer = match chosen {
                acp::PermissionOptionKind::RejectOnce | acp::PermissionOptionKind::RejectAlways => {
                    "rejected"
                }
                _ => "allowed",
            };
            show_phone_notice(
                &chat.window,
                format!("{answer} {tool_title} in {}", chat.title),
                cx,
            );
            Ok(PhoneActionOutcome::default())
        }
        PhoneAction::Message { chat_id, text } => {
            let chat = find_open_chat(&chat_id, cx).ok_or(TeamActionError::NotLoaded)?;
            let sent = chat
                .view
                .update(cx, |view, cx| view.send_to_chat(&chat_id, text, cx));
            if !sent {
                return Err(TeamActionError::NotLoaded);
            }
            show_phone_notice(
                &chat.window,
                format!("sent a message to {}", chat.title),
                cx,
            );
            Ok(PhoneActionOutcome::default())
        }
        PhoneAction::NewChat {
            workspace_id,
            agent,
        } => {
            let agent = chat_agent(&agent)
                .ok_or_else(|| TeamActionError::Invalid(format!("Unknown agent {agent}")))?;
            let window =
                find_workspace_window(&workspace_id, cx).ok_or(TeamActionError::NotLoaded)?;
            let view = WorkspaceHost::view_in(window.read(cx), &workspace_id, cx)
                .ok_or(TeamActionError::NotLoaded)?;
            let chat_id = view
                .read(cx)
                .window_handle()
                .update(cx, |_, window, cx| {
                    view.update(cx, |view, cx| view.start_chat(agent, window, cx))
                })
                .map_err(|err| {
                    TeamActionError::Failed(format!("Couldn't start the chat: {err}"))
                })?;
            show_phone_notice(&window, format!("started a {} chat", agent.label()), cx);
            Ok(PhoneActionOutcome {
                chat_id: Some(chat_id),
            })
        }
    }
}

fn show_phone_notice(window: &Entity<Workspace>, what: String, cx: &mut App) {
    show_step_in_notice(window, format!("From your phone: {what}"), cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_feed::test_support::{
        allow_always_and_once_options, is_waiting, thread_waiting_on,
    };
    use acp_thread::{
        AuthorizationKind, PermissionOptionChoice, PermissionOptions, RequestPermissionOutcome,
        SelectedPermissionOutcome,
    };
    use gpui::TestAppContext;

    #[gpui::test]
    async fn phone_can_answer_with_an_always_option(cx: &mut TestAppContext) {
        let (thread, _answer) = thread_waiting_on(
            allow_always_and_once_options(),
            AuthorizationKind::PermissionGrant,
            cx,
        )
        .await;

        let answered = cx.update(|cx| {
            answer_permission(&thread, "run-tests", "always", PermissionScope::All, cx)
        });

        assert_eq!(
            answered,
            Ok((
                "Run the tests".to_string(),
                acp::PermissionOptionKind::AllowAlways
            ))
        );
        assert!(!is_waiting(&thread, cx));
    }

    #[gpui::test]
    async fn answer_after_the_mac_answered_is_nothing_to_do(cx: &mut TestAppContext) {
        let (thread, _answer) = thread_waiting_on(
            allow_always_and_once_options(),
            AuthorizationKind::PermissionGrant,
            cx,
        )
        .await;
        thread.update(cx, |thread, cx| {
            thread.authorize_tool_call(
                acp::ToolCallId::new("run-tests"),
                SelectedPermissionOutcome::new(
                    acp::PermissionOptionId::new("allow"),
                    acp::PermissionOptionKind::AllowOnce,
                ),
                cx,
            )
        });

        let answered = cx.update(|cx| {
            answer_permission(&thread, "run-tests", "always", PermissionScope::All, cx)
        });

        assert!(
            matches!(answered, Err(TeamActionError::NothingToDo(_))),
            "answering again gave {answered:?}"
        );
    }

    #[gpui::test]
    async fn an_always_answer_on_a_dropdown_takes_the_tool_wide_choice(cx: &mut TestAppContext) {
        let choice = |allow: (&str, &str, acp::PermissionOptionKind),
                      deny: (&str, &str, acp::PermissionOptionKind),
                      sub_patterns: Vec<String>| PermissionOptionChoice {
            allow: acp::PermissionOption::new(allow.0.to_string(), allow.1.to_string(), allow.2),
            deny: acp::PermissionOption::new(deny.0.to_string(), deny.1.to_string(), deny.2),
            sub_patterns,
        };
        let always = acp::PermissionOptionKind::AllowAlways;
        let never = acp::PermissionOptionKind::RejectAlways;
        let options = PermissionOptions::Dropdown(vec![
            choice(
                ("always_allow:terminal", "Always for terminal", always),
                ("always_deny:terminal", "Always for terminal", never),
                Vec::new(),
            ),
            choice(
                (
                    "always_allow:terminal",
                    "Always for `cargo build` commands",
                    always,
                ),
                (
                    "always_deny:terminal",
                    "Always for `cargo build` commands",
                    never,
                ),
                vec!["^cargo\\s+build(\\s|$)".to_string()],
            ),
            choice(
                (
                    "allow",
                    "Only this time",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                (
                    "deny",
                    "Only this time",
                    acp::PermissionOptionKind::RejectOnce,
                ),
                Vec::new(),
            ),
        ]);
        let (thread, answer) =
            thread_waiting_on(options, AuthorizationKind::PermissionGrant, cx).await;

        let answered = cx.update(|cx| {
            answer_permission(
                &thread,
                "run-tests",
                "always_allow:terminal",
                PermissionScope::All,
                cx,
            )
        });
        assert!(answered.is_ok(), "answering gave {answered:?}");

        let RequestPermissionOutcome::Selected(outcome) = answer.await else {
            panic!("the request was cancelled instead of answered");
        };
        assert_eq!(outcome.option_kind, always);
        assert!(
            outcome.params.is_none(),
            "the phone shows the tool-wide choice, so no command pattern may be attached: {:?}",
            outcome.params
        );
    }

    #[test]
    fn new_chat_names_pick_their_agent() {
        assert_eq!(chat_agent("claude"), Some(ChatAgent::Claude));
        assert_eq!(chat_agent("codex"), Some(ChatAgent::Codex));
        assert_eq!(chat_agent("OpenCode"), Some(ChatAgent::OpenCode));
        assert_eq!(chat_agent("codex-acp"), Some(ChatAgent::Codex));
        assert_eq!(chat_agent("someone-else"), None);
    }

    #[test]
    fn restarts_on_a_new_tailscale_address() {
        let running = TailscaleSelf {
            ip: Ipv4Addr::new(100, 64, 0, 1),
            login: "jay@example.com".into(),
            display_name: "Jay".into(),
            dns_name: "jay-mbp.tail1234.ts.net".into(),
        };
        assert!(!PhoneShare::needs_restart(&running, &running.clone()));
        let moved = TailscaleSelf {
            ip: Ipv4Addr::new(100, 64, 0, 2),
            ..running.clone()
        };
        assert!(PhoneShare::needs_restart(&running, &moved));
        let switched_account = TailscaleSelf {
            login: "someone@example.com".into(),
            ..running.clone()
        };
        assert!(PhoneShare::needs_restart(&running, &switched_account));
    }
}
