//! Serving the phone page: the owner's phone, over Tailscale, follows every
//! workspace's chats and acts on them. Runs while `phone.enabled` is on,
//! whether or not anything is shared with teammates.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use acp_thread::ThreadStatus;
use agent_client_protocol::schema as acp;
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use futures::future::{self, Either};
use gpui::{AnyWindowHandle, App, AppContext as _, Context, Entity, Global, Subscription, Task};
use mnemonic_core::models::{DiscoveredRepository, WorkspaceDetail, WorkspacePrStatus};
use mnemonic_core::repositories::{
    push_device_repository, push_subscription_repository, repository_repository,
    settings_repository, workspace_repository,
};
use mnemonic_core::services::fcm_service::{self, AccessTokenCache, FcmOutcome, ServiceAccount};
use mnemonic_core::services::ntfy_service::{
    self, Notice, NoticeKind, NtfyRequest, mac_idle_seconds, notice_body, ntfy_request,
    should_notify,
};
use mnemonic_core::services::phone_server::{
    PhoneAction, PhoneActionHandler, PhoneActionOutcome, PhoneAgent, PhoneChatOption,
    PhoneChatOptionKind, PhoneChatOptionValue, PhoneOptionChoice, PhonePrStatus, PhoneRepository,
    PhoneServer, PhoneServerConfig, PhoneSources,
};
use mnemonic_core::services::pr_draft_service;
use mnemonic_core::services::team_files_service::TEAM_COPY_SOURCE;
use mnemonic_core::services::team_share_service::{
    DEFAULT_TEAM_PORT, TailscaleSelf, TailscaleWhois, TeamActionError, TeamShareHub, TlsFiles,
    tailscale_cert, tailscale_self,
};
use mnemonic_core::services::web_push_service::{
    self, PushDevice, PushOutcome, PushSubscription, load_or_create_vapid_keys,
};
use settings::{RegisterSetting, Settings, SettingsStore};
use workspace::Workspace;

use crate::WorkspacePanel;
use crate::chat_feed::{
    ChatFeed, ChatFeedEvent, PermissionScope, QuickAnswers, SharedRecords, TranscriptToRead,
    WorkspaceFlags, load_records, read_saved_transcripts,
};
use crate::team::{answer_permission, show_step_in_notice, wait_for_reply, workspace_diff_source};
use crate::workspace_detail_panel::request_pr;
use crate::workspace_view::{
    ChatAgent, OpenChat, WorkspaceHost, WorkspaceView, config_option_values, find_open_chat,
    find_workspace_window, open_chat_options, open_window_handles,
};

const DEFAULT_NOTIFY_AFTER_IDLE_MINUTES: u32 = 5;
/// How often what the phone sees is refreshed.
const PUBLISH_INTERVAL: Duration = Duration::from_secs(2);
/// How long to wait before trying to start the server again, as when
/// Tailscale wasn't connected yet.
const RETRY_START_AFTER: Duration = Duration::from_secs(30);
/// How often the running server checks Tailscale is still on the address
/// and account it started with.
const CHECK_TAILSCALE_EVERY: Duration = Duration::from_secs(15);
/// How often the running server re-runs `tailscale cert`, which renews the
/// certificate by itself once it nears expiry.
const CHECK_CERTIFICATE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// An alert repeated within this long is sent once. See `AlertKey`.
const NOTIFY_COLLAPSE_WITHIN: Duration = Duration::from_secs(30);
/// How long the phone's request waits for the app to carry out its action.
const ACTION_TIMEOUT: Duration = Duration::from_secs(10);
/// The same for starting a task, which makes a worktree, and merging, which
/// waits on GitHub.
const LONG_ACTION_TIMEOUT: Duration = Duration::from_secs(60);
const NO_WINDOW: &str = "Open Mnemonic on the Mac first";

/// The `phone` section of the settings file.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub(crate) struct PhoneSettings {
    pub enabled: bool,
    pub port: u16,
    pub ntfy_url: String,
    pub ntfy_token: Option<String>,
    pub notify_after_idle_minutes: u32,
    pub fcm_service_account: Option<PathBuf>,
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
            fcm_service_account: phone
                .fcm_service_account
                .map(|path| path.trim().to_string())
                .filter(|path| !path.is_empty())
                .map(|path| match path.strip_prefix('~') {
                    Some(rest) => util::paths::home_dir().join(rest.trim_start_matches('/')),
                    None => PathBuf::from(path),
                }),
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

/// What the Team window says about app alerts, kept here so rendering never
/// has to touch the database.
#[derive(Clone, Debug, Default, PartialEq)]
struct AppAlertStatus {
    has_devices: bool,
    key_error: Option<String>,
    /// Why the last Google sign-in failed, until one succeeds.
    sign_in_error: Option<String>,
    /// Why Firebase refused every send in the last attempt, until one is delivered.
    send_error: Option<String>,
}

impl AppAlertStatus {
    fn error(&self) -> Option<String> {
        self.key_error
            .clone()
            .or_else(|| self.sign_in_error.clone())
            .or_else(|| self.send_error.clone())
            .filter(|_| self.has_devices)
    }

    /// A device Firebase dropped counts as neither delivered nor failed.
    fn record_sends(&mut self, delivered: usize, errors: &[String]) {
        if delivered > 0 {
            self.send_error = None;
        } else if let Some(first) = errors.first() {
            self.send_error = Some(first.clone());
        }
    }
}

/// State the server's request threads and the alert sender share, since
/// neither can read the settings or the app's entities.
struct AppAlerts {
    key_path: Mutex<Option<PathBuf>>,
    page_url: Mutex<Option<String>>,
    status: Mutex<AppAlertStatus>,
    changed: mpsc::UnboundedSender<()>,
}

impl AppAlerts {
    fn refresh_status(&self) {
        let has_devices = match mnemonic_bridge::mnemonic_state_cloned() {
            Some(state) => match push_device_repository::list(&state.db) {
                Ok(devices) => !devices.is_empty(),
                Err(err) => {
                    log::warn!("Couldn't list the app devices: {err}");
                    false
                }
            },
            None => false,
        };
        let key_path = lock(&self.key_path).clone();
        let key_error = fcm_service::fcm_status(key_path.as_deref()).err();
        {
            let mut status = lock(&self.status);
            status.has_devices = has_devices;
            status.key_error = key_error;
        }
        // The receiver only goes away when the app is quitting.
        self.changed.unbounded_send(()).ok();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) struct PhoneShare {
    /// The port the server runs, or is starting, on. None while it is off.
    port: Option<u16>,
    server: Option<PhoneServer>,
    /// The page's address, once the server is running.
    page_url: Option<String>,
    /// Why the server last failed to start, until it starts or is turned off.
    start_error: Option<String>,
    /// Why the running server serves plain HTTP rather than HTTPS.
    https_error: Option<String>,
    /// Publishes every workspace and its chats into the hub the server
    /// serves. Kept for as long as the app runs, so its events can be
    /// followed across restarts of the server.
    pub(crate) feed: Entity<ChatFeed>,
    start_task: Option<Task<()>>,
    publish_task: Option<Task<()>>,
    /// Restarts the server when this Mac's Tailscale address or account
    /// changes, since it stays bound to the address it started on, or when
    /// its HTTPS certificate is renewed.
    tailscale_watch: Option<Task<()>>,
    /// Carries out the phone's actions on the main thread.
    action_task: Option<Task<()>>,
    /// When each alert last went out.
    last_notified: HashMap<AlertKey, Instant>,
    /// What the phone may choose, kept here for the server's request
    /// threads, which can't read the settings or the chats.
    choices: Arc<Mutex<PhoneChoices>>,
    /// Each agent's model and effort, as its chats last offered them, so a
    /// new chat can be started with them before any is connected. Loaded
    /// from the database on first use.
    agent_options: Option<HashMap<String, Vec<PhoneChatOption>>>,
    app_alerts: Arc<AppAlerts>,
    _app_alerts_task: Task<()>,
    _feed_subscription: Subscription,
}

struct GlobalPhoneShare(Entity<PhoneShare>);

impl Global for GlobalPhoneShare {}

pub(crate) fn init(cx: &mut App) {
    let phone = cx.new(|cx| {
        let mut phone = PhoneShare::new(cx);
        phone.apply_settings(cx);
        cx.on_app_quit(|phone, _| {
            phone.stop();
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

/// Why the phone server couldn't start, while it is still trying.
pub(crate) fn phone_start_error(cx: &App) -> Option<String> {
    phone_share(cx)?.read(cx).start_error.clone()
}

/// Why the running phone server couldn't get an HTTPS certificate.
pub(crate) fn phone_https_error(cx: &App) -> Option<String> {
    phone_share(cx)?.read(cx).https_error.clone()
}

/// Why app alerts can't be sent, when an app is registered to get them.
pub(crate) fn phone_app_alerts_error(cx: &App) -> Option<String> {
    lock(&phone_share(cx)?.read(cx).app_alerts.status).error()
}

impl PhoneShare {
    fn new(cx: &mut Context<Self>) -> Self {
        let feed = cx.new(|_| ChatFeed::new(TeamShareHub::new(), PermissionScope::All));
        let feed_subscription = cx.subscribe(&feed, |this, _, event: &ChatFeedEvent, cx| {
            this.notify_idle_phone(event, cx);
        });
        let (changed, mut changed_receiver) = mpsc::unbounded::<()>();
        let app_alerts_task = cx.spawn(async move |this, cx| {
            while changed_receiver.next().await.is_some() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self {
            port: None,
            server: None,
            page_url: None,
            start_error: None,
            https_error: None,
            feed,
            start_task: None,
            publish_task: None,
            tailscale_watch: None,
            action_task: None,
            last_notified: HashMap::new(),
            choices: Arc::default(),
            agent_options: None,
            app_alerts: Arc::new(AppAlerts {
                key_path: Mutex::new(None),
                page_url: Mutex::new(None),
                status: Mutex::new(AppAlertStatus::default()),
                changed,
            }),
            _app_alerts_task: app_alerts_task,
            _feed_subscription: feed_subscription,
        }
    }

    /// Sends an ntfy alert for what happened in a chat, when the Mac has
    /// been left alone long enough that nobody is watching it.
    fn notify_idle_phone(&mut self, event: &ChatFeedEvent, cx: &mut Context<Self>) {
        let settings = PhoneSettings::get_global(cx);
        let Some(page_url) = self.page_url.clone() else {
            return;
        };
        let mut permission = None;
        let (workspace_id, notice, stop_reason) = match event {
            ChatFeedEvent::PermissionRequested {
                workspace_id,
                chat_id,
                tool_kind,
                tool_call_id,
                quick_answers,
            } => {
                permission = quick_answers.as_ref().map(|answers| PushPermission {
                    tool_call_id: tool_call_id.clone(),
                    answers: answers.clone(),
                });
                (
                    workspace_id,
                    (
                        NoticeKind::Permission,
                        chat_id,
                        String::new(),
                        Some(*tool_kind),
                    ),
                    None,
                )
            }
            ChatFeedEvent::TurnEnded {
                workspace_id,
                chat_id,
                chat_title,
                stop_reason,
            } => (
                workspace_id,
                (NoticeKind::Finished, chat_id, chat_title.clone(), None),
                Some(*stop_reason),
            ),
            ChatFeedEvent::Errored {
                workspace_id,
                chat_id,
                chat_title,
            } => (
                workspace_id,
                (NoticeKind::Error, chat_id, chat_title.clone(), None),
                None,
            ),
        };
        let (kind, chat_id, chat_title, tool_kind) = notice;
        let (push_title, workspace_name) = self
            .feed
            .read(cx)
            .hub()
            .workspaces()
            .into_iter()
            .find(|workspace| &workspace.id == workspace_id)
            .map(|workspace| {
                (
                    format!("{} / {}", workspace.project, workspace.name),
                    workspace.name,
                )
            })
            .unwrap_or_else(|| (workspace_id.clone(), workspace_id.clone()));
        let notice = Notice {
            kind,
            workspace_name,
            chat_id: chat_id.clone(),
            chat_title,
            tool_kind,
        };
        let key = alert_key(event);
        let ntfy_url = Some(settings.ntfy_url.clone()).filter(|url| !url.is_empty());
        let token = settings.ntfy_token.clone();
        let threshold_minutes = settings.notify_after_idle_minutes;
        let app_alerts = self.app_alerts.clone();
        cx.spawn(async move |this, cx| {
            let idle_seconds = cx.background_spawn(async move { mac_idle_seconds() }).await;
            if !should_notify(&notice.kind, idle_seconds, threshold_minutes, stop_reason) {
                return;
            }
            let Ok(collapsed) = this.update(cx, |this, _| {
                collapse_repeat(&mut this.last_notified, key, Instant::now())
            }) else {
                return;
            };
            if collapsed {
                return;
            }
            let payload = push_payload(&push_title, &notice, &page_url, permission.as_ref());
            let request: Option<NtfyRequest> = ntfy_url
                .map(|ntfy_url| ntfy_request(&ntfy_url, token.as_deref(), &page_url, &notice));
            cx.background_spawn(async move {
                if let Some(request) = request {
                    if let Err(err) = ntfy_service::send(&request) {
                        log::warn!("Couldn't send the phone alert: {err}");
                    }
                }
                send_web_push(&payload);
                send_app_push(&payload, &app_alerts);
            })
            .await;
        })
        .detach();
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let settings = PhoneSettings::get_global(cx);
        let port = settings.enabled.then_some(settings.port);
        let key_path = settings.fcm_service_account.clone();
        if *lock(&self.app_alerts.key_path) != key_path {
            *lock(&self.app_alerts.key_path) = key_path;
            lock(&self.app_alerts.status).sign_in_error = None;
            let app_alerts = self.app_alerts.clone();
            cx.background_spawn(async move { app_alerts.refresh_status() })
                .detach();
        }
        if port == self.port {
            return;
        }
        match port {
            Some(port) => self.start(port, cx),
            None => {
                self.stop();
                self.feed.update(cx, |feed, _| feed.clear());
            }
        }
        cx.notify();
    }

    /// Whether Tailscale moved since the server started: the server is bound
    /// to the old address, or lets in the old account.
    fn needs_restart(running: &TailscaleSelf, now: &TailscaleSelf) -> bool {
        running.ip != now.ip || running.login != now.login
    }

    fn start(&mut self, port: u16, cx: &mut Context<Self>) {
        self.stop();
        self.port = Some(port);
        let hub = self.feed.read(cx).hub().clone();
        let choices = self.choices.clone();
        let app_alerts = self.app_alerts.clone();
        let (action_sender, mut action_receiver) = mpsc::unbounded::<PendingPhoneAction>();
        self.action_task = Some(cx.spawn(async move |_, cx| {
            while let Some(pending) = action_receiver.next().await {
                if pending.claimed.swap(true, Ordering::SeqCst) {
                    log::info!("Skipped a phone action that timed out while waiting");
                    continue;
                }
                let PendingPhoneAction { action, reply, .. } = pending;
                let timeout = action_timeout(&action);
                let on_timeout = timed_out(&action);
                let outcome = cx.update(|cx| {
                    within_timeout(run_phone_action(action, cx), timeout, on_timeout, cx)
                });
                // Awaited on its own, so a task being started or a merge
                // doesn't hold up a Stop or a permission answer behind it.
                cx.spawn(async move |_| {
                    if reply.send(outcome.await).is_err() {
                        log::warn!("A phone action finished after its request gave up");
                    }
                })
                .detach();
            }
        }));
        self.start_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let attempt = cx
                    .background_spawn({
                        let hub = hub.clone();
                        let choices = choices.clone();
                        let action_sender = action_sender.clone();
                        let app_alerts = app_alerts.clone();
                        async move { start_server(port, hub, choices, app_alerts, action_sender) }
                    })
                    .await;
                let started = this.update(cx, |this, cx| {
                    if this.port != Some(port) {
                        return true;
                    }
                    let started = match attempt {
                        Ok(StartedServer {
                            server,
                            started_as,
                            certificate,
                            https_error,
                        }) => {
                            let page_url = page_url(
                                &started_as,
                                server.address().port(),
                                certificate.is_some(),
                            );
                            *lock(&this.app_alerts.page_url) = Some(page_url.clone());
                            this.page_url = Some(page_url);
                            let app_alerts = this.app_alerts.clone();
                            cx.background_spawn(async move { app_alerts.refresh_status() })
                                .detach();
                            this.start_error = None;
                            this.https_error = https_error;
                            this.server = Some(server);
                            this.start_publishing(cx);
                            this.tailscale_watch =
                                Some(Self::watch_tailscale(port, started_as, certificate, cx));
                            true
                        }
                        Err(err) => {
                            log::warn!("Couldn't start the phone server: {err}");
                            this.start_error = Some(err.to_string());
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

    fn watch_tailscale(
        port: u16,
        started_as: TailscaleSelf,
        certificate: Option<Vec<u8>>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let mut certificate_checked = SystemTime::now();
            loop {
                cx.background_executor().timer(CHECK_TAILSCALE_EVERY).await;
                let now = cx.background_spawn(async move { tailscale_self() }).await;
                let moved = match &now {
                    Ok(now) => Self::needs_restart(&started_as, now),
                    Err(_) => true,
                };
                // Wall-clock time, since a Mac asleep overnight doesn't count
                // that time on a monotonic clock.
                let certificate_due = certificate_checked
                    .elapsed()
                    .map_or(true, |elapsed| elapsed >= CHECK_CERTIFICATE_EVERY);
                let reason = if moved {
                    "Tailscale changed"
                } else if certificate_due {
                    certificate_checked = SystemTime::now();
                    let me = started_as.clone();
                    let fetched = cx
                        .background_spawn(async move {
                            https_certificate(
                                &me,
                                certificate_directory().as_deref(),
                                tailscale_cert,
                            )
                        })
                        .await;
                    if !certificate_changed(certificate.as_deref(), &fetched) {
                        continue;
                    }
                    "The HTTPS certificate changed"
                } else {
                    continue;
                };
                this.update(cx, |this, cx| {
                    if this.port == Some(port) {
                        log::info!(
                            "{reason} since the phone server started on {}; restarting it",
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

    /// Keeps the feed, so a restart for a renewed certificate or a Tailscale
    /// change doesn't lose which turns ended and when.
    fn stop(&mut self) {
        self.start_task = None;
        self.publish_task = None;
        self.tailscale_watch = None;
        self.action_task = None;
        self.server = None;
        self.port = None;
        self.page_url = None;
        *lock(&self.app_alerts.page_url) = None;
        self.start_error = None;
        self.https_error = None;
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
        self.publish_choices(cx);
        self.feed.update(cx, |feed, cx| {
            feed.publish(records, &|_| WorkspaceFlags::default(), cx)
        })
    }
}

impl PhoneShare {
    fn publish_choices(&mut self, cx: &App) {
        let agent_options = self.agent_options.get_or_insert_with(load_agent_options);
        let mut chat_options = HashMap::new();
        let mut agent_options_changed = false;
        for (record_id, agent, options) in open_chat_options(cx) {
            let options = phone_chat_options(&options);
            if options.is_empty() {
                continue;
            }
            let offered: Vec<PhoneChatOption> = options
                .iter()
                .map(|option| PhoneChatOption {
                    current: None,
                    ..option.clone()
                })
                .collect();
            let known = agent_options.get(agent.agent_id());
            if known != Some(&offered) {
                agent_options.insert(agent.agent_id().to_string(), offered);
                agent_options_changed = true;
            }
            chat_options.insert(record_id, options);
        }
        if agent_options_changed {
            save_agent_options(agent_options.clone(), cx);
        }
        let agents = ChatAgent::enabled(cx)
            .into_iter()
            .map(|agent| PhoneAgent {
                id: agent.agent_id().to_string(),
                label: agent.label().to_string(),
                options: agent_options
                    .get(agent.agent_id())
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect();
        *lock(&self.choices) = PhoneChoices {
            agents,
            chat_options,
        };
    }
}

/// What the phone may choose: the agents for new chats, and each connected
/// chat's model and effort.
#[derive(Default)]
struct PhoneChoices {
    agents: Vec<PhoneAgent>,
    chat_options: HashMap<String, Vec<PhoneChatOption>>,
}

const AGENT_OPTIONS_KEY: &str = "phone_agent_options";

fn load_agent_options() -> HashMap<String, Vec<PhoneChatOption>> {
    let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
        return HashMap::new();
    };
    match settings_repository::get_value(&state.db, AGENT_OPTIONS_KEY) {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_else(|err| {
            log::error!("Couldn't read {AGENT_OPTIONS_KEY}: {err}");
            HashMap::new()
        }),
        Ok(None) => HashMap::new(),
        Err(err) => {
            log::error!("Couldn't read {AGENT_OPTIONS_KEY}: {err}");
            HashMap::new()
        }
    }
}

fn save_agent_options(options: HashMap<String, Vec<PhoneChatOption>>, cx: &App) {
    let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
        return;
    };
    cx.background_spawn(async move {
        let saved = serde_json::to_string(&options)
            .map_err(|err| err.to_string())
            .and_then(|json| settings_repository::set_value(&state.db, AGENT_OPTIONS_KEY, &json));
        if let Err(err) = saved {
            log::error!("Couldn't save {AGENT_OPTIONS_KEY}: {err}");
        }
    })
    .detach();
}

/// The chat's model and effort, the only options the phone changes.
fn phone_chat_options(options: &[acp::SessionConfigOption]) -> Vec<PhoneChatOption> {
    options
        .iter()
        .filter_map(|option| {
            let kind = match option.category.as_ref()? {
                acp::SessionConfigOptionCategory::Model => PhoneChatOptionKind::Model,
                acp::SessionConfigOptionCategory::ThoughtLevel => PhoneChatOptionKind::Effort,
                _ => return None,
            };
            let acp::SessionConfigKind::Select(select) = &option.kind else {
                return None;
            };
            Some(PhoneChatOption {
                id: option.id.0.to_string(),
                name: option.name.clone(),
                kind,
                current: Some(select.current_value.0.to_string()),
                values: config_option_values(option)
                    .into_iter()
                    .map(|value| PhoneChatOptionValue {
                        id: value.value.0.to_string(),
                        name: value.name.clone(),
                    })
                    .collect(),
            })
        })
        .collect()
}

fn page_url(me: &TailscaleSelf, port: u16, https: bool) -> String {
    let host = if me.dns_name.is_empty() {
        me.ip.to_string()
    } else {
        me.dns_name.clone()
    };
    let scheme = if https { "https" } else { "http" };
    format!("{scheme}://{host}:{port}/")
}

fn certificate_directory() -> Option<PathBuf> {
    mnemonic_bridge::mnemonic_state_cloned().map(|state| state.data_dir.join("phone"))
}

/// The certificate to serve the page over HTTPS with, or why it can't be.
fn https_certificate(
    me: &TailscaleSelf,
    directory: Option<&Path>,
    fetch: impl FnOnce(&str, &Path) -> Result<TlsFiles, String>,
) -> Result<TlsFiles, String> {
    if me.dns_name.is_empty() {
        return Err("MagicDNS is off in this tailnet, so there's no name to certify".into());
    }
    let directory = directory.ok_or_else(|| "Mnemonic's data folder isn't ready".to_string())?;
    fetch(&me.dns_name, directory)
}

/// Whether a renewal check found a certificate other than the one being
/// served. A failed check keeps what is running.
fn certificate_changed(running: Option<&[u8]>, fetched: &Result<TlsFiles, String>) -> bool {
    match fetched {
        Ok(files) => running != Some(files.certificate.as_slice()),
        Err(err) => {
            log::warn!("Couldn't renew the phone page's HTTPS certificate: {err}");
            false
        }
    }
}

struct StartedServer {
    server: PhoneServer,
    started_as: TailscaleSelf,
    /// The certificate served, None when serving plain HTTP.
    certificate: Option<Vec<u8>>,
    /// Why plain HTTP.
    https_error: Option<String>,
}

fn start_server(
    port: u16,
    hub: TeamShareHub,
    choices: Arc<Mutex<PhoneChoices>>,
    app_alerts: Arc<AppAlerts>,
    action_sender: mpsc::UnboundedSender<PendingPhoneAction>,
) -> Result<StartedServer, String> {
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
        let timeout = action_timeout(&action);
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
        wait_for_reply(&reply_receiver, &claimed, timeout)
    });
    let start = |tls: Option<TlsFiles>| {
        let agent_choices = choices.clone();
        let chat_choices = choices.clone();
        let app_alerts = app_alerts.clone();
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
                tls,
            },
            hub.clone(),
            Arc::new(TailscaleWhois::default()),
            PhoneSources {
                diff: workspace_diff_source(),
                agents: Arc::new(move || lock(&agent_choices).agents.clone()),
                chat_options: Arc::new(move |chat_id| {
                    lock(&chat_choices).chat_options.get(chat_id).cloned()
                }),
                repositories: Arc::new(phone_repositories),
                pr_status: Arc::new(phone_pr_status),
                push_key: Arc::new(phone_push_key),
                save_push_subscription: Arc::new(save_push_subscription),
                save_push_device: Arc::new({
                    let app_alerts = app_alerts.clone();
                    move |device| {
                        save_push_device(device)?;
                        app_alerts.refresh_status();
                        Ok(())
                    }
                }),
                send_test_alert: Arc::new(move || send_test_alert(&app_alerts)),
            },
            action_handler.clone(),
        )
    };
    let served_over_https =
        https_certificate(&me, certificate_directory().as_deref(), tailscale_cert).and_then(
            |tls| {
                let certificate = tls.certificate.clone();
                start(Some(tls)).map(|server| (server, certificate))
            },
        );
    let (server, certificate, https_error) = match served_over_https {
        Ok((server, certificate)) => (server, Some(certificate), None),
        Err(err) => {
            log::warn!("Serving the phone page over plain HTTP: {err}");
            (start(None)?, None, Some(err))
        }
    };
    Ok(StartedServer {
        server,
        started_as: me,
        certificate,
        https_error,
    })
}

/// What a repeated alert is matched on. Permission prompts match by their
/// tool call, so the agent's next prompt after one answered from the
/// notification still alerts; a chat's finishes and errors match by kind.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AlertKey {
    chat_id: String,
    kind: NoticeKind,
    tool_call_id: Option<String>,
}

fn alert_key(event: &ChatFeedEvent) -> AlertKey {
    match event {
        ChatFeedEvent::PermissionRequested {
            chat_id,
            tool_call_id,
            ..
        } => AlertKey {
            chat_id: chat_id.clone(),
            kind: NoticeKind::Permission,
            tool_call_id: Some(tool_call_id.clone()),
        },
        ChatFeedEvent::TurnEnded { chat_id, .. } => AlertKey {
            chat_id: chat_id.clone(),
            kind: NoticeKind::Finished,
            tool_call_id: None,
        },
        ChatFeedEvent::Errored { chat_id, .. } => AlertKey {
            chat_id: chat_id.clone(),
            kind: NoticeKind::Error,
            tool_call_id: None,
        },
    }
}

/// Whether an alert matching `key` went out within `NOTIFY_COLLAPSE_WITHIN`
/// of `now`. When it didn't, records it as going out now.
fn collapse_repeat(
    last_notified: &mut HashMap<AlertKey, Instant>,
    key: AlertKey,
    now: Instant,
) -> bool {
    let collapsed = last_notified
        .get(&key)
        .is_some_and(|sent| now.duration_since(*sent) < NOTIFY_COLLAPSE_WITHIN);
    if !collapsed {
        last_notified.retain(|_, sent| now.duration_since(*sent) < NOTIFY_COLLAPSE_WITHIN);
        last_notified.insert(key, now);
    }
    collapsed
}

struct PushPermission {
    tool_call_id: String,
    answers: QuickAnswers,
}

/// Like the ntfy alert, never carries a command, tool input, message or
/// diff: only what kind of thing happened and where.
fn push_payload(
    title: &str,
    notice: &Notice,
    page_url: &str,
    permission: Option<&PushPermission>,
) -> String {
    serde_json::json!({
        "title": title,
        "body": notice_body(notice),
        "chat_id": notice.chat_id,
        "url": format!("{page_url}#chat={}", notice.chat_id),
        "permission": permission.map(|permission| serde_json::json!({
            "tool_call_id": permission.tool_call_id,
            "allow_option_id": permission.answers.allow_option_id,
            "deny_option_id": permission.answers.deny_option_id,
        })),
    })
    .to_string()
}

fn send_web_push(payload: &str) {
    if let Err(err) = try_send_web_push(payload) {
        log::warn!("Couldn't send the phone's web push: {err}");
    }
}

fn try_send_web_push(payload: &str) -> Result<(), String> {
    let state = mnemonic_bridge::mnemonic_state_cloned()
        .ok_or_else(|| "Mnemonic isn't ready".to_string())?;
    let subscriptions = push_subscription_repository::list(&state.db)?;
    if subscriptions.is_empty() {
        return Ok(());
    }
    let keys = load_or_create_vapid_keys(&state.data_dir.join("phone"))?;
    for subscription in subscriptions {
        match web_push_service::send(&subscription, payload.as_bytes(), &keys) {
            Ok(PushOutcome::Delivered) => {}
            Ok(PushOutcome::Gone) => {
                if let Err(err) =
                    push_subscription_repository::delete(&state.db, &subscription.endpoint)
                {
                    log::warn!("Couldn't forget a push subscription the browser dropped: {err}");
                }
            }
            Err(err) => log::warn!("Couldn't send a web push: {err}"),
        }
    }
    Ok(())
}

fn send_app_push(payload: &str, app_alerts: &AppAlerts) {
    if let Err(err) = try_send_app_push(payload, app_alerts) {
        log::warn!("Couldn't send the phone's app alert: {err}");
    }
}

/// Tries every device even when one fails, and reports what went wrong.
fn try_send_app_push(payload: &str, app_alerts: &AppAlerts) -> Result<(), String> {
    let result = send_to_devices(payload, app_alerts);
    app_alerts.refresh_status();
    result
}

fn send_to_devices(payload: &str, app_alerts: &AppAlerts) -> Result<(), String> {
    let state = mnemonic_bridge::mnemonic_state_cloned()
        .ok_or_else(|| "Mnemonic isn't ready".to_string())?;
    let devices = push_device_repository::list(&state.db)?;
    if devices.is_empty() {
        return Ok(());
    }
    let key_path = lock(&app_alerts.key_path).clone();
    let key_path = key_path.ok_or_else(|| "No Firebase key file is set".to_string())?;
    let account = ServiceAccount::load(&key_path)?;
    let access_token = match AccessTokenCache::shared().token(&account) {
        Ok(access_token) => {
            lock(&app_alerts.status).sign_in_error = None;
            access_token
        }
        Err(err) => {
            lock(&app_alerts.status).sign_in_error = Some(err.clone());
            return Err(err);
        }
    };
    let mut failures = Vec::new();
    let mut errors = Vec::new();
    let mut delivered = 0;
    for device in devices {
        match fcm_service::send(&account, &access_token, &device, payload.as_bytes()) {
            Ok(FcmOutcome::Delivered) => delivered += 1,
            Ok(FcmOutcome::Unregistered) => {
                if let Err(err) = push_device_repository::delete(&state.db, &device.install_id) {
                    log::warn!("Couldn't forget an app device Firebase dropped: {err}");
                }
            }
            Err(err) => {
                log::warn!("Couldn't send an app alert to {}: {err}", device.name);
                failures.push(format!("{}: {err}", device.name));
                errors.push(err);
            }
        }
    }
    lock(&app_alerts.status).record_sends(delivered, &errors);
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn phone_push_key() -> Result<String, String> {
    let state = mnemonic_bridge::mnemonic_state_cloned()
        .ok_or_else(|| "Mnemonic isn't ready".to_string())?;
    Ok(load_or_create_vapid_keys(&state.data_dir.join("phone"))?.public_key_base64url())
}

fn save_push_subscription(subscription: PushSubscription) -> Result<(), String> {
    let state = mnemonic_bridge::mnemonic_state_cloned()
        .ok_or_else(|| "Mnemonic isn't ready".to_string())?;
    push_subscription_repository::upsert(&state.db, &subscription)
}

fn save_push_device(device: PushDevice) -> Result<(), String> {
    let state = mnemonic_bridge::mnemonic_state_cloned()
        .ok_or_else(|| "Mnemonic isn't ready".to_string())?;
    push_device_repository::upsert(&state.db, &device)
}

/// Goes to every browser and app regardless of how long the Mac has been
/// idle, so the owner can check alerts reach the phone.
fn send_test_alert(app_alerts: &AppAlerts) -> Result<(), String> {
    let page_url = lock(&app_alerts.page_url)
        .clone()
        .ok_or_else(|| "The phone page isn't running".to_string())?;
    let payload = serde_json::json!({
        "title": "Mnemonic",
        "body": "Test alert from your Mac",
        "chat_id": "",
        "url": page_url,
        "permission": null,
    })
    .to_string();
    let web = try_send_web_push(&payload);
    let app = try_send_app_push(&payload, app_alerts);
    let errors: Vec<String> = [web, app].into_iter().filter_map(Result::err).collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// The repositories, those with the newest workspace first. Workspaces carry
/// no creation date, so their newest-first listing order stands in for it.
fn phone_repositories() -> Result<Vec<PhoneRepository>, String> {
    let state = mnemonic_bridge::mnemonic_state_cloned()
        .ok_or_else(|| "Mnemonic isn't ready".to_string())?;
    let repositories = repository_repository::list(&state.db)?;
    let workspaces = workspace_repository::list(&state.db)?;
    let recency = |repository: &DiscoveredRepository| {
        workspaces.iter().position(|workspace| {
            workspace.repository_id.as_deref() == Some(repository.id.as_str())
        })
    };
    let mut ranked: Vec<(Option<usize>, DiscoveredRepository)> = repositories
        .into_iter()
        .map(|repository| (recency(&repository), repository))
        .collect();
    ranked.sort_by(|(left_rank, left), (right_rank, right)| {
        match (left_rank, right_rank) {
            (Some(left_rank), Some(right_rank)) => left_rank.cmp(right_rank),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| left.name.cmp(&right.name))
    });
    Ok(ranked
        .into_iter()
        .map(|(_, repository)| PhoneRepository {
            id: repository.id,
            name: repository.name,
        })
        .collect())
}

fn phone_pr_status(workspace_id: &str) -> Result<PhonePrStatus, String> {
    let state = mnemonic_bridge::mnemonic_state_cloned()
        .ok_or_else(|| "Mnemonic isn't ready".to_string())?;
    let status = pr_draft_service::get_workspace_pr_status(&state, workspace_id)?;
    Ok(PhonePrStatus {
        can_merge: status.can_merge(),
        found: status.found,
        number: status.number,
        title: status.title,
        url: status.url,
        state: status.state,
        is_draft: status.is_draft,
        merge_state_status: status.merge_state_status,
        head_ref_oid: status.head_ref_oid,
        checks_summary: status.checks_summary,
        merge_method: pr_draft_service::configured_merge_method(&state),
    })
}

fn action_timeout(action: &PhoneAction) -> Duration {
    match action {
        PhoneAction::StartTask { .. } | PhoneAction::Merge { .. } | PhoneAction::Archive { .. } => {
            LONG_ACTION_TIMEOUT
        }
        _ => ACTION_TIMEOUT,
    }
}

/// What the phone hears when an action outlasts its timeout. A task, a
/// merge or a message waiting for the agent to stop is usually still going,
/// so calling it a failure would invite the owner to do it a second time.
fn timed_out(action: &PhoneAction) -> Result<PhoneActionOutcome, TeamActionError> {
    let still_running = |message: &str| {
        Ok(PhoneActionOutcome {
            message: Some(message.to_string()),
            ..PhoneActionOutcome::default()
        })
    };
    match action {
        PhoneAction::StartTask { .. } => {
            still_running("Still starting on the Mac; it will appear in the list shortly")
        }
        PhoneAction::Merge { .. } => {
            still_running("Still merging on the Mac; check the PR in a moment")
        }
        PhoneAction::Archive { .. } => {
            still_running("Still archiving on the Mac; it will leave the list shortly")
        }
        PhoneAction::Message { .. } => {
            still_running("Stopping the agent on the Mac; your message goes out once it stops.")
        }
        _ => Err(TeamActionError::Failed("Timed out".to_string())),
    }
}

/// The action's outcome, or `on_timeout` once it has run for `timeout`, so
/// a hung `git` or `gh` can't hold the phone's request open forever. The
/// action itself carries on, so a half-made workspace isn't abandoned.
fn within_timeout(
    action: Task<Result<PhoneActionOutcome, TeamActionError>>,
    timeout: Duration,
    on_timeout: Result<PhoneActionOutcome, TeamActionError>,
    cx: &mut App,
) -> Task<Result<PhoneActionOutcome, TeamActionError>> {
    let (outcome_sender, outcome_receiver) = oneshot::channel();
    cx.spawn(async move |_| {
        if outcome_sender.send(action.await).is_err() {
            log::info!("A phone action finished after it timed out");
        }
    })
    .detach();
    let timer = cx.background_executor().timer(timeout);
    cx.background_spawn(async move {
        match future::select(outcome_receiver, timer).await {
            Either::Left((Ok(outcome), _)) => outcome,
            Either::Left((Err(oneshot::Canceled), _)) => Err(TeamActionError::Failed(
                "The action stopped before it finished".to_string(),
            )),
            Either::Right(_) => on_timeout,
        }
    })
}

/// The agent the phone named, by its label or its id.
fn chat_agent(name: &str) -> Option<ChatAgent> {
    let name = name.trim();
    ChatAgent::ALL
        .into_iter()
        .find(|agent| agent.label().eq_ignore_ascii_case(name) || agent.agent_id() == name)
}

fn enabled_agent(name: &str, enabled: &[ChatAgent]) -> Result<ChatAgent, TeamActionError> {
    let agent = chat_agent(name)
        .ok_or_else(|| TeamActionError::Invalid(format!("Unknown agent {name}")))?;
    if !enabled.contains(&agent) {
        return Err(TeamActionError::Invalid(format!(
            "{} isn't enabled on this Mac",
            agent.label()
        )));
    }
    Ok(agent)
}

/// The repository and agent a task from the phone starts with.
fn task_to_start(
    repositories: &[DiscoveredRepository],
    enabled: &[ChatAgent],
    repository_id: &str,
    agent: &str,
) -> Result<(DiscoveredRepository, ChatAgent), TeamActionError> {
    let repository = repositories
        .iter()
        .find(|repository| repository.id == repository_id)
        .cloned()
        .ok_or_else(|| TeamActionError::Invalid(format!("Unknown repository {repository_id}")))?;
    Ok((repository, enabled_agent(agent, enabled)?))
}

/// The PR's number when it may be merged at `head`, the commit the phone
/// saw.
fn check_merge(status: &WorkspacePrStatus, head: &str) -> Result<i64, TeamActionError> {
    let cannot_merge =
        || TeamActionError::NothingToDo("This PR can't be merged right now".to_string());
    if !status.can_merge() {
        return Err(cannot_merge());
    }
    if status.head_ref_oid.as_deref() != Some(head) {
        return Err(TeamActionError::NothingToDo(
            "The PR changed since you looked. Refresh and try again.".to_string(),
        ));
    }
    status.number.ok_or_else(cannot_merge)
}

fn no_window() -> TeamActionError {
    TeamActionError::NothingToDo(NO_WINDOW.to_string())
}

fn not_ready() -> TeamActionError {
    TeamActionError::Failed("Mnemonic isn't ready".to_string())
}

/// The frontmost Mnemonic window, or any open one when none is in front.
fn front_window(cx: &App) -> Option<Entity<Workspace>> {
    let windows = open_window_handles(cx);
    let in_front = cx
        .window_stack()
        .unwrap_or_default()
        .into_iter()
        .find_map(|handle| {
            windows
                .iter()
                .find(|(_, window)| window.window_id() == handle.window_id())
                .map(|(workspace, _)| workspace.clone())
        });
    in_front.or_else(|| windows.into_iter().next().map(|(workspace, _)| workspace))
}

fn window_handle_of(workspace: &Entity<Workspace>, cx: &App) -> Option<AnyWindowHandle> {
    open_window_handles(cx)
        .into_iter()
        .find(|(open, _)| open.entity_id() == workspace.entity_id())
        .map(|(_, window)| window)
}

/// The workspace holding a chat, as last published to the phone.
fn saved_chat_workspace(chat_id: &str, cx: &App) -> Option<String> {
    phone_share(cx)?
        .read(cx)
        .feed
        .read(cx)
        .hub()
        .workspaces()
        .into_iter()
        .find(|workspace| workspace.chats.iter().any(|chat| chat.id == chat_id))
        .map(|workspace| workspace.id)
}

/// The workspace's loaded view, opening it in the frontmost window the way
/// selecting it in the workspace list does when no window has it loaded.
fn open_workspace_for_phone(
    workspace_id: &str,
    cx: &mut App,
) -> Result<(Entity<Workspace>, Entity<WorkspaceView>), TeamActionError> {
    let loaded = find_workspace_window(workspace_id, cx).and_then(|window| {
        let view = WorkspaceHost::view_in(window.read(cx), workspace_id, cx)?;
        Some((window, view))
    });
    if let Some(loaded) = loaded {
        return Ok(loaded);
    }
    let window = front_window(cx).ok_or_else(no_window)?;
    let window_handle = window_handle_of(&window, cx).ok_or_else(no_window)?;
    let panel = window
        .read(cx)
        .panel::<WorkspacePanel>(cx)
        .ok_or_else(no_window)?;
    window_handle
        .update(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                // A workspace made moments ago may not be listed yet.
                panel.refresh(cx);
                panel.select_workspace(workspace_id.to_string(), window, cx);
            })
        })
        .map_err(|err| TeamActionError::Failed(format!("Couldn't open the workspace: {err}")))?;
    let view = WorkspaceHost::view_in(window.read(cx), workspace_id, cx).ok_or_else(|| {
        TeamActionError::NothingToDo("This workspace isn't on the Mac any more".to_string())
    })?;
    Ok((window, view))
}

/// Carries out the phone's action. Unlike a teammate's, it needs no
/// step-in switch and may give any answer the agent offers. A workspace not
/// loaded on the Mac is opened first, except to merge its PR.
fn run_phone_action(
    action: PhoneAction,
    cx: &mut App,
) -> Task<Result<PhoneActionOutcome, TeamActionError>> {
    match action {
        PhoneAction::Stop { chat_id } => Task::ready(stop_chat(chat_id, cx)),
        PhoneAction::Permission {
            chat_id,
            tool_call_id,
            option_id,
        } => Task::ready(answer_chat_permission(chat_id, tool_call_id, option_id, cx)),
        PhoneAction::Message { chat_id, text } => send_message(chat_id, text, cx),
        PhoneAction::SetChatOptions { chat_id, choices } => set_chat_options(chat_id, choices, cx),
        PhoneAction::NewChat {
            workspace_id,
            agent,
        } => Task::ready(start_new_chat(workspace_id, agent, cx)),
        PhoneAction::CreatePr { workspace_id } => Task::ready(ask_for_pr(workspace_id, cx)),
        PhoneAction::StartTask {
            repository_id,
            agent,
            text,
            choices,
        } => start_task(repository_id, agent, text, choices, cx),
        PhoneAction::Merge { workspace_id, head } => merge_pr(workspace_id, head, cx),
        PhoneAction::Archive { workspace_id } => archive_workspace(workspace_id, cx),
    }
}

/// Why the phone may not archive a workspace with this source, if it may
/// not. None for the source means the Mac has no such workspace.
fn archive_refusal(source: Option<&str>) -> Option<TeamActionError> {
    match source {
        None => Some(TeamActionError::Invalid("No such workspace".to_string())),
        Some(TEAM_COPY_SOURCE) => Some(TeamActionError::Invalid(
            "Archive a teammate's copy on the Mac".to_string(),
        )),
        Some(_) => None,
    }
}

/// Archives in the workspace's own window, else the frontmost one, as the
/// agent tool does. `check_can_archive`'s reason goes back as the refusal.
fn archive_workspace(
    workspace_id: String,
    cx: &mut App,
) -> Task<Result<PhoneActionOutcome, TeamActionError>> {
    let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
        return Task::ready(Err(not_ready()));
    };
    let workspace = match workspace_repository::list(&state.db) {
        Ok(workspaces) => workspaces
            .into_iter()
            .find(|workspace| workspace.id == workspace_id),
        Err(err) => return Task::ready(Err(TeamActionError::Failed(err))),
    };
    let source = workspace
        .as_ref()
        .map(|workspace| workspace.workspace_source.as_str());
    if let Some(refusal) = archive_refusal(source) {
        return Task::ready(Err(refusal));
    }
    let workspace_name = workspace
        .map(|workspace| workspace.name)
        .unwrap_or_default();
    let Some(fallback_window) = front_window(cx) else {
        return Task::ready(Err(no_window()));
    };
    cx.spawn(async move |cx| {
        crate::agent_hub::archive_workspace_by_id(workspace_id, Some(fallback_window.clone()), cx)
            .await
            .map_err(TeamActionError::Invalid)?;
        cx.update(|cx| {
            show_phone_notice(&fallback_window, format!("archived {workspace_name}"), cx);
        });
        Ok(PhoneActionOutcome::default())
    })
}

fn stop_chat(chat_id: String, cx: &mut App) -> Result<PhoneActionOutcome, TeamActionError> {
    let not_running = || TeamActionError::NothingToDo("The agent isn't running".to_string());
    let chat = find_open_chat(&chat_id, cx).ok_or_else(not_running)?;
    // None while the chat's agent is still connecting.
    let Some(thread) = &chat.thread else {
        return Err(not_running());
    };
    if thread.read(cx).status() == ThreadStatus::Idle {
        return Err(not_running());
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

/// A chat that isn't loaded has no prompt waiting: whatever the phone saw
/// was answered before the chat was closed.
fn answer_chat_permission(
    chat_id: String,
    tool_call_id: String,
    option_id: String,
    cx: &mut App,
) -> Result<PhoneActionOutcome, TeamActionError> {
    let already_answered = || TeamActionError::NothingToDo("Already answered".to_string());
    let chat = find_open_chat(&chat_id, cx).ok_or_else(already_answered)?;
    let Some(thread) = &chat.thread else {
        return Err(already_answered());
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

/// Sends the message at once, stopping the agent's turn if it's working.
fn send_message(
    chat_id: String,
    text: String,
    cx: &mut App,
) -> Task<Result<PhoneActionOutcome, TeamActionError>> {
    let closed = || TeamActionError::NothingToDo("This chat was closed on the Mac".to_string());
    let chat = match open_chat_for_message(&chat_id, cx) {
        Ok(chat) => chat,
        Err(err) => return Task::ready(Err(err)),
    };
    let sending = chat
        .view
        .update(cx, |view, cx| view.send_now_to_chat(&chat_id, text, cx));
    cx.spawn(async move |cx| {
        sending.await.map_err(|_| closed())?;
        cx.update(|cx| {
            show_phone_notice(
                &chat.window,
                format!("sent a message to {}", chat.title),
                cx,
            )
        });
        Ok(PhoneActionOutcome::default())
    })
}

/// The chat, its workspace opened first if it isn't loaded on the Mac.
fn open_chat_for_message(chat_id: &str, cx: &mut App) -> Result<OpenChat, TeamActionError> {
    let closed = || TeamActionError::NothingToDo("This chat was closed on the Mac".to_string());
    if find_open_chat(chat_id, cx).is_none() {
        let workspace_id = saved_chat_workspace(chat_id, cx).ok_or_else(closed)?;
        open_workspace_for_phone(&workspace_id, cx)?;
    }
    find_open_chat(chat_id, cx).ok_or_else(closed)
}

/// Sets the chat's model or effort, opening its workspace first if it
/// isn't loaded on the Mac.
fn set_chat_options(
    chat_id: String,
    choices: Vec<PhoneOptionChoice>,
    cx: &mut App,
) -> Task<Result<PhoneActionOutcome, TeamActionError>> {
    let chat = match open_chat_for_message(&chat_id, cx) {
        Ok(chat) => chat,
        Err(err) => return Task::ready(Err(err)),
    };
    let setting = chat
        .view
        .update(cx, |view, cx| view.set_chat_options(&chat_id, choices, cx));
    cx.spawn(async move |cx| {
        let options = setting.await.map_err(TeamActionError::Invalid)?;
        cx.update(|cx| {
            show_phone_notice(
                &chat.window,
                format!("changed the model or effort in {}", chat.title),
                cx,
            )
        });
        Ok(PhoneActionOutcome {
            options: options.map(|options| phone_chat_options(&options)),
            ..PhoneActionOutcome::default()
        })
    })
}

fn start_new_chat(
    workspace_id: String,
    agent: String,
    cx: &mut App,
) -> Result<PhoneActionOutcome, TeamActionError> {
    let agent = enabled_agent(&agent, &ChatAgent::enabled(cx))?;
    let (window, view) = open_workspace_for_phone(&workspace_id, cx)?;
    let chat_id = view
        .read(cx)
        .window_handle()
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| view.start_chat(agent, window, cx))
        })
        .map_err(|err| TeamActionError::Failed(format!("Couldn't start the chat: {err}")))?;
    show_phone_notice(&window, format!("started a {} chat", agent.label()), cx);
    Ok(PhoneActionOutcome {
        chat_id: Some(chat_id),
        ..PhoneActionOutcome::default()
    })
}

fn ask_for_pr(workspace_id: String, cx: &mut App) -> Result<PhoneActionOutcome, TeamActionError> {
    let (window, _view) = open_workspace_for_phone(&workspace_id, cx)?;
    let state = mnemonic_bridge::mnemonic_state_cloned().ok_or_else(not_ready)?;
    let summary =
        workspace_repository::get(&state.db, &workspace_id).map_err(TeamActionError::Failed)?;
    let sent = window.update(cx, |workspace, cx| {
        request_pr(workspace, &workspace_id, &summary.branch, cx)
    });
    if !sent {
        return Err(TeamActionError::Failed(
            "Couldn't send the PR instruction".to_string(),
        ));
    }
    show_phone_notice(&window, format!("asked for a PR in {}", summary.name), cx);
    Ok(PhoneActionOutcome::default())
}

/// Resolves to the new workspace and its primary chat's record id.
type WorkspaceCreation = Task<Result<(WorkspaceDetail, String), String>>;

/// Starts creating the task's workspace in the frontmost window, returning
/// the creation and the repository's name.
fn create_task_workspace(
    repository_id: &str,
    agent: &str,
    cx: &mut App,
) -> Result<(WorkspaceCreation, String), TeamActionError> {
    let window = front_window(cx).ok_or_else(no_window)?;
    let state = mnemonic_bridge::mnemonic_state_cloned().ok_or_else(not_ready)?;
    let repositories = repository_repository::list(&state.db).map_err(TeamActionError::Failed)?;
    let (repository, agent) =
        task_to_start(&repositories, &ChatAgent::enabled(cx), repository_id, agent)?;
    let window_handle = window_handle_of(&window, cx).ok_or_else(no_window)?;
    let panel = window
        .read(cx)
        .panel::<WorkspacePanel>(cx)
        .ok_or_else(no_window)?;
    let repository_name = repository.name.clone();
    let created = window_handle
        .update(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.create_project_workspace(repository, agent, window, cx)
            })
        })
        .map_err(|err| TeamActionError::Failed(format!("Couldn't start the task: {err}")))?;
    Ok((created, repository_name))
}

/// Creates a workspace for the task, then sends `text` to its chat as the
/// user's own first message, so the workspace is named from it as when
/// typed on the Mac. The message waits in the chat while its agent connects.
fn start_task(
    repository_id: String,
    agent: String,
    text: String,
    choices: Vec<PhoneOptionChoice>,
    cx: &mut App,
) -> Task<Result<PhoneActionOutcome, TeamActionError>> {
    let (created, repository_name) = match create_task_workspace(&repository_id, &agent, cx) {
        Ok(creating) => creating,
        Err(err) => return Task::ready(Err(err)),
    };
    cx.spawn(async move |cx| {
        let (detail, chat_id) = created.await.map_err(TeamActionError::Failed)?;
        let workspace_id = detail.summary.id;
        let (setting, outcome) = cx.update(|cx| {
            let (window, view) = open_workspace_for_phone(&workspace_id, cx)?;
            // Set first, so the message waits for the model and effort.
            let setting = (!choices.is_empty())
                .then(|| view.update(cx, |view, cx| view.set_chat_options(&chat_id, choices, cx)));
            let sent = view.update(cx, |view, cx| view.send_to_chat(&chat_id, text, cx));
            if !sent {
                return Err(TeamActionError::Failed(
                    "The new workspace's chat didn't open".to_string(),
                ));
            }
            show_phone_notice(&window, format!("started a task in {repository_name}"), cx);
            Ok((
                setting,
                PhoneActionOutcome {
                    chat_id: Some(chat_id),
                    workspace_id: Some(workspace_id),
                    ..PhoneActionOutcome::default()
                },
            ))
        })?;
        match setting {
            Some(setting) => match setting.await {
                Ok(_) => Ok(outcome),
                Err(err) => Ok(PhoneActionOutcome {
                    message: Some(format!(
                        "Started, but the model or effort wasn't set: {err}"
                    )),
                    ..outcome
                }),
            },
            None => Ok(outcome),
        }
    })
}

/// Merges without opening the workspace. GitHub refuses the merge if the
/// PR's head moved past `head` since the status was read here.
fn merge_pr(
    workspace_id: String,
    head: String,
    cx: &mut App,
) -> Task<Result<PhoneActionOutcome, TeamActionError>> {
    let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
        return Task::ready(Err(not_ready()));
    };
    cx.spawn(async move |cx| {
        let (number, message) = cx
            .background_spawn({
                let workspace_id = workspace_id.clone();
                async move {
                    let status = pr_draft_service::get_workspace_pr_status(&state, &workspace_id)
                        .map_err(TeamActionError::Failed)?;
                    let number = check_merge(&status, &head)?;
                    let method = pr_draft_service::configured_merge_method(&state);
                    let message = pr_draft_service::merge_workspace_pr(
                        &state,
                        &workspace_id,
                        number,
                        status.url.as_deref(),
                        &head,
                        &method,
                    )
                    .map_err(TeamActionError::Failed)?;
                    Ok::<_, TeamActionError>((number, message))
                }
            })
            .await?;
        cx.update(|cx| {
            let window = find_workspace_window(&workspace_id, cx).or_else(|| front_window(cx));
            if let Some(window) = window {
                show_phone_notice(&window, format!("merged PR #{number}"), cx);
            }
        });
        Ok(PhoneActionOutcome {
            workspace_id: Some(workspace_id),
            message: Some(message),
            ..PhoneActionOutcome::default()
        })
    })
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
    use mnemonic_core::models::WorkspacePrStatus;
    use mnemonic_core::services::team_share_service::TeamWorkspace;

    #[test]
    fn archive_refuses_an_unknown_workspace_or_a_teammates_copy() {
        assert_eq!(
            archive_refusal(None),
            Some(TeamActionError::Invalid("No such workspace".to_string()))
        );
        assert_eq!(
            archive_refusal(Some(TEAM_COPY_SOURCE)),
            Some(TeamActionError::Invalid(
                "Archive a teammate's copy on the Mac".to_string()
            ))
        );
        assert_eq!(archive_refusal(Some("local")), None);
    }

    #[test]
    fn app_alert_error_reports_firebase_refusing_every_send_until_one_lands() {
        let mut status = AppAlertStatus {
            has_devices: true,
            ..Default::default()
        };
        status.record_sends(0, &["404 project not found".to_string()]);
        assert_eq!(status.error().as_deref(), Some("404 project not found"));

        status.sign_in_error = Some("sign in failed".to_string());
        assert_eq!(status.error().as_deref(), Some("sign in failed"));
        status.key_error = Some("key missing".to_string());
        assert_eq!(status.error().as_deref(), Some("key missing"));

        status.record_sends(0, &[]);
        assert_eq!(status.send_error.as_deref(), Some("404 project not found"));
        status.record_sends(1, &["403 mismatch".to_string()]);
        assert_eq!(status.send_error, None);
    }

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

    fn saved_workspace(workspace_id: &str, chat_id: &str) -> TeamWorkspace {
        serde_json::from_value(serde_json::json!({
            "id": workspace_id,
            "project": "mnemonic",
            "name": "fix-login",
            "branch": "jay/fix-login",
            "chats": [{ "id": chat_id, "title": "Claude", "agent": "claude-acp", "open": true }],
        }))
        .unwrap_or_else(|err| panic!("the test workspace didn't parse: {err}"))
    }

    #[gpui::test]
    async fn no_window_says_open_mnemonic(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            let phone = cx.new(PhoneShare::new);
            phone
                .read(cx)
                .feed
                .read(cx)
                .hub()
                .set_workspaces(vec![saved_workspace("ws-1", "chat-1")]);
            cx.set_global(GlobalPhoneShare(phone));
        });

        let open_mnemonic = || {
            Err(TeamActionError::NothingToDo(
                "Open Mnemonic on the Mac first".to_string(),
            ))
        };
        let needs_a_window = [
            PhoneAction::Message {
                chat_id: "chat-1".to_string(),
                text: "Carry on".to_string(),
            },
            PhoneAction::NewChat {
                workspace_id: "ws-1".to_string(),
                agent: "claude".to_string(),
            },
            PhoneAction::CreatePr {
                workspace_id: "ws-1".to_string(),
            },
            PhoneAction::StartTask {
                repository_id: "repo-1".to_string(),
                agent: "claude".to_string(),
                text: "Fix the login page".to_string(),
                choices: Vec::new(),
            },
        ];
        for action in needs_a_window {
            let outcome = cx.update(|cx| run_phone_action(action.clone(), cx)).await;
            assert_eq!(outcome, open_mnemonic(), "{action:?}");
        }

        let stopped = cx
            .update(|cx| {
                run_phone_action(
                    PhoneAction::Stop {
                        chat_id: "chat-1".to_string(),
                    },
                    cx,
                )
            })
            .await;
        assert_eq!(
            stopped,
            Err(TeamActionError::NothingToDo(
                "The agent isn't running".to_string()
            ))
        );
        let answered = cx
            .update(|cx| {
                run_phone_action(
                    PhoneAction::Permission {
                        chat_id: "chat-1".to_string(),
                        tool_call_id: "run-tests".to_string(),
                        option_id: "allow".to_string(),
                    },
                    cx,
                )
            })
            .await;
        assert_eq!(
            answered,
            Err(TeamActionError::NothingToDo("Already answered".to_string()))
        );
    }

    fn repository(id: &str, name: &str) -> DiscoveredRepository {
        DiscoveredRepository {
            id: id.to_string(),
            name: name.to_string(),
            path: format!("/code/{name}"),
            current_branch: Some("main".to_string()),
            head: None,
            is_dirty: false,
            worktrees: Vec::new(),
            last_scanned_at: String::new(),
        }
    }

    #[test]
    fn start_task_refuses_an_unknown_repository() {
        let repositories = vec![repository("repo-1", "mnemonic")];
        let enabled = [ChatAgent::Claude];

        assert!(matches!(
            task_to_start(&repositories, &enabled, "repo-2", "claude"),
            Err(TeamActionError::Invalid(_))
        ));
        assert!(matches!(
            task_to_start(&repositories, &enabled, "repo-1", "codex"),
            Err(TeamActionError::Invalid(_))
        ));
        let started = task_to_start(&repositories, &enabled, "repo-1", "Claude")
            .map(|(repository, agent)| (repository.id, agent));
        assert_eq!(started, Ok(("repo-1".to_string(), ChatAgent::Claude)));
    }

    fn mergeable_pr(head: &str) -> WorkspacePrStatus {
        WorkspacePrStatus {
            workspace_id: "ws-1".to_string(),
            found: true,
            number: Some(212),
            title: Some("Fix the login page".to_string()),
            url: Some("https://github.com/example/mnemonic/pull/212".to_string()),
            state: Some("OPEN".to_string()),
            is_draft: false,
            review_decision: None,
            mergeable: Some("MERGEABLE".to_string()),
            merge_state_status: Some("CLEAN".to_string()),
            head_ref_oid: Some(head.to_string()),
            checks_summary: String::new(),
            checks: Vec::new(),
            warning: None,
        }
    }

    #[test]
    fn merge_refuses_a_moved_head() {
        let status = mergeable_pr("0123abc");

        assert_eq!(check_merge(&status, "0123abc"), Ok(212));
        assert_eq!(
            check_merge(&status, "9999fff"),
            Err(TeamActionError::NothingToDo(
                "The PR changed since you looked. Refresh and try again.".to_string()
            ))
        );
        let draft = WorkspacePrStatus {
            is_draft: true,
            ..status
        };
        assert_eq!(
            check_merge(&draft, "0123abc"),
            Err(TeamActionError::NothingToDo(
                "This PR can't be merged right now".to_string()
            ))
        );
    }

    #[gpui::test]
    async fn a_hung_action_times_out(cx: &mut TestAppContext) {
        let hung: Task<Result<PhoneActionOutcome, TeamActionError>> =
            cx.update(|cx| cx.spawn(async move |_| futures::future::pending().await));
        let reply = cx.update(|cx| {
            within_timeout(
                hung,
                LONG_ACTION_TIMEOUT,
                Err(TeamActionError::Failed("Timed out".to_string())),
                cx,
            )
        });
        cx.executor().advance_clock(LONG_ACTION_TIMEOUT);

        assert_eq!(
            reply.await,
            Err(TeamActionError::Failed("Timed out".to_string()))
        );

        let quick = cx.update(|cx| {
            within_timeout(
                Task::ready(Ok(PhoneActionOutcome::default())),
                ACTION_TIMEOUT,
                Err(TeamActionError::Failed("Timed out".to_string())),
                cx,
            )
        });
        assert_eq!(quick.await, Ok(PhoneActionOutcome::default()));
    }

    #[test]
    fn a_message_still_stopping_the_agent_is_not_a_failure() {
        let message = PhoneAction::Message {
            chat_id: "chat".to_string(),
            text: "carry on".to_string(),
        };
        assert_eq!(
            timed_out(&message),
            Ok(PhoneActionOutcome {
                message: Some(
                    "Stopping the agent on the Mac; your message goes out once it stops."
                        .to_string()
                ),
                ..PhoneActionOutcome::default()
            })
        );
    }

    #[test]
    fn each_permission_prompt_alerts_once_while_finishes_collapse() {
        let prompt = |tool_call_id: &str| ChatFeedEvent::PermissionRequested {
            workspace_id: "ws-1".to_string(),
            chat_id: "chat-1".to_string(),
            tool_kind: acp::ToolKind::Execute,
            tool_call_id: tool_call_id.to_string(),
            quick_answers: None,
        };
        let finished = ChatFeedEvent::TurnEnded {
            workspace_id: "ws-1".to_string(),
            chat_id: "chat-1".to_string(),
            chat_title: "Claude".to_string(),
            stop_reason: acp::StopReason::EndTurn,
        };
        let start = Instant::now();
        let after = |seconds: u64| start + Duration::from_secs(seconds);
        let mut last_notified = HashMap::new();

        assert!(!collapse_repeat(
            &mut last_notified,
            alert_key(&prompt("run-tests")),
            start
        ));
        assert!(!collapse_repeat(
            &mut last_notified,
            alert_key(&prompt("push")),
            after(5)
        ));
        assert!(collapse_repeat(
            &mut last_notified,
            alert_key(&prompt("push")),
            after(10)
        ));

        assert!(!collapse_repeat(
            &mut last_notified,
            alert_key(&finished),
            after(10)
        ));
        assert!(collapse_repeat(
            &mut last_notified,
            alert_key(&finished),
            after(20)
        ));
        assert!(!collapse_repeat(
            &mut last_notified,
            alert_key(&finished),
            after(41)
        ));
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

    #[test]
    fn a_failed_certificate_serves_http_and_says_why() {
        let me = TailscaleSelf {
            ip: Ipv4Addr::new(100, 64, 0, 1),
            login: "jay@example.com".into(),
            display_name: "Jay".into(),
            dns_name: "jay-mbp.tail1234.ts.net".into(),
        };
        let directory = Path::new("/data/phone");
        let failed = https_certificate(&me, Some(directory), |dns_name, directory| {
            assert_eq!(
                (dns_name, directory),
                ("jay-mbp.tail1234.ts.net", Path::new("/data/phone"))
            );
            Err("HTTPS is not enabled for this tailnet".to_string())
        });
        assert!(failed.is_err_and(|err| err == "HTTPS is not enabled for this tailnet"));
        assert_eq!(
            page_url(&me, 47232, false),
            "http://jay-mbp.tail1234.ts.net:47232/"
        );
        assert_eq!(
            page_url(&me, 47232, true),
            "https://jay-mbp.tail1234.ts.net:47232/"
        );

        let without_magic_dns = TailscaleSelf {
            dns_name: String::new(),
            ..me
        };
        let skipped = https_certificate(&without_magic_dns, Some(directory), |_, _| {
            Err("tailscale cert ran without a name".to_string())
        });
        assert!(skipped.is_err_and(|err| err.contains("MagicDNS")));
    }

    #[test]
    fn renewal_restarts_only_for_a_new_certificate() {
        let files = |certificate: &[u8]| -> Result<TlsFiles, String> {
            Ok(TlsFiles {
                certificate: certificate.to_vec(),
                private_key: b"key".to_vec(),
            })
        };
        assert!(!certificate_changed(Some(b"old"), &files(b"old")));
        assert!(certificate_changed(Some(b"old"), &files(b"new")));
        assert!(certificate_changed(None, &files(b"new")));
        assert!(!certificate_changed(
            Some(b"old"),
            &Err("tailscale is offline".to_string())
        ));
    }
}
