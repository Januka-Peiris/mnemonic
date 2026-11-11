use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::schema as acp;
use agent_settings::AgentSettings;
use agent_ui::{
    Agent, AgentConnectionStore, AgentNotification, AgentNotificationEvent, AgentThreadSource,
    ConversationView,
};
use editor::Editor;
use futures::future::Either;
use gpui::{
    Action as _, Anchor, AnyWindowHandle, App, AppContext as _, Axis, Context, DragMoveEvent, Empty,
    Entity, EntityId, EventEmitter, ExternalPaths, FocusHandle, Focusable, IntoElement, Pixels,
    Render, SharedString, Subscription, WeakEntity, Window, WindowHandle, actions, px,
};
use mnemonic_bridge::AgentAttention;
use mnemonic_core::models::{ChatSessionRecord, WorkspaceSummary};
use mnemonic_core::repositories::{chat_session_repository, workspace_repository};
use mnemonic_core::services::workspace_script_service::RunCommandPlan;
use mnemonic_core::services::{
    checkpoint_service, terminal_service, workspace_script_service, workspace_service,
};
use project::agent_server_store::AllAgentServersSettings;
use project::{AgentId, Project};
use settings::{NotifyWhenAgentWaiting, ReviewAgent, Settings as _};
use task::{RevealStrategy, Shell, SpawnInTerminal, TaskId};
use terminal::Terminal;
use terminal_view::TerminalView;
use util::path_list::PathList;
use util::ResultExt as _;
use util::shell_builder::ShellBuilder;
use ui::{
    Button, ButtonStyle, ContextMenu, Icon, IconButton, IconName, Label, PopoverMenu,
    Tooltip, h_flex, prelude::*, v_flex,
};
use workspace::item::{Item, ItemEvent, ItemHandle};
use workspace::notifications::NotificationId;
use workspace::{
    ActivatePaneDown, ActivatePaneLeft, ActivatePaneRight, ActivatePaneUp, DraggedTab, Pane,
    Member, PaneAxis, PaneGroup, PaneRenderContext, SplitDirection, SplitMode, Toast, Workspace,
    WorkspaceId, pane,
};
use zed_actions::mnemonic::AgentSessionKind;

use crate::workspace_detail_panel::WorkspaceDetailPanel;
use crate::workspaces_settings::WorkspacesSettings;
use crate::workspace_panel::{
    WorkspacePanel, branch_slug_from_message, is_generated_workspace_name,
};

/// Marker for auto-fix loop toasts.
struct MnemonicAutoFixToast;

actions!(
    mnemonic_composer,
    [
        /// Send the composer contents to the workspace's agent terminal.
        Send,
        /// Cycle the focused agent's mode by sending shift+tab to its terminal
        /// (Claude: default, acceptEdits, plan, bypass, auto; Codex:
        /// default and plan).
        CycleAgentMode
    ]
);

/// Forge's proven dispatch pattern: the prompt text and the submitting
/// carriage return must arrive as separate input events with a pause in
/// between, or the agent TUI treats the Enter as part of a paste.
const ENTER_DELAY_MILLIS: u64 = 250;

/// How long after the agent terminal's last output it still counts as
/// running. Claude Code redraws its spinner continuously while working, so
/// output going quiet for this long means it's waiting for input.
const AGENT_IDLE_AFTER: Duration = Duration::from_secs(4);

/// Archive waits at most this long for the workspace's teardown commands.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(120);

/// Restart waits this long after killing the run commands so a dev server
/// has released its port before the new one binds it.
const RUN_RESTART_DELAY: Duration = Duration::from_millis(500);

/// Auto-fix loop: how often to poll GitHub for check results.
const AUTO_FIX_POLL_INTERVAL: Duration = Duration::from_secs(45);
/// Auto-fix loop: minimum gap between fix prompts, so the agent isn't
/// re-prompted while its previous fix is still building on CI.
const AUTO_FIX_MIN_GAP: Duration = Duration::from_secs(180);
/// Auto-fix loop: give up after this many fix attempts.
const AUTO_FIX_MAX_ATTEMPTS: u32 = 5;

/// Claude model IDs and chip labels.
const CLAUDE_MODELS: &[(&str, &str)] = &[
    ("claude-fable-5-2", "Fable 5.2"),
    ("claude-fable-5-1", "Fable 5.1"),
    ("claude-opus-5-5", "Opus 5.5"),
    ("claude-sonnet-5-5", "Sonnet 5.5"),
    ("claude-haiku-5-5", "Haiku 5.5"),
];

/// Codex model IDs and chip labels.
const CODEX_MODELS: &[(&str, &str)] = &[
    ("gpt-6.1-sol", "GPT-6.1 Sol"),
    ("gpt-6.1-terra", "GPT-6.1 Terra"),
    ("gpt-6-astra", "GPT-6 Astra"),
    ("gpt-6-luna", "GPT-6 Luna"),
];

/// Effort levels for the effort dropdown.
const AGENT_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// An effort value as shown in the composer, such as "High" for "high".
fn effort_label(effort: &str) -> String {
    if effort == "xhigh" {
        return "Extra high".to_string();
    }
    let mut characters = effort.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

/// The modes shift+tab cycles a Claude session through: the base modes
/// (default, acceptEdits, plan) then the optional ones, bypass before auto.
/// Sessions launch with --dangerously-skip-permissions, so the chip starts on
/// "Bypass". The label is a best-effort mirror: pressing shift+tab inside the
/// terminal directly can desync it.
const CLAUDE_AGENT_MODES: &[&str] = &["Default", "Auto-accept", "Plan", "Bypass", "Auto"];
const INITIAL_CLAUDE_MODE_INDEX: usize = 3;

/// The modes shift+tab cycles a Codex session through. Codex's shift+tab only
/// toggles the collaboration mode (plan vs default); its permission presets
/// are set separately via /permissions and aren't part of this cycle.
const CODEX_AGENT_MODES: &[&str] = &["Default", "Plan"];
const INITIAL_CODEX_MODE_INDEX: usize = 0;

/// Which agent provider a terminal session is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentProvider {
    Claude,
    Codex,
}

/// The ACP agents a workspace chat can run. Each id must be a registry
/// agent listed under `agent_servers` in the default settings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ChatAgent {
    Claude,
    Codex,
    Gemini,
    OpenCode,
    Cursor,
}

impl ChatAgent {
    const ALL: [Self; 5] = [
        Self::Claude,
        Self::Codex,
        Self::Gemini,
        Self::OpenCode,
        Self::Cursor,
    ];

    fn agent_id(self) -> &'static str {
        match self {
            Self::Claude => "claude-acp",
            Self::Codex => "codex-acp",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
            Self::Cursor => "cursor",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Gemini => "Gemini",
            Self::OpenCode => "OpenCode",
            Self::Cursor => "Cursor",
        }
    }

    fn from_agent_id(agent_id: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|agent| agent.agent_id() == agent_id)
            .unwrap_or(Self::Claude)
    }

    /// The agents offered for new chats: those listed under `agent_servers`
    /// in the settings.
    fn enabled(cx: &App) -> Vec<Self> {
        let servers = AllAgentServersSettings::get_global(cx);
        Self::ALL
            .into_iter()
            .filter(|agent| servers.contains_key(agent.agent_id()))
            .collect()
    }

    /// The CLI tab offered when this agent's chat can't start.
    fn cli_fallback(self) -> Option<NewSession> {
        match self {
            Self::Claude => Some(NewSession::ClaudeCli),
            Self::Codex => Some(NewSession::CodexCli),
            Self::Gemini | Self::OpenCode | Self::Cursor => None,
        }
    }
}

pub(crate) struct ChatSession {
    record_id: String,
    agent: ChatAgent,
    is_review: bool,
    view: Entity<ConversationView>,
    stored_session_id: Option<String>,
    observed_thread: Option<EntityId>,
    pending_messages: PendingMessages,
    /// Instructions the app sent (review prompts, Create PR, CI fixes), so
    /// they are never mistaken for the user's first message.
    app_messages: Vec<String>,
    /// A new review chat holds its prompt until it has been switched to the
    /// review model.
    awaiting_model: bool,
    /// Watches the current root thread; replaced when the thread changes.
    thread_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

pub(crate) struct TerminalSession {
    view: Entity<TerminalView>,
    /// The agent running in this terminal; None for plain shells.
    provider: Option<AgentProvider>,
    /// The persistent tmux session backing this terminal, if any.
    session_name: Option<String>,
    /// Started by Run from the workspace's `.mnemonic/config.json`.
    run_command: bool,
    _subscription: Subscription,
}

pub(crate) enum SessionKind {
    Terminal(TerminalSession),
    Chat(ChatSession),
}

pub(crate) struct WorkspaceSession {
    title: SharedString,
    /// The session's tab, living in one of the workspace's panes.
    item: Entity<SessionItem>,
    kind: SessionKind,
}

impl WorkspaceSession {
    fn terminal(&self) -> Option<&Entity<TerminalView>> {
        match &self.kind {
            SessionKind::Terminal(terminal) => Some(&terminal.view),
            SessionKind::Chat(_) => None,
        }
    }

    fn provider(&self) -> Option<AgentProvider> {
        match &self.kind {
            SessionKind::Terminal(terminal) => terminal.provider,
            SessionKind::Chat(_) => None,
        }
    }

    fn chat(&self) -> Option<&ChatSession> {
        match &self.kind {
            SessionKind::Chat(chat) => Some(chat),
            SessionKind::Terminal(_) => None,
        }
    }

    fn chat_mut(&mut self) -> Option<&mut ChatSession> {
        match &mut self.kind {
            SessionKind::Chat(chat) => Some(chat),
            SessionKind::Terminal(_) => None,
        }
    }
}

enum SessionContent {
    Terminal(Entity<TerminalView>),
    Chat {
        view: Entity<ConversationView>,
        record_id: String,
        agent: ChatAgent,
    },
}

/// A session's tab in one of the workspace's panes. Terminals are wrapped
/// instead of being added to the pane directly because `TerminalView` hands
/// dropped tabs to the terminal panel.
pub(crate) struct SessionItem {
    item_id: EntityId,
    title: SharedString,
    content: SessionContent,
    workspace_view: WeakEntity<WorkspaceView>,
}

impl EventEmitter<ItemEvent> for SessionItem {}

impl Focusable for SessionItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.content {
            SessionContent::Terminal(view) => view.focus_handle(cx),
            SessionContent::Chat { view, .. } => view.focus_handle(cx),
        }
    }
}

impl Item for SessionItem {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(match &self.content {
            SessionContent::Terminal(_) => IconName::Terminal,
            SessionContent::Chat { .. } => IconName::Chat,
        }))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn handle_drop(
        &self,
        active_pane: &Pane,
        dropped: &dyn Any,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        // Away from an edge the pane moves the tab into itself.
        let (Some(tab), Some(direction)) = (
            dropped.downcast_ref::<DraggedTab>(),
            active_pane.drag_split_direction(),
        ) else {
            return false;
        };
        let workspace_view = self.workspace_view.clone();
        let target_item = self.item_id;
        let source_pane = tab.pane.clone();
        let dragged_item = tab.item.item_id();
        // The pane handling this drop is mid-update, so the split waits
        // until it is done.
        window.defer(cx, move |window, cx| {
            workspace_view
                .update(cx, |workspace_view, cx| {
                    workspace_view.split_for_dropped_tab(
                        target_item,
                        source_pane,
                        dragged_item,
                        direction,
                        window,
                        cx,
                    );
                })
                .log_err();
        });
        true
    }
}

impl Render for SessionItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (view, record_id, agent) = match &self.content {
            SessionContent::Terminal(view) => {
                return div().size_full().child(view.clone()).into_any_element();
            }
            SessionContent::Chat {
                view,
                record_id,
                agent,
            } => (view.clone(), record_id.clone(), *agent),
        };
        let failed = view.read(cx).has_load_error();
        let workspace_view = self.workspace_view.clone();
        v_flex()
            .size_full()
            .when(failed, |el| {
                el.child(
                    h_flex()
                        .p_2()
                        .gap_2()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Label::new("This chat couldn't start.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Button::new("chat-start-new", "Start new chat")
                                .label_size(LabelSize::Small)
                                .on_click({
                                    let workspace_view = workspace_view.clone();
                                    move |_, window, cx| {
                                        workspace_view
                                            .update(cx, |workspace_view, cx| {
                                                workspace_view.restart_chat(
                                                    record_id.clone(),
                                                    window,
                                                    cx,
                                                );
                                            })
                                            .log_err();
                                    }
                                }),
                        )
                        .when_some(agent.cli_fallback(), |el, cli| {
                            el.child(
                                Button::new(
                                    "chat-open-cli-instead",
                                    format!("Open {} CLI instead", agent.label()),
                                )
                                .label_size(LabelSize::Small)
                                .on_click(move |_, window, cx| {
                                    workspace_view
                                        .update(cx, |workspace_view, cx| {
                                            workspace_view.add_session(cli, window, cx);
                                        })
                                        .log_err();
                                }),
                            )
                        }),
                )
            })
            .child(div().flex_grow(1.0).min_h(px(0.)).child(view))
            .into_any_element()
    }
}

/// Whether a drag carries a workspace session's tab. The window's own panes
/// refuse these, so a session can only be dropped inside its workspace.
pub fn is_session_tab(dragged: &dyn Any) -> bool {
    dragged
        .downcast_ref::<DraggedTab>()
        .is_some_and(|tab| tab.item.downcast::<SessionItem>().is_some())
}

/// Types files dropped on a workspace into the terminal under the cursor,
/// or attaches them to the chat there. Returns false when no session is
/// under the cursor.
fn insert_dropped_paths(
    view: &Entity<WorkspaceView>,
    paths: &ExternalPaths,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let (terminal, chat, project) = {
        let view = view.read(cx);
        let pane = view
            .center
            .pane_at_pixel_position(window.mouse_position())
            .unwrap_or(&view.active_pane);
        let session = pane
            .read(cx)
            .active_item()
            .and_then(|item| view.session_index(item.item_id()))
            .and_then(|index| view.sessions.get(index));
        (
            session.and_then(WorkspaceSession::terminal).cloned(),
            session
                .and_then(WorkspaceSession::chat)
                .map(|chat| chat.view.clone()),
            view.project.upgrade(),
        )
    };
    if let Some(terminal) = terminal {
        terminal.update(cx, |terminal, cx| {
            terminal.add_paths_to_terminal(paths.paths(), window, cx);
        });
        return true;
    }
    let (Some(chat), Some(project)) = (chat, project) else {
        return false;
    };
    let tasks = paths
        .paths()
        .iter()
        .map(|path| Workspace::project_path_for_path(project.clone(), path, false, cx))
        .collect::<Vec<_>>();
    window
        .spawn(cx, async move |cx| {
            let mut project_paths = Vec::new();
            let mut added_worktrees = Vec::new();
            for entry in futures::future::join_all(tasks).await {
                if let Some((worktree, project_path)) = entry.log_err() {
                    added_worktrees.push(worktree);
                    project_paths.push(project_path);
                }
            }
            chat.update_in(cx, |chat, window, cx| {
                chat.insert_dragged_files(project_paths, added_worktrees, window, cx);
            })
            .log_err();
        })
        .detach();
    true
}

/// Whether a dragged tab is one of the given workspace's sessions.
fn is_session_tab_of(
    workspace_view: &WeakEntity<WorkspaceView>,
    dragged: &dyn Any,
    cx: &App,
) -> bool {
    dragged.downcast_ref::<DraggedTab>().is_some_and(|tab| {
        tab.item
            .downcast::<SessionItem>()
            .is_some_and(|item| &item.read(cx).workspace_view == workspace_view)
    })
}

/// Instructions waiting for a chat to connect or finish its current turn,
/// sent oldest first.
#[derive(Default)]
pub(crate) struct PendingMessages(std::collections::VecDeque<String>);

impl PendingMessages {
    pub(crate) fn push(&mut self, text: String) {
        self.0.push_back(text);
    }

    pub(crate) fn take_next(&mut self) -> Option<String> {
        self.0.pop_front()
    }
}

/// What dispatch routing needs to know about a session.
#[derive(Clone, Debug)]
pub(crate) enum SessionSummary {
    Chat {
        agent: ChatAgent,
        is_review: bool,
        failed: bool,
    },
    Terminal { is_agent: bool },
}

/// Who wrote an instruction. Only text the user typed may name the
/// workspace's branch; app-generated instructions never do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MessageSource {
    User,
    App,
}

/// Where a dispatched instruction came from. Composer text is typed while
/// looking at a CLI tab, so it goes to that CLI; everything else (Create PR,
/// Fix errors, auto-fix) prefers the workspace's chat.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DispatchOrigin {
    Composer,
    External,
}

/// Whether a thread event ends the current turn, so the next queued
/// instruction can be sent. A turn ending does not notify the thread's
/// observers, only emits these events.
pub(crate) fn releases_pending(event: &acp_thread::AcpThreadEvent) -> bool {
    matches!(
        event,
        acp_thread::AcpThreadEvent::Stopped(_)
            | acp_thread::AcpThreadEvent::Error
            | acp_thread::AcpThreadEvent::Refusal
    )
}

/// The typed text of a user message, without mentions or attachments.
fn user_message_text(message: &acp_thread::UserMessage) -> String {
    message
        .chunks
        .iter()
        .filter_map(|chunk| match chunk {
            acp::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Workspaces created chat-first have a primary chat; every other workspace
/// keeps starting its main claude CLI, even after chats are added to it.
pub(crate) fn should_start_main_cli(records: &[ChatSessionRecord]) -> bool {
    !records.iter().any(|record| record.is_primary)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DispatchTarget {
    Chat(usize),
    Terminal(usize),
}

/// Picks where a dispatched instruction goes: the active chat, else the
/// first Claude chat, else the first chat, else the active agent CLI, else
/// the first agent CLI. Composer text goes to the agent CLI being viewed.
/// The review chat and chats that failed to load never receive
/// instructions.
pub(crate) fn pick_dispatch_target(
    origin: DispatchOrigin,
    sessions: &[SessionSummary],
    active: usize,
) -> Option<DispatchTarget> {
    let is_work_chat = |session: &SessionSummary| {
        matches!(
            session,
            SessionSummary::Chat {
                is_review: false,
                failed: false,
                ..
            }
        )
    };
    let is_agent_terminal = |session: &SessionSummary| match session {
        SessionSummary::Terminal { is_agent } => *is_agent,
        SessionSummary::Chat { .. } => false,
    };

    if origin == DispatchOrigin::Composer && sessions.get(active).is_some_and(is_agent_terminal) {
        return Some(DispatchTarget::Terminal(active));
    }
    if sessions.get(active).is_some_and(is_work_chat) {
        return Some(DispatchTarget::Chat(active));
    }
    let first_claude_chat = sessions.iter().position(|session| {
        is_work_chat(session)
            && matches!(
                session,
                SessionSummary::Chat {
                    agent: ChatAgent::Claude,
                    ..
                }
            )
    });
    if let Some(index) = first_claude_chat.or_else(|| sessions.iter().position(is_work_chat)) {
        return Some(DispatchTarget::Chat(index));
    }
    if sessions.get(active).is_some_and(is_agent_terminal) {
        return Some(DispatchTarget::Terminal(active));
    }
    sessions
        .iter()
        .position(is_agent_terminal)
        .map(DispatchTarget::Terminal)
}

#[derive(Clone, Copy)]
enum NewSession {
    Chat(ChatAgent),
    ClaudeCli,
    CodexCli,
    Shell,
}

impl From<AgentSessionKind> for NewSession {
    fn from(kind: AgentSessionKind) -> Self {
        match kind {
            AgentSessionKind::ClaudeChat => Self::Chat(ChatAgent::Claude),
            AgentSessionKind::CodexChat => Self::Chat(ChatAgent::Codex),
            AgentSessionKind::ClaudeCli => Self::ClaudeCli,
            AgentSessionKind::CodexCli => Self::CodexCli,
            AgentSessionKind::Shell => Self::Shell,
        }
    }
}

/// Drag payload for the composer resize handle.
struct ComposerDrag;

impl Render for ComposerDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// The center view for a Mnemonic workspace. The workspace is the unit the
/// user switches between; its sessions (chats, agent CLIs and shells) are tabs
/// in its own panes, all rooted in the workspace's worktree.
pub struct WorkspaceView {
    workspace_id: String,
    workspace_name: SharedString,
    worktree_path: Option<PathBuf>,
    workspace: WeakEntity<Workspace>,
    project: WeakEntity<Project>,
    database_id: Option<WorkspaceId>,
    sessions: Vec<WorkspaceSession>,
    /// The session showing in the focused pane.
    active_session: usize,
    center: PaneGroup,
    active_pane: Entity<Pane>,
    /// The strip along the bottom that shells and Run output open in.
    terminal_pane: Option<Entity<Pane>>,
    connection_store: Option<Entity<AgentConnectionStore>>,
    composer: Entity<Editor>,
    composer_height: Pixels,
    claude_model: String,
    codex_model: String,
    agent_effort: String,
    claude_mode_index: usize,
    codex_mode_index: usize,
    pending_image_paths: Vec<String>,
    last_agent_activity: Option<Instant>,
    idle_check_scheduled: bool,
    auto_fix_enabled: bool,
    auto_fix_attempts: u32,
    last_auto_fix_at: Option<Instant>,
    auto_fix_state: SharedString,
    auto_fix_task: Option<gpui::Task<()>>,
    /// Set once the placeholder name has been considered for renaming, so
    /// later messages never rename the branch again.
    branch_rename_started: bool,
    run_command_count: usize,
    run_tooltip: SharedString,
    run_starting: bool,
    /// The window this view lives in, for bringing it forward from a
    /// notification.
    window_handle: AnyWindowHandle,
    window_active: bool,
    /// Agent CLI output time, kept apart from chat activity because only a
    /// CLI going quiet is read as its turn ending.
    last_cli_activity: Option<Instant>,
    cli_quiet_reported: bool,
    notification_windows: Vec<WindowHandle<AgentNotification>>,
    notification_subscriptions: Vec<Subscription>,
    _window_activation: Subscription,
    _pane_refresh: Subscription,
}

impl WorkspaceView {
    /// Starts the workspace's primary agent terminal, attached to its
    /// persistent tmux session.
    fn spawn_primary_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.spawn_agent_terminal("main", "agent", AgentProvider::Claude, None, window, cx);
    }

    fn add_session(&mut self, kind: NewSession, window: &mut Window, cx: &mut Context<Self>) {
        match kind {
            NewSession::Chat(agent) => self.add_chat(agent, window, cx),
            NewSession::ClaudeCli => self.add_claude_terminal(window, cx),
            NewSession::CodexCli => self.add_codex_terminal(window, cx),
            NewSession::Shell => self.add_shell_terminal(window, cx),
        }
    }

    /// Opens an additional Claude agent session in this workspace, with its
    /// own persistent tmux session.
    fn add_claude_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let claude_count = self
            .terminal_titles()
            .filter(|title| title.starts_with("claude"))
            .count();
        let session = format!("claude-{}", claude_count + 1);
        let title = format!("claude {}", claude_count + 1);
        self.spawn_agent_terminal(&session, &title, AgentProvider::Claude, None, window, cx);
    }

    /// Opens a Codex agent session in this workspace, with its own persistent
    /// tmux session.
    fn add_codex_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let codex_count = self
            .terminal_titles()
            .filter(|title| title.starts_with("codex"))
            .count();
        let session = format!("codex-{}", codex_count + 1);
        let title = format!("codex {}", codex_count + 1);
        self.spawn_agent_terminal(&session, &title, AgentProvider::Codex, None, window, cx);
    }

    /// Opens (or refocuses) the review session: a separate agent that reviews
    /// the workspace's diff, run by the workspace's review provider. In
    /// chat-first workspaces it is a dedicated Review chat.
    pub(crate) fn start_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let existing_review = self.sessions.iter().position(|session| {
            session.chat().is_some_and(|chat| chat.is_review)
                || (session.terminal().is_some() && session.title.as_ref() == "review")
        });
        if let Some(index) = existing_review {
            self.activate_session(index, window, cx);
            return;
        }

        let prompt = "You are a code reviewer. Review all changes in this worktree thoroughly.\n\n\
                      Steps:\n\
                      1. Run `git status` to see what files have changed.\n\
                      2. Run `git diff origin/main...HEAD` (fall back to the repo's default branch \
                         if main doesn't exist) to get the full diff.\n\
                      3. Read the changed files in full where the diff alone is insufficient to \
                         understand context.\n\n\
                      Produce a structured review covering:\n\
                      - **Blocking issues**: bugs, logic errors, data loss risks, race conditions, \
                        or anything that would break in production.\n\
                      - **Security**: injection risks, auth gaps, secrets exposure, unsafe input \
                        handling.\n\
                      - **Performance**: N+1 queries, unnecessary allocations, missing indexes, \
                        expensive operations in hot paths.\n\
                      - **Error handling**: swallowed errors, missing propagation, unhelpful error \
                        messages.\n\
                      - **Suggestions**: style, naming, simplification, missing tests, dead code.\n\
                      - **What looks good**: call out solid patterns and good decisions.\n\n\
                      Be direct and specific — reference file paths and line ranges. \
                      Do not make code changes unless explicitly asked."
            .to_string();
        let chat_first = self
            .sessions
            .iter()
            .any(|session| session.chat().is_some());
        let review_agent = WorkspacesSettings::get_global(cx).review_agent;
        if chat_first {
            let agent = match review_agent {
                ReviewAgent::Claude => ChatAgent::Claude,
                ReviewAgent::Codex => ChatAgent::Codex,
            };
            let record = ChatSessionRecord {
                id: uuid::Uuid::new_v4().to_string(),
                workspace_id: self.workspace_id.clone(),
                agent: agent.agent_id().to_string(),
                title: "Review".to_string(),
                acp_session_id: None,
                is_review: true,
                is_primary: false,
                position: self.next_chat_position(cx),
            };
            let state = mnemonic_bridge::mnemonic_state(cx);
            if let Err(err) = chat_session_repository::insert(&state.db, &record) {
                log::error!("Failed to save review chat session: {err}");
            }
            self.open_chat(record, Some(prompt), window, cx);
            return;
        }

        let provider = match review_agent {
            ReviewAgent::Claude => AgentProvider::Claude,
            ReviewAgent::Codex => AgentProvider::Codex,
        };
        self.spawn_agent_terminal("review", "review", provider, Some(prompt), window, cx);
    }

    /// Starts an agent terminal (Claude or Codex) attached to the given
    /// persistent tmux session of this workspace, optionally typing an
    /// initial prompt into it once it has had a moment to boot.
    fn spawn_agent_terminal(
        &mut self,
        session: &str,
        title: &str,
        provider: AgentProvider,
        initial_prompt: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace_id = self.workspace_id.clone();
        let workspace_name = self.workspace_name.to_string();
        let cwd = self.worktree_path.clone();
        let linked_paths = self.linked_workspace_paths(cx);
        let project = self.project.clone();
        let session = session.to_string();
        let title = title.to_string();

        cx.spawn_in(window, async move |this, cx| {
            // Command resolution may shell out to tmux; keep it off the UI thread.
            let (command, args) = cx
                .background_spawn({
                    let workspace_id = workspace_id.clone();
                    let session = session.clone();
                    let cwd = cwd.clone();
                    async move {
                        let agent_binary = match provider {
                            AgentProvider::Claude => "claude".to_string(),
                            AgentProvider::Codex => "codex".to_string(),
                        };

                        // Sessions always launch with full bypass; the Mode
                        // chip switches the running session live via shift+tab.
                        let mut agent_args = Vec::new();
                        if agent_binary == "codex" {
                            agent_args
                                .push("--dangerously-bypass-approvals-and-sandbox".to_string());
                        } else {
                            agent_args.push("--dangerously-skip-permissions".to_string());
                        }
                        // Both CLIs take --add-dir for folders outside the
                        // working directory.
                        for path in &linked_paths {
                            agent_args.push("--add-dir".to_string());
                            agent_args.push(path.to_string_lossy().into_owned());
                        }
                        match &cwd {
                            Some(dir) => terminal_service::embedded_workspace_terminal_command(
                                &workspace_id,
                                &session,
                                dir,
                                &agent_binary,
                                &agent_args,
                            ),
                            None => (agent_binary, agent_args),
                        }
                    }
                })
                .await;

            let command_label = std::iter::once(command.as_str())
                .chain(args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            let spawn_task = SpawnInTerminal {
                id: TaskId(format!("mnemonic-{workspace_id}-{session}")),
                full_label: workspace_name.clone(),
                label: workspace_name,
                command: Some(command),
                args,
                command_label,
                cwd,
                shell: Shell::System,
                reveal: RevealStrategy::Always,
                use_new_terminal: true,
                allow_concurrent_runs: true,
                ..Default::default()
            };

            let terminal = match project.update(cx, |project, cx| {
                project.create_terminal_task(spawn_task, cx)
            }) {
                Ok(task) => task.await,
                Err(e) => Err(e),
            };

            match terminal {
                Ok(terminal) => {
                    let terminal_for_prompt = terminal.clone();
                    let session_for_close = session.clone();
                    this.update_in(cx, |this, window, cx| {
                        this.push_terminal(
                            &title,
                            Some(session_for_close),
                            Some(provider),
                            false,
                            terminal,
                            window,
                            cx,
                        );
                    })
                    .ok();

                    // Give the TUI a moment to boot, then type the initial
                    // prompt using the text-then-Enter dispatch pattern.
                    if let Some(prompt) = initial_prompt {
                        cx.background_executor().timer(Duration::from_secs(5)).await;
                        terminal_for_prompt.update(cx, |terminal, _| {
                            terminal.input(prompt.into_bytes());
                        });
                        cx.background_executor()
                            .timer(Duration::from_millis(ENTER_DELAY_MILLIS))
                            .await;
                        terminal_for_prompt
                            .update(cx, |terminal, _| terminal.input(b"\r".to_vec()));
                    }
                }
                Err(e) => log::error!("Failed to start agent terminal: {e}"),
            }
        })
        .detach();
    }

    /// Opens an additional plain shell in the workspace's worktree.
    fn add_shell_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.worktree_path.clone();
        let project = self.project.clone();
        let title = format!("shell {}", self.terminal_titles().count());

        cx.spawn_in(window, async move |this, cx| {
            let terminal = match project
                .update(cx, |project, cx| project.create_terminal_shell(cwd, cx))
            {
                Ok(task) => task.await,
                Err(e) => Err(e),
            };

            match terminal {
                Ok(terminal) => {
                    this.update_in(cx, |this, window, cx| {
                        this.push_terminal(&title, None, None, false, terminal, window, cx);
                    })
                    .ok();
                }
                Err(e) => log::error!("Failed to open terminal: {e}"),
            }
        })
        .detach();
    }

    /// Releases what a closed session holds: a terminal's tmux session is
    /// killed so it doesn't linger, and a chat is disconnected and forgotten
    /// so it does not come back on restart.
    fn dispose_session(&mut self, session: WorkspaceSession, cx: &mut Context<Self>) {
        match session.kind {
            SessionKind::Terminal(terminal) => {
                let Some(session_name) = terminal.session_name else {
                    return;
                };
                let workspace_id = self.workspace_id.clone();
                cx.background_spawn(async move {
                    if let Err(err) = terminal_service::kill_embedded_workspace_terminal(
                        &workspace_id,
                        &session_name,
                    ) {
                        log::warn!("Failed to kill tmux session: {err}");
                    }
                })
                .detach();
            }
            SessionKind::Chat(chat) => {
                let close_task = chat.view.update(cx, |view, cx| {
                    view.as_connected()
                        .map(|connected| connected.close_all_sessions(cx))
                });
                if let Some(close_task) = close_task {
                    close_task.detach();
                }
                let state = mnemonic_bridge::mnemonic_state(cx);
                if let Err(err) = chat_session_repository::delete(&state.db, &chat.record_id) {
                    log::error!("Failed to delete chat session: {err}");
                }
            }
        }
    }

    /// Closes a session's tab from code. Tabs the user closes arrive through
    /// `handle_pane_event` instead.
    fn remove_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.sessions.len() {
            return;
        }
        let session = self.sessions.remove(index);
        let item_id = session.item.entity_id();
        if let Some(pane) = self.pane_for_item(item_id, cx) {
            pane.update(cx, |pane, cx| {
                pane.remove_item(item_id, false, true, window, cx);
            });
        }
        self.dispose_session(session, cx);
        self.sync_active_session(cx);
        cx.notify();
    }

    fn session_index(&self, item_id: EntityId) -> Option<usize> {
        self.sessions
            .iter()
            .position(|session| session.item.entity_id() == item_id)
    }

    fn pane_for_item(&self, item_id: EntityId, cx: &App) -> Option<Entity<Pane>> {
        self.center
            .panes()
            .into_iter()
            .find(|pane| pane.read(cx).items().any(|item| item.item_id() == item_id))
            .cloned()
    }

    /// Points `active_session` at the tab showing in the focused pane.
    fn sync_active_session(&mut self, cx: &App) {
        let active_item = self
            .active_pane
            .read(cx)
            .active_item()
            .map(|item| item.item_id());
        self.active_session = match active_item.and_then(|item_id| self.session_index(item_id)) {
            Some(index) => index,
            None => self
                .active_session
                .min(self.sessions.len().saturating_sub(1)),
        };
    }

    /// Creates one of the workspace's session panes: a regular Zed pane, so
    /// its tabs can be reordered, dragged between panes and split.
    fn new_session_pane(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Pane> {
        let this = cx.weak_entity();
        let pane = cx.new(|cx| {
            let mut pane = Pane::new(
                workspace,
                project,
                Default::default(),
                Some(Arc::new({
                    let this = this.clone();
                    move |dragged: &dyn Any, _window: &mut Window, cx: &mut App| {
                        is_session_tab_of(&this, dragged, cx)
                    }
                })),
                zed_actions::mnemonic::NewDefaultAgentSession.boxed_clone(),
                false,
                window,
                cx,
            );
            pane.set_can_navigate(false, cx);
            pane.display_nav_history_buttons(None);
            pane.set_should_display_tab_bar(|_, _| true);
            pane.set_can_toggle_zoom(false, cx);
            pane.set_can_split(Some(Arc::new({
                let this = this.clone();
                move |pane, dragged, _window, cx| {
                    let Some(tab) = dragged.downcast_ref::<DraggedTab>() else {
                        return false;
                    };
                    is_session_tab_of(&this, dragged, cx)
                        && (tab.pane != cx.entity() || pane.items_len() > 1)
                }
            })));
            pane.set_render_tab_bar_buttons(cx, {
                let this = this.clone();
                move |pane, _window, cx| {
                    let Some(view) = this.upgrade() else {
                        return (None, None);
                    };
                    let controls = view.read(cx).render_pane_controls(
                        this.clone(),
                        cx.entity(),
                        pane.items_len() > 1,
                        cx,
                    );
                    (None, Some(controls))
                }
            });
            pane
        });
        cx.subscribe_in(&pane, window, Self::handle_pane_event)
            .detach();
        pane
    }

    fn add_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<Pane>> {
        let project = self.project.upgrade()?;
        Some(Self::new_session_pane(
            self.workspace.clone(),
            project,
            window,
            cx,
        ))
    }

    fn handle_pane_event(
        &mut self,
        pane: &Entity<Pane>,
        event: &pane::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            pane::Event::AddItem { .. } | pane::Event::ActivateItem { .. } => {
                self.sync_active_session(cx);
                cx.notify();
            }
            pane::Event::Focus => {
                self.active_pane = pane.clone();
                self.sync_active_session(cx);
                cx.notify();
            }
            pane::Event::RemovedItem { item } => {
                self.handle_item_removed(item.as_ref(), cx);
            }
            pane::Event::Remove { focus_on_pane } => {
                // The last pane stays, and so does one a tab was moved back
                // into before this event arrived.
                if self.center.panes().len() <= 1 || pane.read(cx).items_len() > 0 {
                    return;
                }
                self.center.remove(pane, cx).log_err();
                if &self.active_pane == pane {
                    let next_pane = focus_on_pane
                        .clone()
                        .filter(|pane| self.center.panes().contains(&pane))
                        .unwrap_or_else(|| self.center.first_pane());
                    self.active_pane = next_pane;
                    if self.is_on_screen(cx) {
                        window.focus(&self.active_pane.focus_handle(cx), cx);
                    }
                }
                self.sync_active_session(cx);
                cx.notify();
            }
            &pane::Event::Split { direction, .. } => {
                self.split_active_tab(pane, direction, window, cx);
            }
            _ => {}
        }
    }

    /// A tab left one of this workspace's panes: it either moved to another
    /// of them or was closed.
    fn handle_item_removed(&mut self, item: &dyn ItemHandle, cx: &mut Context<Self>) {
        let item_id = item.item_id();
        let Some(index) = self.session_index(item_id) else {
            return;
        };
        if self.pane_for_item(item_id, cx).is_none() {
            let session = self.sessions.remove(index);
            self.dispose_session(session, cx);
        }
        self.sync_active_session(cx);
        cx.notify();
    }

    /// Moves the pane's active tab into a new pane beside it. A session can
    /// only be shown once, so splitting never duplicates a tab.
    fn split_active_tab(
        &mut self,
        pane: &Entity<Pane>,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if pane.read(cx).items_len() <= 1 {
            return;
        }
        let Some(new_pane) = self.add_pane(window, cx) else {
            return;
        };
        let Some(item) = pane.update(cx, |pane, cx| pane.take_active_item(window, cx)) else {
            return;
        };
        new_pane.update(cx, |new_pane, cx| {
            new_pane.add_item(item, true, true, None, window, cx);
        });
        self.center.split(pane, &new_pane, direction, cx);
        self.active_pane = new_pane;
        self.sync_active_session(cx);
        cx.notify();
    }

    /// A session tab was dropped on the edge of another session: it moves
    /// into a new pane on that side.
    fn split_for_dropped_tab(
        &mut self,
        target_item: EntityId,
        source_pane: Entity<Pane>,
        dragged_item: EntityId,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target_pane) = self.pane_for_item(target_item, cx) else {
            return;
        };
        if self.session_index(dragged_item).is_none() {
            return;
        }
        if source_pane == target_pane && target_pane.read(cx).items_len() <= 1 {
            return;
        }
        let Some(new_pane) = self.add_pane(window, cx) else {
            return;
        };
        self.center.split(&target_pane, &new_pane, direction, cx);
        workspace::move_item(&source_pane, &new_pane, dragged_item, 0, true, window, cx);
        self.active_pane = new_pane;
        self.sync_active_session(cx);
        cx.notify();
    }

    /// Focuses the split next to the focused one. With no split on that
    /// side the action carries on to the window's own panes.
    fn activate_pane_in_direction(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self
            .center
            .find_pane_in_direction(&self.active_pane, direction, cx)
            .cloned();
        match pane {
            Some(pane) => window.focus(&pane.focus_handle(cx), cx),
            None => cx.propagate(),
        }
    }

    fn new_session_item(
        &self,
        title: SharedString,
        content: SessionContent,
        cx: &mut Context<Self>,
    ) -> Entity<SessionItem> {
        let workspace_view = cx.weak_entity();
        cx.new(|cx| SessionItem {
            item_id: cx.entity_id(),
            title,
            content,
            workspace_view,
        })
    }

    /// Shows a session that was just pushed onto `sessions` as a new tab in
    /// the focused pane.
    fn show_new_session(
        &mut self,
        item: Entity<SessionItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_session = self.sessions.len().saturating_sub(1);
        self.active_pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(item), true, true, None, window, cx);
        });
        cx.notify();
    }

    fn push_terminal(
        &mut self,
        title: &str,
        session_name: Option<String>,
        provider: Option<AgentProvider>,
        run_command: bool,
        terminal: Entity<Terminal>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Agent terminals' output drives the running/waiting status chip.
        let is_agent = provider.is_some();
        let subscription = cx.subscribe(&terminal, move |this, _terminal, event, cx| {
            if matches!(event, terminal::Event::Wakeup) {
                if is_agent {
                    this.last_agent_activity = Some(Instant::now());
                    this.last_cli_activity = this.last_agent_activity;
                    this.cli_quiet_reported = false;
                    mnemonic_bridge::record_agent_activity(&this.workspace_id);
                    this.schedule_idle_check(cx);
                }
                cx.notify();
            }
        });

        let view = cx.new(|cx| {
            TerminalView::new(
                terminal,
                self.workspace.clone(),
                self.database_id,
                self.project.clone(),
                window,
                cx,
            )
        });
        let title = SharedString::from(title.to_string());
        let item = self.new_session_item(title.clone(), SessionContent::Terminal(view.clone()), cx);
        self.sessions.push(WorkspaceSession {
            title,
            item: item.clone(),
            kind: SessionKind::Terminal(TerminalSession {
                view,
                provider,
                session_name,
                run_command,
                _subscription: subscription,
            }),
        });
        if provider.is_some() {
            self.show_new_session(item, window, cx);
            return;
        }
        let Some(pane) = self.ensure_terminal_pane(window, cx) else {
            self.show_new_session(item, window, cx);
            return;
        };
        self.active_session = self.sessions.len().saturating_sub(1);
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(item), true, true, None, window, cx);
        });
        self.active_pane = pane;
        cx.notify();
    }

    /// The pane along the bottom of the workspace for shells and Run output,
    /// created under all the other panes the first time one opens, so a
    /// terminal doesn't take the place of the chat.
    fn ensure_terminal_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Pane>> {
        let existing = self
            .terminal_pane
            .clone()
            .filter(|pane| self.center.panes().contains(&pane));
        if existing.is_some() {
            return existing;
        }
        let pane = self.add_pane(window, cx)?;
        let above = self.center.root.clone();
        // Flexes are relative to an even split, so these give the chats
        // about 70% of the height and the terminal 30%.
        self.center.root = Member::Axis(PaneAxis::load(
            Axis::Vertical,
            vec![above, Member::Pane(pane.clone())],
            Some(vec![1.4, 0.6]),
        ));
        self.center.mark_positions(cx);
        self.terminal_pane = Some(pane.clone());
        Some(pane)
    }

    /// Reads the workspace's run commands off the UI thread, so the Run
    /// control reflects `.mnemonic/config.json` each time the workspace is
    /// selected.
    fn load_run_config(&mut self, cx: &mut Context<Self>) {
        let state = mnemonic_bridge::mnemonic_state(cx);
        let workspace_id = self.workspace_id.clone();
        cx.spawn(async move |this, cx| {
            let config = cx
                .background_spawn(async move {
                    workspace_script_service::get_workspace_mnemonic_config(&state, &workspace_id)
                })
                .await;
            this.update(cx, |this, cx| {
                match config {
                    Ok(config) => {
                        this.run_command_count = config.run.len();
                        this.run_tooltip = SharedString::from(match config.warning {
                            Some(warning) => warning,
                            None if config.run.is_empty() => {
                                "Add \"run\" commands to .mnemonic/config.json to enable Run"
                                    .to_string()
                            }
                            None => format!("Run {}", config.run.join(" + ")),
                        });
                    }
                    Err(err) => {
                        log::warn!("Failed to load workspace run config: {err}");
                        this.run_command_count = 0;
                        this.run_tooltip = SharedString::from(err);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn has_run_terminals(&self) -> bool {
        self.sessions.iter().any(|session| {
            matches!(&session.kind, SessionKind::Terminal(terminal) if terminal.run_command)
        })
    }

    /// Starts each configured run command in its own terminal tab, rooted in
    /// the workspace. The core service only gates and hooks the commands; its
    /// own start path runs them in headless PTYs this app never shows.
    fn start_run_commands(
        &mut self,
        action_kind: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.run_starting {
            return;
        }
        self.run_starting = true;
        cx.notify();

        let state = mnemonic_bridge::mnemonic_state(cx);
        let workspace_id = self.workspace_id.clone();
        let project = self.project.clone();
        cx.spawn_in(window, async move |this, cx| {
            let prepared = cx
                .background_spawn({
                    let state = state.clone();
                    let workspace_id = workspace_id.clone();
                    async move {
                        workspace_script_service::prepare_workspace_run_commands(
                            &state,
                            &workspace_id,
                            action_kind,
                        )
                    }
                })
                .await;
            let (root, plans) = match prepared {
                Ok(prepared) => prepared,
                Err(err) => {
                    this.update(cx, |this, cx| {
                        this.run_starting = false;
                        this.show_workspace_toast(
                            format!("{}: couldn't run: {err}", this.workspace_name),
                            cx,
                        );
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };

            let mut failures = Vec::new();
            for plan in &plans {
                let (command, args) = ShellBuilder::new(&Shell::System, cfg!(windows))
                    .build(Some(plan.command.clone()), &[]);
                let spawn_task = SpawnInTerminal {
                    id: TaskId(format!("mnemonic-run-{workspace_id}-{}", plan.index)),
                    full_label: plan.title.clone(),
                    label: plan.title.clone(),
                    command: Some(command),
                    args,
                    command_label: plan.command.clone(),
                    cwd: Some(root.clone()),
                    shell: Shell::System,
                    reveal: RevealStrategy::Always,
                    use_new_terminal: true,
                    allow_concurrent_runs: true,
                    ..Default::default()
                };
                let terminal = match project
                    .update(cx, |project, cx| project.create_terminal_task(spawn_task, cx))
                {
                    Ok(task) => task.await,
                    Err(err) => Err(err),
                };
                match terminal {
                    Ok(terminal) => {
                        this.update_in(cx, |this, window, cx| {
                            this.push_terminal(&plan.title, None, None, true, terminal, window, cx);
                        })
                        .ok();
                    }
                    Err(err) => failures.push(format!("{}: {err}", plan.title)),
                }
            }

            this.update(cx, |this, cx| {
                this.run_starting = false;
                if !failures.is_empty() {
                    log::error!("Failed to start run commands: {}", failures.join("; "));
                    this.show_workspace_toast(
                        format!(
                            "{}: couldn't start {}",
                            this.workspace_name,
                            failures.join("; ")
                        ),
                        cx,
                    );
                }
                cx.notify();
            })
            .ok();

            let finished = cx
                .background_spawn(async move {
                    workspace_script_service::finish_workspace_run_commands(
                        &state,
                        &workspace_id,
                        action_kind,
                        &plans,
                    )
                })
                .await;
            if let Err(err) = finished {
                log::warn!("Post-run hooks failed: {err}");
            }
        })
        .detach();
    }

    /// Kills the foreground process group of every run terminal; dropping a
    /// terminal alone only signals its shell, which can leave a dev server
    /// running.
    pub(crate) fn kill_run_terminals(&self, cx: &mut Context<Self>) {
        for session in &self.sessions {
            if let SessionKind::Terminal(terminal) = &session.kind {
                if terminal.run_command {
                    let terminal = terminal.view.read(cx).terminal().clone();
                    terminal.update(cx, |terminal, _| terminal.kill_active_task());
                }
            }
        }
    }

    fn stop_run_commands(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.kill_run_terminals(cx);
        let mut index = self.sessions.len();
        while index > 0 {
            index -= 1;
            let is_run = matches!(
                self.sessions.get(index).map(|session| &session.kind),
                Some(SessionKind::Terminal(terminal)) if terminal.run_command
            );
            if is_run {
                self.remove_session(index, window, cx);
            }
        }
    }

    fn restart_run_commands(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_run_commands(window, cx);
        self.run_starting = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(RUN_RESTART_DELAY).await;
            this.update_in(cx, |this, window, cx| {
                this.run_starting = false;
                this.start_run_commands("restart_run_command", window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Runs the workspace's teardown commands one after another in visible
    /// terminal tabs. A failure or running past `TEARDOWN_TIMEOUT` raises a
    /// toast; the returned task always completes so archiving can continue.
    pub(crate) fn run_teardown_commands(
        &mut self,
        root: PathBuf,
        plans: Vec<RunCommandPlan>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        let project = self.project.clone();
        let workspace_id = self.workspace_id.clone();
        let deadline = Instant::now() + TEARDOWN_TIMEOUT;
        cx.spawn_in(window, async move |this, cx| {
            let mut outcome = Ok(());
            for plan in &plans {
                let (command, args) = ShellBuilder::new(&Shell::System, cfg!(windows))
                    .build(Some(plan.command.clone()), &[]);
                let spawn_task = SpawnInTerminal {
                    id: TaskId(format!("mnemonic-teardown-{workspace_id}-{}", plan.index)),
                    full_label: plan.title.clone(),
                    label: plan.title.clone(),
                    command: Some(command),
                    args,
                    command_label: plan.command.clone(),
                    cwd: Some(root.clone()),
                    shell: Shell::System,
                    reveal: RevealStrategy::Always,
                    use_new_terminal: true,
                    allow_concurrent_runs: true,
                    ..Default::default()
                };
                let terminal = match project.update(cx, |project, cx| {
                    project.create_terminal_task(spawn_task, cx)
                }) {
                    Ok(task) => task.await,
                    Err(err) => Err(err),
                };
                let terminal = match terminal {
                    Ok(terminal) => terminal,
                    Err(err) => {
                        outcome = Err(format!("couldn't start {}: {err}", plan.title));
                        break;
                    }
                };
                this.update_in(cx, |this, window, cx| {
                    this.push_terminal(
                        &plan.title,
                        None,
                        None,
                        false,
                        terminal.clone(),
                        window,
                        cx,
                    );
                })
                .log_err();

                let completion =
                    terminal.read_with(cx, |terminal, cx| terminal.wait_for_completed_task(cx));
                let timeout = cx
                    .background_executor()
                    .timer(deadline.saturating_duration_since(Instant::now()));
                match futures::future::select(completion, timeout).await {
                    Either::Left((Some(status), _)) if !status.success() => {
                        outcome = Err(match status.code() {
                            Some(code) => format!("{} exited with code {code}", plan.title),
                            None => format!("{} was interrupted", plan.title),
                        });
                        break;
                    }
                    Either::Left(_) => {}
                    Either::Right(_) => {
                        terminal.update(cx, |terminal, _| terminal.kill_active_task());
                        outcome = Err(format!(
                            "{} didn't finish within {} minutes",
                            plan.title,
                            TEARDOWN_TIMEOUT.as_secs() / 60
                        ));
                        break;
                    }
                }
            }

            if let Err(err) = outcome {
                log::warn!("Teardown for workspace {workspace_id}: {err}");
                this.update(cx, |this, cx| {
                    this.show_workspace_toast(
                        format!("{}: teardown {err}; archiving anyway", this.workspace_name),
                        cx,
                    );
                })
                .log_err();
            }
        })
    }

    fn render_run_controls(&self, this: &WeakEntity<Self>) -> AnyElement {
        if self.has_run_terminals() {
            return h_flex()
                .gap_0p5()
                .child(
                    IconButton::new("workspace-run-restart", IconName::RotateCw)
                        .icon_size(IconSize::Small)
                        .disabled(self.run_starting)
                        .tooltip(Tooltip::text("Stop and re-run the workspace's run commands"))
                        .on_click({
                            let this = this.clone();
                            move |_, window, cx| {
                                this.update(cx, |this, cx| this.restart_run_commands(window, cx))
                                    .log_err();
                            }
                        }),
                )
                .child(
                    IconButton::new("workspace-run-stop", IconName::Stop)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Error)
                        .tooltip(Tooltip::text("Stop the run commands and close their tabs"))
                        .on_click({
                            let this = this.clone();
                            move |_, window, cx| {
                                this.update(cx, |this, cx| this.stop_run_commands(window, cx))
                                    .log_err();
                            }
                        }),
                )
                .into_any_element();
        }
        Button::new(
            "workspace-run",
            if self.run_starting { "Starting…" } else { "Run" },
        )
        .style(ButtonStyle::Subtle)
        .label_size(LabelSize::XSmall)
        .start_icon(
            Icon::new(IconName::PlayFilled)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
        .disabled(self.run_starting || self.run_command_count == 0)
        .tooltip(Tooltip::text(self.run_tooltip.clone()))
        .on_click({
            let this = this.clone();
            move |_, window, cx| {
                this.update(cx, |this, cx| {
                    this.start_run_commands("run_command", window, cx);
                })
                .log_err();
            }
        })
        .into_any_element()
    }

    fn terminal_titles(&self) -> impl Iterator<Item = &SharedString> {
        self.sessions
            .iter()
            .filter(|session| session.terminal().is_some())
            .map(|session| &session.title)
    }

    fn first_terminal(&self) -> Option<&Entity<TerminalView>> {
        self.sessions.iter().find_map(WorkspaceSession::terminal)
    }

    fn active_is_chat(&self) -> bool {
        self.sessions
            .get(self.active_session)
            .is_some_and(|session| session.chat().is_some())
    }

    fn activate_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.get(index) else {
            return;
        };
        let item = session.item.clone();
        self.active_session = index;
        if let Some(pane) = self.pane_for_item(item.entity_id(), cx) {
            pane.update(cx, |pane, cx| {
                if let Some(item_index) = pane.index_for_item(&item) {
                    pane.activate_item(item_index, true, true, window, cx);
                }
            });
            self.active_pane = pane;
        }
        cx.notify();
    }

    /// Adds a new chat tab running the given agent, persisted so it comes
    /// back after a restart.
    fn add_chat(&mut self, agent: ChatAgent, window: &mut Window, cx: &mut Context<Self>) {
        let same_agent_count = self
            .sessions
            .iter()
            .filter_map(WorkspaceSession::chat)
            .filter(|chat| chat.agent == agent && !chat.is_review)
            .count();
        let title = if same_agent_count == 0 {
            agent.label().to_string()
        } else {
            format!("{} {}", agent.label(), same_agent_count + 1)
        };
        let record = ChatSessionRecord {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_id: self.workspace_id.clone(),
            agent: agent.agent_id().to_string(),
            title,
            acp_session_id: None,
            is_review: false,
            is_primary: false,
            position: self.next_chat_position(cx),
        };
        let state = mnemonic_bridge::mnemonic_state(cx);
        if let Err(err) = chat_session_repository::insert(&state.db, &record) {
            log::error!("Failed to save chat session: {err}");
        }
        self.open_chat(record, None, window, cx);
    }

    fn next_chat_position(&self, cx: &App) -> i64 {
        let state = mnemonic_bridge::mnemonic_state(cx);
        match chat_session_repository::list_for_workspace(&state.db, &self.workspace_id) {
            Ok(records) => records
                .iter()
                .map(|record| record.position + 1)
                .max()
                .unwrap_or(0),
            Err(err) => {
                log::error!("Failed to list chat sessions: {err}");
                0
            }
        }
    }

    /// Starts the conversation for a saved chat session, resuming the
    /// agent's session when one was recorded. An initial prompt is queued and
    /// sent once the agent is connected and idle.
    /// The folders of the workspaces linked to this one.
    fn linked_workspace_paths(&self, cx: &App) -> Vec<PathBuf> {
        let state = mnemonic_bridge::mnemonic_state(cx);
        match workspace_service::linked_workspaces(&state, &self.workspace_id) {
            Ok(workspaces) => workspaces
                .into_iter()
                .filter_map(|workspace| workspace.workspace_root_path)
                .map(PathBuf::from)
                .collect(),
            Err(err) => {
                log::error!("Failed to read linked workspaces: {err}");
                Vec::new()
            }
        }
    }

    fn build_chat(
        &mut self,
        record: &ChatSessionRecord,
        initial_prompt: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<ChatSession> {
        let project = self.project.upgrade()?;
        let connection_store = match &self.connection_store {
            Some(store) => store.clone(),
            None => {
                let store = cx.new(|cx| AgentConnectionStore::new(project.clone(), cx));
                self.connection_store = Some(store.clone());
                store
            }
        };

        let agent = Agent::Custom {
            id: AgentId(SharedString::from(record.agent.clone())),
        };
        let fs = project.read(cx).fs().clone();
        let server = agent.server(fs, agent::ThreadStore::global(cx));
        let resume_session_id = record.acp_session_id.clone().map(acp::SessionId::new);
        // The workspace's own folder comes first, which makes it the session's
        // working directory; linked workspaces follow as extra folders the
        // agent may read and edit.
        let work_dirs = self.worktree_path.as_ref().map(|path| {
            let mut paths = vec![path.clone()];
            paths.extend(self.linked_workspace_paths(cx));
            PathList::new(&paths)
        });
        let title = SharedString::from(record.title.clone());
        let workspace = self.workspace.clone();
        let app_messages = initial_prompt.iter().cloned().collect();

        let view = cx.new(|cx| {
            let mut view = ConversationView::new(
                server,
                connection_store,
                agent,
                resume_session_id,
                None,
                work_dirs,
                Some(title),
                None,
                workspace,
                project,
                None,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
            view.suppress_notifications();
            view
        });

        let view_id = view.entity_id();
        let observation = cx.observe(&view, move |this, _view, cx| {
            this.handle_chat_changed(view_id, cx);
        });

        Some(ChatSession {
            record_id: record.id.clone(),
            agent: ChatAgent::from_agent_id(&record.agent),
            is_review: record.is_review,
            view,
            stored_session_id: record.acp_session_id.clone(),
            observed_thread: None,
            pending_messages: {
                let mut pending = PendingMessages::default();
                if let Some(prompt) = initial_prompt {
                    pending.push(prompt);
                }
                pending
            },
            app_messages,
            awaiting_model: record.is_review && record.acp_session_id.is_none(),
            thread_subscriptions: Vec::new(),
            _subscriptions: vec![observation],
        })
    }

    /// Opens a chat tab for a saved chat session.
    fn open_chat(
        &mut self,
        record: ChatSessionRecord,
        initial_prompt: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.build_chat(&record, initial_prompt, window, cx) else {
            return;
        };
        let view_id = chat.view.entity_id();
        let title = SharedString::from(record.title);
        let item = self.new_session_item(
            title.clone(),
            SessionContent::Chat {
                view: chat.view.clone(),
                record_id: chat.record_id.clone(),
                agent: chat.agent,
            },
            cx,
        );
        self.sessions.push(WorkspaceSession {
            title,
            item: item.clone(),
            kind: SessionKind::Chat(chat),
        });
        self.show_new_session(item, window, cx);
        self.handle_chat_changed(view_id, cx);
    }

    fn chat_index(&self, view_id: EntityId) -> Option<usize> {
        self.sessions.iter().position(|session| {
            session
                .chat()
                .is_some_and(|chat| chat.view.entity_id() == view_id)
        })
    }

    /// Records the agent's session id once it exists (so the chat can be
    /// resumed) and starts watching its thread for activity and turn ends.
    fn handle_chat_changed(&mut self, view_id: EntityId, cx: &mut Context<Self>) {
        let Some(index) = self.chat_index(view_id) else {
            return;
        };
        let Some(thread) = self
            .sessions
            .get(index)
            .and_then(WorkspaceSession::chat)
            .and_then(|chat| chat.view.read(cx).root_thread(cx))
        else {
            return;
        };
        let session_id = thread.read(cx).session_id().0.to_string();
        let thread_id = thread.entity_id();
        let is_new_thread = self
            .sessions
            .get(index)
            .and_then(WorkspaceSession::chat)
            .is_some_and(|chat| chat.observed_thread != Some(thread_id));

        let thread_subscriptions = if is_new_thread {
            vec![
                cx.observe(&thread, move |this, thread, cx| {
                    if thread.read(cx).status() == acp_thread::ThreadStatus::Generating {
                        this.last_agent_activity = Some(Instant::now());
                        mnemonic_bridge::record_agent_activity(&this.workspace_id);
                        this.schedule_idle_check(cx);
                    }
                    cx.notify();
                }),
                cx.subscribe(&thread, move |this, thread, event, cx| {
                    if releases_pending(event) {
                        this.flush_pending(view_id, cx);
                        cx.notify();
                    }
                    if matches!(event, acp_thread::AcpThreadEvent::NewEntry) {
                        this.checkpoint_new_user_message(&thread, cx);
                        this.name_workspace_from_thread(view_id, &thread, cx);
                    }
                    this.raise_thread_attention(view_id, &thread, event, cx);
                }),
            ]
        } else {
            Vec::new()
        };

        let Some(chat) = self
            .sessions
            .get_mut(index)
            .and_then(WorkspaceSession::chat_mut)
        else {
            return;
        };

        if chat.stored_session_id.as_deref() != Some(session_id.as_str()) {
            chat.stored_session_id = Some(session_id.clone());
            let record_id = chat.record_id.clone();
            cx.background_spawn(async move {
                let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
                    return;
                };
                if let Err(err) = chat_session_repository::set_acp_session_id(
                    &state.db,
                    &record_id,
                    Some(&session_id),
                ) {
                    log::error!("Failed to save chat session id: {err}");
                }
            })
            .detach();
        }

        if is_new_thread {
            chat.observed_thread = Some(thread_id);
            chat.thread_subscriptions = thread_subscriptions;
            if chat.awaiting_model {
                self.select_review_model(view_id, thread, cx);
            } else {
                self.flush_pending(view_id, cx);
            }
        }
    }

    /// Checkpoints the worktree when a chat turn starts, whether the message
    /// was typed into the chat or sent by the app. Only live messages count
    /// (the thread is generating); history replayed on resume arrives idle.
    fn checkpoint_new_user_message(
        &self,
        thread: &Entity<acp_thread::AcpThread>,
        cx: &mut Context<Self>,
    ) {
        let message = {
            let thread = thread.read(cx);
            if thread.status() != acp_thread::ThreadStatus::Generating {
                return;
            }
            match thread.entries().last() {
                Some(acp_thread::AgentThreadEntry::UserMessage(message)) => {
                    user_message_text(message)
                }
                _ => return,
            }
        };
        self.checkpoint_before_turn(message, cx);
    }

    /// Snapshots the worktree off the main thread so the coming agent turn
    /// can be reverted from the Checkpoints tab. A failure is logged and
    /// never holds up or fails the message being sent.
    fn checkpoint_before_turn(&self, message: String, cx: &mut Context<Self>) {
        let workspace_id = self.workspace_id.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let workspace_id = workspace_id.clone();
                    async move {
                        let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
                            return Ok(None);
                        };
                        checkpoint_service::create_turn_checkpoint(&state, &workspace_id, &message)
                    }
                })
                .await;
            match result {
                Ok(Some(_)) => {
                    this.update(cx, |this, cx| this.notify_checkpoints_changed(cx))
                        .ok();
                }
                Ok(None) => {}
                Err(err) => log::warn!(
                    "Failed to checkpoint workspace {workspace_id} before an agent turn: {err}"
                ),
            }
        })
        .detach();
    }

    fn notify_checkpoints_changed(&self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let workspace_id = self.workspace_id.clone();
        workspace.update(cx, |workspace, cx| {
            if let Some(panel) = workspace.panel::<WorkspaceDetailPanel>(cx) {
                panel.update(cx, |panel, cx| panel.checkpoints_changed(&workspace_id, cx));
            }
        });
    }

    /// Renames a placeholder-named workspace after the first meaningful
    /// message the user typed into a work chat. Instructions the app sent are
    /// skipped, and only messages sent live count (the thread is generating),
    /// so replaying a resumed conversation never renames.
    fn name_workspace_from_thread(
        &mut self,
        view_id: EntityId,
        thread: &Entity<acp_thread::AcpThread>,
        cx: &mut Context<Self>,
    ) {
        if self.branch_rename_started || !is_generated_workspace_name(&self.workspace_name) {
            return;
        }
        let Some(chat) = self
            .chat_index(view_id)
            .and_then(|index| self.sessions.get(index))
            .and_then(WorkspaceSession::chat)
            .filter(|chat| !chat.is_review)
        else {
            return;
        };
        let slug = {
            let thread = thread.read(cx);
            if thread.status() != acp_thread::ThreadStatus::Generating {
                return;
            }
            let entries = thread.entries();
            let first_meaningful = entries.iter().enumerate().find_map(|(index, entry)| {
                let acp_thread::AgentThreadEntry::UserMessage(message) = entry else {
                    return None;
                };
                let text = user_message_text(message);
                if chat.app_messages.contains(&text) {
                    return None;
                }
                branch_slug_from_message(&text).map(|slug| (index, slug))
            });
            match first_meaningful {
                Some((index, slug)) if index + 1 == entries.len() => slug,
                _ => return,
            }
        };
        self.rename_placeholder_workspace(slug, cx);
    }

    /// Renames the workspace and its branch to `<prefix>/<slug>` off the main
    /// thread. The core service refuses when the branch was pushed or a PR
    /// exists; failures are logged and never affect the message being sent.
    fn rename_placeholder_workspace(&mut self, slug: String, cx: &mut Context<Self>) {
        if self.branch_rename_started || !is_generated_workspace_name(&self.workspace_name) {
            return;
        }
        self.branch_rename_started = true;
        if mnemonic_bridge::pr_state(&self.workspace_id).is_some() {
            return;
        }
        let workspace_id = self.workspace_id.clone();
        let expected_name = self.workspace_name.to_string();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
                        return Ok(None);
                    };
                    workspace_service::rename_unpublished_workspace(
                        &state,
                        &workspace_id,
                        &expected_name,
                        &slug,
                    )
                })
                .await;
            let summary = match result {
                Ok(Some(summary)) => summary,
                Ok(None) => return,
                Err(err) => {
                    log::error!("Failed to rename workspace branch: {err}");
                    return;
                }
            };
            log::info!("Renamed workspace to {} ({})", summary.name, summary.branch);
            this.update(cx, |this, cx| {
                this.workspace_name = SharedString::from(summary.name);
                this.refresh_workspace_chrome(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Redraws the surfaces that show the workspace name: the host tab and
    /// the side panels, which reload from the database when rendered.
    fn refresh_workspace_chrome(&self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            if let Some(panel) = workspace.panel::<WorkspacePanel>(cx) {
                panel.update(cx, |_, cx| cx.notify());
            }
            if let Some(panel) = workspace.panel::<WorkspaceDetailPanel>(cx) {
                panel.update(cx, |_, cx| cx.notify());
            }
            for pane in workspace.panes().to_vec() {
                let host = pane
                    .read(cx)
                    .items()
                    .find_map(|item| item.downcast::<WorkspaceHost>());
                if let Some(host) = host {
                    host.update(cx, |_, cx| cx.emit(ItemEvent::UpdateTab));
                }
            }
        });
    }

    /// Sends the next queued instruction to a chat if its thread is idle.
    fn flush_pending(&mut self, view_id: EntityId, cx: &mut Context<Self>) {
        let Some(index) = self.chat_index(view_id) else {
            return;
        };
        let Some(thread) = self
            .sessions
            .get(index)
            .and_then(WorkspaceSession::chat)
            .and_then(|chat| chat.view.read(cx).root_thread(cx))
        else {
            return;
        };
        if thread.read(cx).status() != acp_thread::ThreadStatus::Idle {
            return;
        }
        let Some(chat) = self
            .sessions
            .get_mut(index)
            .and_then(WorkspaceSession::chat_mut)
        else {
            return;
        };
        if chat.awaiting_model {
            return;
        }
        let Some(text) = chat.pending_messages.take_next() else {
            return;
        };
        let send = thread.update(cx, |thread, cx| thread.send(vec![text.as_str().into()], cx));
        cx.spawn(async move |_this, _cx| {
            if let Err(err) = send.await {
                log::error!("Failed to send instruction to chat: {err}");
            }
        })
        .detach();
    }

    /// Switches a new review chat to a Sonnet model when the agent offers
    /// one, then releases its review prompt.
    fn select_review_model(
        &mut self,
        view_id: EntityId,
        thread: Entity<acp_thread::AcpThread>,
        cx: &mut Context<Self>,
    ) {
        let selector = {
            let thread = thread.read(cx);
            thread.connection().model_selector(thread.session_id())
        };
        let list_task = selector.as_ref().map(|selector| selector.list_models(cx));
        cx.spawn(async move |this, cx| {
            if let (Some(selector), Some(list_task)) = (selector, list_task) {
                match list_task.await {
                    Ok(list) => {
                        let models = match list {
                            acp_thread::AgentModelList::Flat(models) => models,
                            acp_thread::AgentModelList::Grouped(groups) => {
                                groups.into_values().flatten().collect()
                            }
                        };
                        let sonnet = models.into_iter().find(|model| {
                            model.id.as_str().to_lowercase().contains("sonnet")
                        });
                        if let Some(model) = sonnet {
                            let select_task = cx.update(|cx| selector.select_model(model.id, cx));
                            if let Err(err) = select_task.await {
                                log::error!("Failed to select review model: {err}");
                            }
                        }
                    }
                    Err(err) => log::error!("Failed to list models for review chat: {err}"),
                }
            }
            this.update(cx, |this, cx| {
                if let Some(chat) = this
                    .chat_index(view_id)
                    .and_then(|index| this.sessions.get_mut(index))
                    .and_then(WorkspaceSession::chat_mut)
                {
                    chat.awaiting_model = false;
                }
                this.flush_pending(view_id, cx);
            })
            .ok();
        })
        .detach();
    }

    fn session_summaries(&self, cx: &App) -> Vec<SessionSummary> {
        self.sessions
            .iter()
            .map(|session| match &session.kind {
                SessionKind::Chat(chat) => SessionSummary::Chat {
                    agent: chat.agent,
                    is_review: chat.is_review,
                    failed: chat.view.read(cx).has_load_error(),
                },
                SessionKind::Terminal(terminal) => SessionSummary::Terminal {
                    is_agent: terminal.provider.is_some(),
                },
            })
            .collect()
    }

    fn any_chat_generating(&self, cx: &App) -> bool {
        self.sessions
            .iter()
            .filter_map(WorkspaceSession::chat)
            .any(|chat| {
                chat.view.read(cx).root_thread(cx).is_some_and(|thread| {
                    thread.read(cx).status() == acp_thread::ThreadStatus::Generating
                })
            })
    }

    /// Closes the agent connections of every chat in this workspace.
    pub(crate) fn close_all_chats(&mut self, cx: &mut Context<Self>) {
        for chat in self.sessions.iter().filter_map(WorkspaceSession::chat) {
            let close_task = chat.view.update(cx, |view, cx| {
                view.as_connected()
                    .map(|connected| connected.close_all_sessions(cx))
            });
            if let Some(close_task) = close_task {
                close_task.detach();
            }
        }
    }

    /// Whether the agent terminal has produced output recently enough to be
    /// considered working (as opposed to waiting at its input prompt).
    fn agent_is_running(&self) -> bool {
        self.last_agent_activity
            .is_some_and(|at| at.elapsed() < AGENT_IDLE_AFTER)
    }

    /// Re-renders once the activity window lapses so the status chip flips
    /// to Waiting without needing another terminal event.
    fn schedule_idle_check(&mut self, cx: &mut Context<Self>) {
        if self.idle_check_scheduled {
            return;
        }
        self.idle_check_scheduled = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(AGENT_IDLE_AFTER).await;
            this.update(cx, |this, cx| {
                this.idle_check_scheduled = false;
                if this.agent_is_running() {
                    this.schedule_idle_check(cx);
                } else {
                    this.report_cli_quiet(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Whether the user can see this workspace right now: it is the one
    /// selected in the sidebar and its window is focused.
    fn is_on_screen(&self, cx: &App) -> bool {
        self.window_active
            && cx
                .try_global::<mnemonic_bridge::ActiveWorkspace>()
                .is_some_and(|active| active.0.as_deref() == Some(self.workspace_id.as_str()))
    }

    /// Flags a chat's finished turn, permission request, or error on the
    /// sidebar and with a system notification, unless the workspace is on
    /// screen.
    fn raise_thread_attention(
        &mut self,
        view_id: EntityId,
        thread: &Entity<acp_thread::AcpThread>,
        event: &acp_thread::AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        let (attention, caption, icon) = match event {
            acp_thread::AcpThreadEvent::Stopped(acp::StopReason::Cancelled) => return,
            acp_thread::AcpThreadEvent::Stopped(_) => {
                // A queued instruction was sent as the turn ended.
                if thread.read(cx).status() == acp_thread::ThreadStatus::Generating {
                    return;
                }
                (AgentAttention::NeedsAttention, "Finished", IconName::Check)
            }
            acp_thread::AcpThreadEvent::ToolAuthorizationRequested(_) => (
                AgentAttention::NeedsAttention,
                "Needs your permission",
                IconName::Info,
            ),
            acp_thread::AcpThreadEvent::Error | acp_thread::AcpThreadEvent::Refusal => (
                AgentAttention::Error,
                "Stopped with an error",
                IconName::Warning,
            ),
            _ => return,
        };
        if self.is_on_screen(cx) {
            return;
        }
        mnemonic_bridge::set_agent_attention(&self.workspace_id, attention);
        self.notify_workspace_panel(cx);
        let source = self
            .chat_index(view_id)
            .and_then(|index| self.sessions.get(index))
            .map(|session| session.title.clone())
            .unwrap_or_default();
        self.show_attention_notification(source, SharedString::from(caption), icon, cx);
    }

    /// An agent CLI that has gone quiet is waiting at its prompt. Only the
    /// sidebar is flagged: terminal output is too coarse a signal to raise
    /// system notifications on.
    fn report_cli_quiet(&mut self, cx: &mut Context<Self>) {
        let went_quiet = self
            .last_cli_activity
            .is_some_and(|at| at.elapsed() >= AGENT_IDLE_AFTER);
        if !went_quiet || self.cli_quiet_reported {
            return;
        }
        self.cli_quiet_reported = true;
        if self.is_on_screen(cx) {
            return;
        }
        if mnemonic_bridge::agent_attention(&self.workspace_id).is_none() {
            mnemonic_bridge::set_agent_attention(
                &self.workspace_id,
                AgentAttention::NeedsAttention,
            );
            self.notify_workspace_panel(cx);
        }
    }

    /// Pops up Zed's agent notification naming this workspace, honoring the
    /// `notify_when_agent_waiting` setting. A newer alert replaces an older
    /// one.
    fn show_attention_notification(
        &mut self,
        source: SharedString,
        caption: SharedString,
        icon: IconName,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_notifications(cx);
        let screens = match AgentSettings::get_global(cx).notify_when_agent_waiting {
            NotifyWhenAgentWaiting::PrimaryScreen => cx.primary_display().into_iter().collect(),
            NotifyWhenAgentWaiting::AllScreens => cx.displays(),
            NotifyWhenAgentWaiting::Never => Vec::new(),
        };
        for screen in screens {
            let options = AgentNotification::window_options(screen, cx);
            let title = self.workspace_name.clone();
            let caption = caption.clone();
            let source = source.clone();
            let Some(window) = cx
                .open_window(options, |_window, cx| {
                    cx.new(|_cx| AgentNotification::new(title, Some(caption), icon, Some(source)))
                })
                .log_err()
            else {
                continue;
            };
            let Some(pop_up) = window.entity(cx).log_err() else {
                continue;
            };
            self.notification_subscriptions.push(cx.subscribe(
                &pop_up,
                |this, _, event: &AgentNotificationEvent, cx| match event {
                    AgentNotificationEvent::Accepted => this.open_from_notification(cx),
                    AgentNotificationEvent::Dismissed => this.dismiss_notifications(cx),
                },
            ));
            self.notification_windows.push(window);
        }
    }

    /// Brings the app forward and selects this workspace in the sidebar.
    fn open_from_notification(&mut self, cx: &mut Context<Self>) {
        self.dismiss_notifications(cx);
        cx.activate(true);
        let window_handle = self.window_handle;
        let workspace = self.workspace.clone();
        let workspace_id = self.workspace_id.clone();
        cx.defer(move |cx| {
            window_handle
                .update(cx, |_, window, cx| {
                    window.activate_window();
                    let panel = workspace
                        .upgrade()
                        .and_then(|workspace| workspace.read(cx).panel::<WorkspacePanel>(cx));
                    if let Some(panel) = panel {
                        panel.update(cx, |panel, cx| {
                            panel.select_workspace(workspace_id, window, cx);
                        });
                    }
                })
                .log_err();
        });
    }

    fn dismiss_notifications(&mut self, cx: &mut Context<Self>) {
        self.notification_subscriptions.clear();
        for window in self.notification_windows.drain(..) {
            window
                .update(cx, |_, window, _| window.remove_window())
                .log_err();
        }
    }

    /// Clears this workspace's sidebar alert and pop-ups once the user is
    /// looking at it.
    pub(crate) fn mark_seen(&mut self, cx: &mut Context<Self>) {
        self.dismiss_notifications(cx);
        if mnemonic_bridge::agent_attention(&self.workspace_id).is_some() {
            mnemonic_bridge::clear_agent_attention(&self.workspace_id);
            self.notify_workspace_panel(cx);
        }
    }

    fn window_activation_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.window_active = window.is_window_active();
        if self.is_on_screen(cx) {
            self.mark_seen(cx);
        }
    }

    /// Repaints the sidebar row. Deferred because this view can be updated
    /// from inside a workspace update.
    fn notify_workspace_panel(&self, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.defer(move |cx| {
            let panel = workspace
                .upgrade()
                .and_then(|workspace| workspace.read(cx).panel::<WorkspacePanel>(cx));
            if let Some(panel) = panel {
                panel.update(cx, |_, cx| cx.notify());
            }
        });
    }

    fn send_to_agent(&mut self, _: &Send, window: &mut Window, cx: &mut Context<Self>) {
        self.send_now(window, cx);
    }

    /// Sends the composer contents to the agent terminal (the first tab),
    /// using Forge's text-then-Enter dispatch so the TUI submits it.
    fn send_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut text = self.composer.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }
        // Restore actual image paths in place of [image] placeholders.
        let pending_paths: Vec<String> = self.pending_image_paths.drain(..).collect();
        for path in pending_paths {
            if let Some(position) = text.find("[image]") {
                text.replace_range(position..position + "[image]".len(), &path);
            } else {
                text = format!("{path} {text}");
            }
        }
        self.dispatch_from(DispatchOrigin::Composer, MessageSource::User, text, cx);
        self.composer.update(cx, |composer, cx| composer.clear(window, cx));
    }

    fn dispatch_target(&self, origin: DispatchOrigin, cx: &App) -> Option<DispatchTarget> {
        pick_dispatch_target(origin, &self.session_summaries(cx), self.active_session)
    }

    /// The terminal that composer input targets: the agent CLI being viewed,
    /// otherwise the first agent CLI.
    fn agent_terminal(&self, cx: &App) -> Option<Entity<Terminal>> {
        match self.dispatch_target(DispatchOrigin::Composer, cx) {
            Some(DispatchTarget::Terminal(index)) => self
                .sessions
                .get(index)
                .and_then(WorkspaceSession::terminal)
                .map(|view| view.read(cx).terminal().clone()),
            Some(DispatchTarget::Chat(_)) | None => None,
        }
    }

    /// Cycles the focused agent's permission mode by sending shift+tab (the
    /// CSI Z back-tab sequence) to its terminal, exactly as pressing
    /// shift+tab in the TUI would. The local label is advanced to mirror it.
    fn cycle_agent_mode(&mut self, cx: &mut Context<Self>) {
        let Some(terminal) = self.agent_terminal(cx) else {
            return;
        };
        terminal.update(cx, |terminal, _| {
            terminal.input(b"\x1b[Z".to_vec());
        });
        match self.active_provider() {
            AgentProvider::Claude => {
                self.claude_mode_index = (self.claude_mode_index + 1) % CLAUDE_AGENT_MODES.len();
            }
            AgentProvider::Codex => {
                self.codex_mode_index = (self.codex_mode_index + 1) % CODEX_AGENT_MODES.len();
            }
        }
        cx.notify();
    }

    fn cycle_agent_mode_action(
        &mut self,
        _: &CycleAgentMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_agent_mode(cx);
    }

    fn agent_mode_label(&self) -> &'static str {
        let (modes, index) = match self.active_provider() {
            AgentProvider::Claude => (CLAUDE_AGENT_MODES, self.claude_mode_index),
            AgentProvider::Codex => (CODEX_AGENT_MODES, self.codex_mode_index),
        };
        modes.get(index).copied().unwrap_or("Default")
    }

    /// Writes an instruction into the agent terminal: text first, then the
    /// submitting Enter as a separate input event (Forge's dispatch pattern).
    /// Targets the active tab when it's an agent session, otherwise the
    /// primary agent terminal.
    pub(crate) fn dispatch_to_agent(&mut self, text: String, cx: &mut Context<Self>) {
        self.dispatch_from(DispatchOrigin::External, MessageSource::App, text, cx);
    }

    fn dispatch_from(
        &mut self,
        origin: DispatchOrigin,
        source: MessageSource,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let index = match self.dispatch_target(origin, cx) {
            Some(DispatchTarget::Chat(index)) => {
                let Some(chat) = self
                    .sessions
                    .get_mut(index)
                    .and_then(WorkspaceSession::chat_mut)
                else {
                    return;
                };
                if source == MessageSource::App {
                    chat.app_messages.push(text.clone());
                }
                chat.pending_messages.push(text);
                let view_id = chat.view.entity_id();
                self.flush_pending(view_id, cx);
                return;
            }
            Some(DispatchTarget::Terminal(index)) => index,
            None => {
                log::warn!("No agent session to receive the instruction");
                return;
            }
        };
        if source == MessageSource::User {
            if let Some(slug) = branch_slug_from_message(&text) {
                self.rename_placeholder_workspace(slug, cx);
            }
        }
        let Some(agent_view) = self.sessions.get(index).and_then(WorkspaceSession::terminal)
        else {
            return;
        };
        self.checkpoint_before_turn(text.clone(), cx);
        let terminal = agent_view.read(cx).terminal().clone();

        terminal.update(cx, |terminal, _| {
            terminal.input(text.into_bytes());
        });
        cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(ENTER_DELAY_MILLIS))
                .await;
            terminal.update(cx, |terminal, _| terminal.input(b"\r".to_vec()));
        })
        .detach();
    }

    /// Enables the auto-fix loop if it isn't already running.
    pub(crate) fn ensure_auto_fix_enabled(&mut self, cx: &mut Context<Self>) {
        if !self.auto_fix_enabled {
            self.toggle_auto_fix(cx);
        }
    }

    /// Toggles the poll → fix → push loop: GitHub checks are polled, and
    /// while they're failing the agent is periodically instructed to
    /// investigate, fix, and push, until checks go green or attempts run out.
    fn toggle_auto_fix(&mut self, cx: &mut Context<Self>) {
        if self.auto_fix_enabled {
            self.auto_fix_enabled = false;
            self.auto_fix_task = None;
            self.auto_fix_state = SharedString::default();
            cx.notify();
            return;
        }

        self.auto_fix_enabled = true;
        self.auto_fix_attempts = 0;
        self.last_auto_fix_at = None;
        self.auto_fix_state = SharedString::from("watching CI");
        let workspace_id = self.workspace_id.clone();
        self.auto_fix_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(AUTO_FIX_POLL_INTERVAL).await;
                let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
                    continue;
                };
                let status = {
                    let workspace_id = workspace_id.clone();
                    cx.background_spawn(async move {
                        mnemonic_core::services::pr_draft_service::get_workspace_pr_status(
                            &state,
                            &workspace_id,
                        )
                    })
                    .await
                };
                let Ok(status) = status else {
                    continue;
                };
                match this.update(cx, |this, cx| this.handle_auto_fix_status(&status, cx)) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => break,
                }
            }
        }));
        cx.notify();
    }

    /// One poll tick of the auto-fix loop. Returns false when the loop
    /// should stop.
    fn handle_auto_fix_status(
        &mut self,
        status: &mnemonic_core::models::WorkspacePrStatus,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.auto_fix_enabled {
            return false;
        }
        mnemonic_bridge::record_pr_status(&self.workspace_id, status);
        if !status.found {
            self.auto_fix_state = SharedString::from("waiting for a PR");
            cx.notify();
            return true;
        }

        let failing: Vec<&str> = status
            .checks
            .iter()
            .filter(|check| {
                matches!(
                    check.conclusion.as_deref(),
                    Some("FAILURE") | Some("TIMED_OUT") | Some("CANCELLED") | Some("ACTION_REQUIRED")
                )
            })
            .map(|check| check.name.as_str())
            .collect();
        let conflicts = status.mergeable.as_deref() == Some("CONFLICTING")
            || status.merge_state_status.as_deref() == Some("DIRTY");
        let any_pending = status
            .checks
            .iter()
            .any(|check| !check.status.eq_ignore_ascii_case("completed"));

        if !failing.is_empty() || conflicts {
            if self.auto_fix_attempts >= AUTO_FIX_MAX_ATTEMPTS {
                self.auto_fix_enabled = false;
                self.auto_fix_state =
                    SharedString::from(format!("gave up after {AUTO_FIX_MAX_ATTEMPTS} attempts"));
                self.show_workspace_toast(
                    format!(
                        "{}: CI still failing after {AUTO_FIX_MAX_ATTEMPTS} fix attempts, needs you",
                        self.workspace_name
                    ),
                    cx,
                );
                cx.notify();
                return false;
            }
            if self.agent_is_running() || self.any_chat_generating(cx) {
                self.auto_fix_state = SharedString::from("agent busy");
                cx.notify();
                return true;
            }
            if self
                .last_auto_fix_at
                .is_some_and(|at| at.elapsed() < AUTO_FIX_MIN_GAP)
            {
                return true;
            }

            self.auto_fix_attempts += 1;
            self.last_auto_fix_at = Some(Instant::now());
            let mut problems = Vec::new();
            if conflicts {
                problems.push("the PR has merge conflicts with the base branch".to_string());
            }
            if !failing.is_empty() {
                problems.push(format!("these CI checks failed: {}", failing.join(", ")));
            }
            let prompt = format!(
                "CI status on this PR needs fixing (attempt {}/{}): {}. \
                 Use `gh pr checks` and `gh run view --log-failed` to inspect the failures \
                 (and resolve any merge conflicts by merging the base branch), fix the \
                 problems, commit, and push.",
                self.auto_fix_attempts,
                AUTO_FIX_MAX_ATTEMPTS,
                problems.join("; "),
            );
            self.dispatch_to_agent(prompt, cx);
            self.auto_fix_state = SharedString::from(format!(
                "fixing ({}/{})",
                self.auto_fix_attempts, AUTO_FIX_MAX_ATTEMPTS
            ));
            self.show_workspace_toast(
                format!(
                    "{}: CI failing, agent fixing ({}/{})",
                    self.workspace_name, self.auto_fix_attempts, AUTO_FIX_MAX_ATTEMPTS
                ),
                cx,
            );
            cx.notify();
            return true;
        }

        if any_pending || status.checks.is_empty() {
            self.auto_fix_state = SharedString::from("CI running…");
            cx.notify();
            return true;
        }

        // Everything completed and nothing failed: done.
        self.auto_fix_enabled = false;
        self.auto_fix_state = SharedString::from("CI green ✓");
        self.show_workspace_toast(format!("{}: CI green ✓", self.workspace_name), cx);
        cx.notify();
        false
    }

    /// Raises a window toast, so background workspaces' auto-fix loops are
    /// heard from without being looked at.
    fn show_workspace_toast(&self, message: String, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<MnemonicAutoFixToast>(), message),
                    cx,
                );
            });
        }
    }

    /// Returns the agent provider of the active tab, defaulting to Claude for
    /// plain shells, chats, or when no tabs exist yet.
    fn active_provider(&self) -> AgentProvider {
        self.sessions
            .get(self.active_session)
            .and_then(WorkspaceSession::provider)
            .unwrap_or(AgentProvider::Claude)
    }

    /// Applies a model choice: live-switches the focused agent session via
    /// /model, and becomes the model for new sessions of that provider.
    fn set_model(&mut self, model: &str, cx: &mut Context<Self>) {
        if CLAUDE_MODELS.iter().any(|(id, _)| *id == model) {
            self.claude_model = model.to_string();
        } else if CODEX_MODELS.iter().any(|(id, _)| *id == model) {
            self.codex_model = model.to_string();
        }
        self.dispatch_from(
            DispatchOrigin::Composer,
            MessageSource::App,
            format!("/model {model}"),
            cx,
        );
        cx.notify();
    }

    fn model_label(&self) -> String {
        let current_model = match self.active_provider() {
            AgentProvider::Claude => &self.claude_model,
            AgentProvider::Codex => &self.codex_model,
        };
        CLAUDE_MODELS
            .iter()
            .chain(CODEX_MODELS.iter())
            .find(|(model, _)| *model == current_model.as_str())
            .map(|(_, label)| label.to_string())
            .unwrap_or_else(|| current_model.clone())
    }

    /// Applies an effort choice: live-switches the focused agent via
    /// /effort, and becomes the effort for new sessions.
    fn set_effort(&mut self, effort: &str, cx: &mut Context<Self>) {
        self.agent_effort = effort.to_string();
        self.dispatch_from(
            DispatchOrigin::Composer,
            MessageSource::App,
            format!("/effort {effort}"),
            cx,
        );
        cx.notify();
    }

    /// Sends Escape to the agent terminal, which interrupts Claude Code
    /// mid-generation.
    fn interrupt_agent(&mut self, cx: &mut Context<Self>) {
        let Some(agent_view) = self.first_terminal() else {
            return;
        };
        let terminal = agent_view.read(cx).terminal().clone();
        terminal.update(cx, |terminal, _| {
            terminal.input(b"\x1b".to_vec());
        });
    }

    /// Shown only while an agent is working; an idle workspace needs no chip.
    fn render_status_chip(&self, cx: &App) -> Option<impl IntoElement> {
        let running = self.agent_is_running() || self.any_chat_generating(cx);
        running.then(|| {
            h_flex()
                .px_1()
                .gap_1()
                .items_center()
                .child(
                    div()
                        .w(px(6.))
                        .h(px(6.))
                        .rounded_full()
                        .bg(cx.theme().status().success),
                )
                .child(
                    Label::new("Running")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
        })
    }

    /// Replaces a chat whose saved session can no longer be resumed with a
    /// fresh conversation in the same tab.
    fn restart_chat(&mut self, record_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.sessions.iter().position(|session| {
            session
                .chat()
                .is_some_and(|chat| chat.record_id == record_id)
        }) else {
            return;
        };
        let state = mnemonic_bridge::mnemonic_state(cx);
        let record = match chat_session_repository::list_for_workspace(&state.db, &self.workspace_id)
        {
            Ok(records) => records.into_iter().find(|record| record.id == record_id),
            Err(err) => {
                log::error!("Failed to load chat session: {err}");
                None
            }
        };
        let Some(mut record) = record else {
            return;
        };
        if let Err(err) = chat_session_repository::set_acp_session_id(&state.db, &record.id, None) {
            log::error!("Failed to clear chat session id: {err}");
        }
        record.acp_session_id = None;

        let Some(chat) = self.build_chat(&record, None, window, cx) else {
            return;
        };
        let view_id = chat.view.entity_id();
        let content = SessionContent::Chat {
            view: chat.view.clone(),
            record_id: chat.record_id.clone(),
            agent: chat.agent,
        };
        let Some(session) = self.sessions.get_mut(index) else {
            return;
        };
        session.item.update(cx, |item, cx| {
            item.content = content;
            cx.notify();
        });
        session.kind = SessionKind::Chat(chat);
        self.activate_session(index, window, cx);
        self.handle_chat_changed(view_id, cx);
    }

    fn render_resize_handle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("composer-resize-handle")
            .w_full()
            .h(px(5.))
            .cursor_row_resize()
            .hover(|style| style.bg(cx.theme().colors().border_focused))
            .on_drag(ComposerDrag, |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| ComposerDrag)
            })
    }

    /// Pasting an image into the composer saves it into the worktree (via
    /// Forge's attachment helper) and inserts the file path, so claude can
    /// read it. Text pastes fall through to the editor untouched.
    fn handle_composer_paste(
        &mut self,
        _: &editor::actions::Paste,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let image = item.entries().iter().find_map(|entry| match entry {
            gpui::ClipboardEntry::Image(image) => Some(image.clone()),
            _ => None,
        });
        let Some(image) = image else {
            return;
        };
        cx.stop_propagation();

        let extension = match image.format() {
            gpui::ImageFormat::Png => "png",
            gpui::ImageFormat::Jpeg => "jpg",
            gpui::ImageFormat::Webp => "webp",
            gpui::ImageFormat::Gif => "gif",
            _ => "png",
        };
        let state = mnemonic_bridge::mnemonic_state(cx);
        match mnemonic_core::services::workspace_file_tree_service::save_workspace_pasted_image(
            &state,
            &self.workspace_id,
            &format!("pasted.{extension}"),
            image.bytes(),
        ) {
            Ok(relative_path) => {
                self.pending_image_paths.push(relative_path);
                self.composer.update(cx, |composer, cx| {
                    composer.insert("[image] ", window, cx);
                });
            }
            Err(err) => log::error!("Failed to save pasted image: {err}"),
        }
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let chevron = || {
            Icon::new(IconName::ChevronDown)
                .size(IconSize::XSmall)
                .color(Color::Muted)
        };

        let model_menu = {
            let this = cx.weak_entity();
            let models: &[(&str, &str)] = match self.active_provider() {
                AgentProvider::Claude => CLAUDE_MODELS,
                AgentProvider::Codex => CODEX_MODELS,
            };
            let models = models.to_vec();
            PopoverMenu::new("composer-model-menu")
                .anchor(Anchor::BottomLeft)
                .trigger(
                    Button::new("composer-model", self.model_label())
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .end_icon(chevron())
                        .tooltip(Tooltip::text(
                            "Model for the focused agent (switches it live) and for new \
                             sessions",
                        )),
                )
                .menu(move |window, cx| {
                    let this = this.clone();
                    let models = models.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        for (model, label) in &models {
                            let this = this.clone();
                            let model = *model;
                            menu = menu.entry(*label, None, move |_, cx| {
                                this.update(cx, |this, cx| {
                                    this.set_model(model, cx);
                                })
                                .ok();
                            });
                        }
                        menu
                    }))
                })
        };

        let effort_menu = {
            let this = cx.weak_entity();
            PopoverMenu::new("composer-effort-menu")
                .anchor(Anchor::BottomLeft)
                .trigger(
                    Button::new("composer-effort", effort_label(&self.agent_effort))
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .end_icon(chevron())
                        .tooltip(Tooltip::text(
                            "Effort for the focused agent (switches it live) and for new \
                             sessions",
                        )),
                )
                .menu(move |window, cx| {
                    let this = this.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        for effort in AGENT_EFFORTS {
                            let this = this.clone();
                            menu = menu.entry(effort_label(effort), None, move |_, cx| {
                                this.update(cx, |this, cx| {
                                    this.set_effort(effort, cx);
                                })
                                .ok();
                            });
                        }
                        menu
                    }))
                })
        };

        let mode_button = Button::new(
            "composer-mode",
            format!("Mode: {}", self.agent_mode_label()),
        )
        .label_size(LabelSize::Small)
        .color(Color::Muted)
        .tooltip(Tooltip::text(
            "Cycle the focused agent's mode: sends shift+tab, the same as pressing \
             shift+tab in the terminal",
        ))
        .on_click(cx.listener(|this, _, _, cx| {
            this.cycle_agent_mode(cx);
        }));

        // Claude's permission mode is the Mode chip itself. Codex's chip only
        // covers plan vs default while its sessions always launch with
        // approvals and the sandbox bypassed, so that is stated separately.
        let codex_full_access = self.active_provider() == AgentProvider::Codex;

        v_flex()
            .key_context("MnemonicComposer")
            .on_action(cx.listener(Self::send_to_agent))
            .on_action(cx.listener(Self::cycle_agent_mode_action))
            .capture_action(cx.listener(Self::handle_composer_paste))
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .p_2()
            .w_full()
            .child(
                v_flex()
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        div()
                            .id("composer-card")
                            .w_full()
                            .h(self.composer_height)
                            .p_2()
                            .overflow_y_scroll()
                            .child(self.composer.clone()),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .px_1()
                            .pb_1()
                            .justify_between()
                            .items_center()
                            .child(
                                h_flex()
                                    .gap_0p5()
                                    .items_center()
                                    .child(model_menu)
                                    .child(effort_menu)
                                    .child(mode_button)
                                    .when(codex_full_access, |el| {
                                        el.child(
                                            Label::new("Full access")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                    }),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .child(
                                        IconButton::new("interrupt-agent", IconName::Stop)
                                            .icon_size(IconSize::Small)
                                            .icon_color(Color::Muted)
                                            .tooltip(Tooltip::text("Interrupt the agent"))
                                            .on_click(cx.listener(|this, _, _window, cx| {
                                                this.interrupt_agent(cx);
                                            })),
                                    )
                                    .child(
                                        IconButton::new("send-to-agent", IconName::ArrowUp)
                                            .icon_size(IconSize::Small)
                                            .style(ButtonStyle::Filled)
                                            .tooltip(Tooltip::text("Send"))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.send_now(window, cx);
                                            })),
                                    ),
                            ),
                    ),
            )
    }

    /// The buttons at the right of a pane's tab bar. Run, auto-fix and the
    /// status chip belong to the whole workspace, so only its first pane
    /// shows them.
    fn render_pane_controls(
        &self,
        this: WeakEntity<Self>,
        pane: Entity<Pane>,
        can_split: bool,
        cx: &App,
    ) -> AnyElement {
        let is_first_pane = self.center.first_pane() == pane;
        h_flex()
            .h_full()
            .pr_1()
            .gap_0p5()
            .items_center()
            .when(is_first_pane, |el| {
                el.child(self.render_run_controls(&this))
                    .when(!self.auto_fix_state.is_empty(), |el| {
                        el.child(
                            Label::new(self.auto_fix_state.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        IconButton::new("auto-fix-toggle", IconName::CheckDouble)
                            .icon_size(IconSize::Small)
                            .icon_color(if self.auto_fix_enabled {
                                Color::Accent
                            } else {
                                Color::Muted
                            })
                            .tooltip(Tooltip::text(if self.auto_fix_enabled {
                                "Keeping CI green: watching this PR's checks and having the \
                                 agent fix failures. Click to stop"
                            } else {
                                "Keep CI green: watch this PR's checks and have the agent fix \
                                 and push until they pass"
                            }))
                            .on_click({
                            let this = this.clone();
                            move |_, _, cx| {
                                this.update(cx, |this, cx| this.toggle_auto_fix(cx))
                                    .log_err();
                            }
                        }),
                    )
                    .children(self.render_status_chip(cx))
            })
            .when(can_split, |el| {
                let pane = pane.clone();
                el.child(
                    PopoverMenu::new("workspace-split-menu")
                        .anchor(Anchor::TopRight)
                        .trigger(
                            IconButton::new("workspace-split", IconName::Split)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Move this tab into a split")),
                        )
                        .menu(move |window, cx| {
                            let pane = pane.clone();
                            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                                [
                                    ("Split Right", SplitDirection::Right),
                                    ("Split Down", SplitDirection::Down),
                                ]
                                .into_iter()
                                .fold(menu, |menu, (label, direction)| {
                                    let pane = pane.clone();
                                    menu.entry(label, None, move |window, cx| {
                                        pane.update(cx, |pane, cx| {
                                            pane.split(direction, SplitMode::MovePane, window, cx);
                                        });
                                    })
                                })
                            }))
                        }),
                )
            })
            .child(
                PopoverMenu::new("workspace-add-session-menu")
                    .anchor(Anchor::TopRight)
                    .trigger(
                        IconButton::new("workspace-add-session", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New session in this workspace")),
                    )
                    .menu(move |window, cx| {
                        let this = this.clone();
                        let pane = pane.clone();
                        let agents = ChatAgent::enabled(cx);
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            let chats = agents.into_iter().map(|agent| {
                                (format!("{} chat", agent.label()), NewSession::Chat(agent))
                            });
                            let terminals = [
                                ("Claude CLI", NewSession::ClaudeCli),
                                ("Codex CLI", NewSession::CodexCli),
                                ("Terminal", NewSession::Shell),
                            ]
                            .into_iter()
                            .map(|(label, kind)| (label.to_string(), kind));
                            chats.chain(terminals).fold(menu, |menu, (label, kind)| {
                                let this = this.clone();
                                let pane = pane.clone();
                                menu.entry(label, None, move |window, cx| {
                                    this.update(cx, |this, cx| {
                                        this.active_pane = pane.clone();
                                        this.add_session(kind, window, cx);
                                    })
                                    .log_err();
                                })
                            })
                        }))
                    }),
            )
            .into_any_element()
    }
}

impl Focusable for WorkspaceView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_pane.focus_handle(cx)
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let show_composer = !self.active_is_chat();
        let panes = self
            .workspace
            .update(cx, |workspace, cx| {
                self.center
                    .render(
                        None,
                        &PaneRenderContext {
                            follower_states: &Default::default(),
                            active_call: workspace.active_call(),
                            active_pane: &self.active_pane,
                            app_state: workspace.app_state(),
                            project: workspace.project(),
                            workspace: &workspace.weak_handle(),
                        },
                        window,
                        cx,
                    )
                    .into_any_element()
            })
            .log_err();

        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<ComposerDrag>, _window, cx| {
                    // Distance from the cursor to the bottom of this view,
                    // minus the composer chrome (buttons row + paddings).
                    let height = event.bounds.bottom() - event.event.position.y - px(60.);
                    this.composer_height = height.clamp(px(60.), px(600.));
                    cx.notify();
                },
            ))
            .on_action(cx.listener(|this, _: &ActivatePaneLeft, window, cx| {
                this.activate_pane_in_direction(SplitDirection::Left, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivatePaneRight, window, cx| {
                this.activate_pane_in_direction(SplitDirection::Right, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivatePaneUp, window, cx| {
                this.activate_pane_in_direction(SplitDirection::Up, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivatePaneDown, window, cx| {
                this.activate_pane_in_direction(SplitDirection::Down, window, cx);
            }))
            .child(
                div()
                    .flex_grow(1.0)
                    .w_full()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .children(panes),
            )
            .when(show_composer, |el| {
                el.child(self.render_resize_handle(cx))
                    .child(self.render_composer(cx))
            })
    }
}

/// The single center item that hosts workspace views. The left panel is the
/// switcher: selecting a workspace swaps which view this item shows, instead
/// of stacking one tab per workspace. Views are cached, so switching back is
/// instant and terminals keep running.
pub struct WorkspaceHost {
    views: HashMap<String, Entity<WorkspaceView>>,
    active: Option<String>,
    focus_handle: FocusHandle,
}

impl WorkspaceHost {
    /// Ensures the host item exists in a pane, activates it, and switches it
    /// to the given workspace.
    pub fn activate_workspace(
        workspace: &mut Workspace,
        summary: &WorkspaceSummary,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let panes = workspace.panes().to_vec();
        let mut existing = None;
        for pane in panes {
            let found = pane.read(cx).items().enumerate().find_map(|(index, item)| {
                item.downcast::<WorkspaceHost>().map(|host| (index, host))
            });
            if let Some((index, host)) = found {
                existing = Some((pane, index, host));
                break;
            }
        }

        let host = match existing {
            Some((pane, index, host)) => {
                pane.update(cx, |pane, cx| {
                    pane.activate_item(index, true, true, window, cx);
                });
                host
            }
            None => {
                let host = cx.new(|cx| WorkspaceHost {
                    views: HashMap::new(),
                    active: None,
                    focus_handle: cx.focus_handle(),
                });
                workspace.add_item_to_active_pane(Box::new(host.clone()), None, true, window, cx);
                host
            }
        };

        let workspace_handle = workspace.weak_handle();
        let project = workspace.project().clone();
        let database_id = workspace.database_id();
        host.update(cx, |host, cx| {
            host.set_active(summary, workspace_handle, project, database_id, window, cx);
        });
    }

    fn set_active(
        &mut self,
        summary: &WorkspaceSummary,
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        database_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.views.contains_key(&summary.id) {
            let worktree_path = summary
                .workspace_root_path
                .as_deref()
                .or(summary.repository_path.as_deref())
                .or(summary.selected_worktree_path.as_deref())
                .map(PathBuf::from)
                .filter(|path| path.exists());

            let composer = cx.new(|cx| {
                let mut editor = Editor::auto_height(3, 32, window, cx);
                editor.set_placeholder_text(
                    "Send instruction to the agent - Enter to send, Shift+Enter for newline",
                    window,
                    cx,
                );
                editor
            });

            let view = cx.new(|cx| {
                let pane = WorkspaceView::new_session_pane(
                    workspace.clone(),
                    project.clone(),
                    window,
                    cx,
                );
                WorkspaceView {
                    _window_activation: cx.observe_window_activation(
                        window,
                        |view: &mut WorkspaceView, window, cx| {
                            view.window_activation_changed(window, cx);
                        },
                    ),
                    // Panes are cached views: their tab bar buttons only pick
                    // up this view's state when the panes are redrawn too.
                    _pane_refresh: cx.observe_self(|view, cx| {
                        for pane in view.center.panes() {
                            pane.update(cx, |_, cx| cx.notify());
                        }
                    }),
                    window_handle: window.window_handle(),
                    window_active: window.is_window_active(),
                    last_cli_activity: None,
                    cli_quiet_reported: false,
                    notification_windows: Vec::new(),
                    notification_subscriptions: Vec::new(),
                    workspace_id: summary.id.clone(),
                    workspace_name: SharedString::from(summary.name.clone()),
                    worktree_path,
                    workspace,
                    project: project.downgrade(),
                    database_id,
                    sessions: Vec::new(),
                    active_session: 0,
                    center: PaneGroup::new(pane.clone()),
                    active_pane: pane,
                    terminal_pane: None,
                    connection_store: None,
                    composer,
                    composer_height: px(110.),
                    claude_model: "claude-opus-5-5".to_string(),
                    codex_model: "gpt-6.1-sol".to_string(),
                    agent_effort: "high".to_string(),
                    claude_mode_index: INITIAL_CLAUDE_MODE_INDEX,
                    codex_mode_index: INITIAL_CODEX_MODE_INDEX,
                    pending_image_paths: Vec::new(),
                    last_agent_activity: None,
                    idle_check_scheduled: false,
                    auto_fix_enabled: false,
                    auto_fix_attempts: 0,
                    last_auto_fix_at: None,
                    auto_fix_state: SharedString::default(),
                    auto_fix_task: None,
                    branch_rename_started: false,
                    run_command_count: 0,
                    run_tooltip: SharedString::from("Reading .mnemonic/config.json…"),
                    run_starting: false,
                }
            });
            let state = mnemonic_bridge::mnemonic_state(cx);
            let chat_records =
                match chat_session_repository::list_for_workspace(&state.db, &summary.id) {
                    Ok(records) => records,
                    Err(err) => {
                        log::error!("Failed to load chat sessions: {err}");
                        Vec::new()
                    }
                };
            view.update(cx, |view, cx| {
                let start_main_cli = should_start_main_cli(&chat_records);
                for record in chat_records {
                    view.open_chat(record, None, window, cx);
                }
                if start_main_cli {
                    view.spawn_primary_agent(window, cx);
                } else {
                    view.activate_session(0, window, cx);
                }
            });
            self.views.insert(summary.id.clone(), view);
        }

        self.active = Some(summary.id.clone());
        if let Some(view) = self.views.get(&summary.id) {
            view.update(cx, |view, cx| {
                view.load_run_config(cx);
                view.mark_seen(cx);
            });
            let focus_handle = view.focus_handle(cx);
            window.focus(&focus_handle, cx);
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn active_view(&self) -> Option<&Entity<WorkspaceView>> {
        self.active.as_ref().and_then(|id| self.views.get(id))
    }

    /// Drops the cached view for an archived workspace, clearing the host's
    /// display if it was the active one.
    pub fn remove_workspace_view(
        workspace: &mut Workspace,
        workspace_id: &str,
        cx: &mut Context<Workspace>,
    ) {
        for pane in workspace.panes().to_vec() {
            let host = pane
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<WorkspaceHost>());
            if let Some(host) = host {
                host.update(cx, |host, cx| {
                    if let Some(view) = host.views.remove(workspace_id) {
                        view.update(cx, |view, cx| view.close_all_chats(cx));
                    }
                    if host.active.as_deref() == Some(workspace_id) {
                        host.active = None;
                        cx.emit(ItemEvent::UpdateTab);
                    }
                    cx.notify();
                });
                return;
            }
        }
    }

    /// Applies a rename made from the Projects panel to the workspace's open
    /// view, which then stops naming the workspace after its first message.
    pub(crate) fn workspace_renamed(
        workspace: &mut Workspace,
        workspace_id: &str,
        name: &str,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(panel) = workspace.panel::<WorkspaceDetailPanel>(cx) {
            panel.update(cx, |_, cx| cx.notify());
        }
        for pane in workspace.panes().to_vec() {
            let host = pane
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<WorkspaceHost>());
            if let Some(host) = host {
                host.update(cx, |host, cx| {
                    if let Some(view) = host.views.get(workspace_id) {
                        view.update(cx, |view, cx| {
                            view.workspace_name = SharedString::from(name.to_string());
                            view.branch_rename_started = true;
                            cx.notify();
                        });
                    }
                    cx.emit(ItemEvent::UpdateTab);
                });
                return;
            }
        }
    }

    /// Kills the run commands of the given workspace's view, if it exists in
    /// this window.
    pub fn kill_run_commands_for_workspace(
        workspace: &mut Workspace,
        workspace_id: &str,
        cx: &mut Context<Workspace>,
    ) {
        for pane in workspace.panes().to_vec() {
            let host = pane
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<WorkspaceHost>());
            if let Some(host) = host {
                if let Some(view) = host.read(cx).views.get(workspace_id).cloned() {
                    view.update(cx, |view, cx| view.kill_run_terminals(cx));
                }
                return;
            }
        }
    }

    /// Runs teardown commands in the given workspace's view, which must
    /// already be open in this window. Returns None when it isn't.
    pub(crate) fn run_teardown_for_workspace(
        workspace: &mut Workspace,
        workspace_id: &str,
        root: PathBuf,
        plans: Vec<RunCommandPlan>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<gpui::Task<()>> {
        for pane in workspace.panes().to_vec() {
            let host = pane
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<WorkspaceHost>());
            if let Some(host) = host {
                let view = host.read(cx).views.get(workspace_id).cloned()?;
                return Some(view.update(cx, |view, cx| {
                    view.run_teardown_commands(root, plans, window, cx)
                }));
            }
        }
        None
    }

    /// Opens (or refocuses) the review claude session for the given
    /// workspace, if its view exists in this window.
    pub fn start_review_for_workspace(
        workspace: &mut Workspace,
        workspace_id: &str,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        for pane in workspace.panes().to_vec() {
            let host = pane
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<WorkspaceHost>());
            if let Some(host) = host {
                let view = host.read(cx).views.get(workspace_id).cloned();
                if let Some(view) = view {
                    view.update(cx, |view, cx| view.start_review(window, cx));
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// Turns on the CI auto-fix loop for the given workspace, if its view
    /// exists in this window.
    pub fn enable_auto_fix_for_workspace(
        workspace: &mut Workspace,
        workspace_id: &str,
        cx: &mut Context<Workspace>,
    ) -> bool {
        for pane in workspace.panes().to_vec() {
            let host = pane
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<WorkspaceHost>());
            if let Some(host) = host {
                let view = host.read(cx).views.get(workspace_id).cloned();
                if let Some(view) = view {
                    view.update(cx, |view, cx| view.ensure_auto_fix_enabled(cx));
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// Whether an agent in the given workspace is mid-turn: a chat is
    /// generating or an agent terminal produced output recently.
    pub fn workspace_agent_busy(workspace: &Workspace, workspace_id: &str, cx: &App) -> bool {
        for pane in workspace.panes() {
            let host = pane
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<WorkspaceHost>());
            if let Some(host) = host {
                return host.read(cx).views.get(workspace_id).is_some_and(|view| {
                    let view = view.read(cx);
                    view.agent_is_running() || view.any_chat_generating(cx)
                });
            }
        }
        false
    }

    /// Shows the given workspace in the host (creating the host and view if
    /// they were closed) and opens a new session of the given kind in it.
    /// Returns false when the workspace no longer exists.
    pub fn add_session_for_workspace(
        workspace: &mut Workspace,
        workspace_id: &str,
        kind: AgentSessionKind,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let state = mnemonic_bridge::mnemonic_state(cx);
        let summary = match workspace_repository::list(&state.db) {
            Ok(summaries) => summaries
                .into_iter()
                .find(|summary| summary.id == workspace_id),
            Err(err) => {
                log::error!("Failed to list workspaces: {err}");
                None
            }
        };
        let Some(summary) = summary else {
            return false;
        };
        Self::activate_workspace(workspace, &summary, window, cx);

        for pane in workspace.panes().to_vec() {
            let host = pane
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<WorkspaceHost>());
            if let Some(host) = host {
                let view = host.read(cx).views.get(workspace_id).cloned();
                if let Some(view) = view {
                    view.update(cx, |view, cx| view.add_session(kind.into(), window, cx));
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// Sends an instruction to the agent terminal of the given workspace, if
    /// its view exists in this window. Returns false when the workspace has
    /// no live view (it hasn't been opened yet).
    pub fn dispatch_to_workspace_agent(
        workspace: &mut Workspace,
        workspace_id: &str,
        text: String,
        cx: &mut Context<Workspace>,
    ) -> bool {
        for pane in workspace.panes().to_vec() {
            let host = pane
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<WorkspaceHost>());
            if let Some(host) = host {
                let view = host.read(cx).views.get(workspace_id).cloned();
                if let Some(view) = view {
                    view.update(cx, |view, cx| view.dispatch_to_agent(text, cx));
                    return true;
                }
                return false;
            }
        }
        false
    }
}

impl Focusable for WorkspaceHost {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_view()
            .map(|view| view.focus_handle(cx))
            .unwrap_or_else(|| self.focus_handle.clone())
    }
}

impl EventEmitter<ItemEvent> for WorkspaceHost {}

impl Item for WorkspaceHost {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.active_view()
            .map(|view| view.read(cx).workspace_name.clone())
            .unwrap_or_else(|| SharedString::from("Workspace"))
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileGit))
    }

    fn hides_tab_bar_when_alone(&self) -> bool {
        true
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn handle_drop(
        &self,
        _active_pane: &Pane,
        dropped: &dyn Any,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let (Some(paths), Some(view)) =
            (dropped.downcast_ref::<ExternalPaths>(), self.active_view())
        else {
            return false;
        };
        insert_dropped_paths(view, paths, window, cx)
    }
}

impl Render for WorkspaceHost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .map(|el| match self.active_view().cloned() {
                Some(view) => el.child(view),
                None => el.child(
                    v_flex().size_full().items_center().justify_center().child(
                        Label::new("Select a workspace")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                ),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(agent: ChatAgent, is_review: bool) -> SessionSummary {
        SessionSummary::Chat {
            agent,
            is_review,
            failed: false,
        }
    }

    fn terminal(title: &str) -> SessionSummary {
        SessionSummary::Terminal {
            is_agent: title.starts_with("claude") || title.starts_with("codex"),
        }
    }

    #[test]
    fn dispatch_prefers_active_chat_then_first_claude_chat() {
        let sessions = [
            chat(ChatAgent::Codex, false),
            chat(ChatAgent::Claude, false),
            chat(ChatAgent::Claude, true),
            terminal("claude"),
        ];
        assert_eq!(pick_dispatch_target(DispatchOrigin::External, &sessions, 0), Some(DispatchTarget::Chat(0)));
        assert_eq!(pick_dispatch_target(DispatchOrigin::External, &sessions, 2), Some(DispatchTarget::Chat(1)));
        assert_eq!(pick_dispatch_target(DispatchOrigin::External, &sessions, 3), Some(DispatchTarget::Chat(1)));
    }

    #[test]
    fn dispatch_falls_back_to_cli_without_chats() {
        let sessions = [terminal("claude"), terminal("shell 1")];
        assert_eq!(pick_dispatch_target(DispatchOrigin::External, &sessions, 1), Some(DispatchTarget::Terminal(0)));
        assert_eq!(pick_dispatch_target(DispatchOrigin::External, &sessions, 0), Some(DispatchTarget::Terminal(0)));
        assert_eq!(pick_dispatch_target(DispatchOrigin::External, &[terminal("shell 1")], 0), None);
        assert_eq!(pick_dispatch_target(DispatchOrigin::External, &[chat(ChatAgent::Claude, true)], 0), None);
    }

    #[test]
    fn dispatch_skips_chats_that_failed_to_load() {
        let sessions = [
            SessionSummary::Chat {
                agent: ChatAgent::Claude,
                is_review: false,
                failed: true,
            },
            terminal("claude"),
        ];
        assert_eq!(
            pick_dispatch_target(DispatchOrigin::External, &sessions, 0),
            Some(DispatchTarget::Terminal(1))
        );
    }

    #[test]
    fn composer_text_goes_to_the_cli_tab_being_viewed() {
        let sessions = [chat(ChatAgent::Claude, false), terminal("claude-2")];
        assert_eq!(
            pick_dispatch_target(DispatchOrigin::Composer, &sessions, 1),
            Some(DispatchTarget::Terminal(1))
        );
        assert_eq!(
            pick_dispatch_target(DispatchOrigin::External, &sessions, 1),
            Some(DispatchTarget::Chat(0))
        );
    }

    #[test]
    fn workspaces_not_created_chat_first_keep_their_main_cli() {
        let record = |is_primary: bool| ChatSessionRecord {
            id: "chat".to_string(),
            workspace_id: "ws".to_string(),
            agent: "codex-acp".to_string(),
            title: "Codex".to_string(),
            acp_session_id: None,
            is_review: false,
            is_primary,
            position: 0,
        };
        assert!(should_start_main_cli(&[]));
        assert!(should_start_main_cli(&[record(false)]));
        assert!(!should_start_main_cli(&[record(true)]));
    }

    #[test]
    fn turn_end_events_release_queued_instructions() {
        assert!(releases_pending(&acp_thread::AcpThreadEvent::Stopped(
            acp::StopReason::EndTurn
        )));
        assert!(releases_pending(&acp_thread::AcpThreadEvent::Error));
        assert!(!releases_pending(
            &acp_thread::AcpThreadEvent::TokenUsageUpdated
        ));
    }

    #[test]
    fn dispatch_to_unconnected_or_busy_chat_is_queued_in_order() {
        let mut pending = PendingMessages::default();
        pending.push("a".to_string());
        pending.push("b".to_string());
        assert_eq!(pending.take_next().as_deref(), Some("a"));
        assert_eq!(pending.take_next().as_deref(), Some("b"));
        assert_eq!(pending.take_next(), None);
    }

}
