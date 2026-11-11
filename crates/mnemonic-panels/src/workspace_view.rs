use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent_client_protocol::schema as acp;
use agent_ui::{Agent, AgentConnectionStore, AgentThreadSource, ConversationView};
use editor::Editor;
use gpui::{
    Anchor, App, AppContext as _, Context, DragMoveEvent, Empty, Entity, EntityId, EventEmitter,
    Focusable, FocusHandle, IntoElement, Pixels, Render, SharedString, Subscription, WeakEntity,
    Window, actions, px,
};
use mnemonic_core::models::{ChatSessionRecord, WorkspaceSummary};
use mnemonic_core::repositories::{chat_session_repository, workspace_repository};
use mnemonic_core::services::{
    checkpoint_service, terminal_service, workspace_script_service, workspace_service,
};
use project::{AgentId, Project};
use task::{RevealStrategy, Shell, SpawnInTerminal, TaskId};
use terminal::Terminal;
use terminal_view::TerminalView;
use util::path_list::PathList;
use util::shell_builder::ShellBuilder;
use ui::{
    Button, ButtonStyle, ContextMenu, Icon, IconButton, IconName, Label, PopoverMenu, TintColor,
    Tooltip, h_flex, prelude::*, v_flex,
};
use workspace::item::{Item, ItemEvent};
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace, WorkspaceId};
use zed_actions::mnemonic::AgentSessionKind;

use crate::workspace_detail_panel::WorkspaceDetailPanel;
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

/// The ACP agents a workspace chat can run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ChatAgent {
    Claude,
    Codex,
}

impl ChatAgent {
    fn agent_id(self) -> &'static str {
        match self {
            Self::Claude => "claude-acp",
            Self::Codex => "codex-acp",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }

    fn from_agent_id(agent_id: &str) -> Self {
        if agent_id == Self::Codex.agent_id() {
            Self::Codex
        } else {
            Self::Claude
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

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.kind {
            SessionKind::Terminal(terminal) => terminal.view.focus_handle(cx),
            SessionKind::Chat(chat) => chat.view.focus_handle(cx),
        }
    }
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
/// user switches between; terminals (the agent's and any extra shells) live
/// inside it as inner tabs, all rooted in the workspace's worktree.
pub struct WorkspaceView {
    workspace_id: String,
    workspace_name: SharedString,
    worktree_path: Option<PathBuf>,
    workspace: WeakEntity<Workspace>,
    project: WeakEntity<Project>,
    database_id: Option<WorkspaceId>,
    sessions: Vec<WorkspaceSession>,
    active_session: usize,
    connection_store: Option<Entity<AgentConnectionStore>>,
    composer: Entity<Editor>,
    composer_height: Pixels,
    claude_model: String,
    codex_model: String,
    agent_effort: String,
    claude_mode_index: usize,
    codex_mode_index: usize,
    review_provider: AgentProvider,
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
    focus_handle: FocusHandle,
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
        if chat_first {
            let agent = match self.review_provider {
                AgentProvider::Claude => ChatAgent::Claude,
                AgentProvider::Codex => ChatAgent::Codex,
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

        let provider = self.review_provider;
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

    /// Closes a terminal tab and kills its tmux session so it doesn't
    /// linger.
    fn close_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(SessionKind::Terminal(terminal)) = self.sessions.get(index).map(|s| &s.kind)
        else {
            return;
        };
        if let Some(session_name) = terminal.session_name.clone() {
            let workspace_id = self.workspace_id.clone();
            cx.background_spawn(async move {
                if let Err(err) =
                    terminal_service::kill_embedded_workspace_terminal(&workspace_id, &session_name)
                {
                    log::warn!("Failed to kill tmux session: {err}");
                }
            })
            .detach();
        }
        self.sessions.remove(index);
        self.after_session_removed(index, window, cx);
    }

    /// Keeps the same tab active after a tab is removed, when possible.
    fn after_session_removed(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.sessions.is_empty() {
            self.active_session = 0;
            cx.notify();
            return;
        }
        if index < self.active_session {
            self.active_session -= 1;
        }
        let next = self.active_session.min(self.sessions.len() - 1);
        self.activate_session(next, window, cx);
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
        self.sessions.push(WorkspaceSession {
            title: SharedString::from(title.to_string()),
            kind: SessionKind::Terminal(TerminalSession {
                view,
                provider,
                session_name,
                run_command,
                _subscription: subscription,
            }),
        });
        self.activate_session(self.sessions.len() - 1, window, cx);
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
                &self.sessions[index].kind,
                SessionKind::Terminal(terminal) if terminal.run_command
            );
            if is_run {
                self.sessions.remove(index);
                if index < self.active_session {
                    self.active_session -= 1;
                }
            }
        }
        if self.sessions.is_empty() {
            self.active_session = 0;
            cx.notify();
            return;
        }
        let next = self.active_session.min(self.sessions.len() - 1);
        self.activate_session(next, window, cx);
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

    fn render_run_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.has_run_terminals() {
            return h_flex()
                .gap_0p5()
                .child(
                    Button::new("workspace-run-restart", "Restart")
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::XSmall)
                        .disabled(self.run_starting)
                        .tooltip(Tooltip::text("Stop and re-run the workspace's run commands"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.restart_run_commands(window, cx);
                        })),
                )
                .child(
                    Button::new("workspace-run-stop", "Stop")
                        .style(ButtonStyle::Tinted(TintColor::Error))
                        .label_size(LabelSize::XSmall)
                        .tooltip(Tooltip::text("Stop the run commands and close their tabs"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.stop_run_commands(window, cx);
                        })),
                )
                .into_any_element();
        }
        Button::new(
            "workspace-run",
            if self.run_starting { "Starting…" } else { "Run" },
        )
        .style(ButtonStyle::Subtle)
        .label_size(LabelSize::XSmall)
        .disabled(self.run_starting || self.run_command_count == 0)
        .tooltip(Tooltip::text(self.run_tooltip.clone()))
        .on_click(cx.listener(|this, _, window, cx| {
            this.start_run_commands("run_command", window, cx);
        }))
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
        self.active_session = index;
        let focus_handle = session.focus_handle(cx);
        window.focus(&focus_handle, cx);
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

    /// Opens a chat tab for a saved chat session, resuming the agent's
    /// session when one was recorded. An initial prompt is queued and sent
    /// once the agent is connected and idle.
    fn open_chat(
        &mut self,
        record: ChatSessionRecord,
        initial_prompt: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
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
        let work_dirs = self
            .worktree_path
            .as_ref()
            .map(|path| PathList::new(&[path]));
        let title = SharedString::from(record.title.clone());
        let workspace = self.workspace.clone();
        let app_messages = initial_prompt.iter().cloned().collect();

        let view = cx.new(|cx| {
            ConversationView::new(
                server,
                connection_store,
                agent,
                resume_session_id,
                None,
                work_dirs,
                Some(title.clone()),
                None,
                workspace,
                project,
                None,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            )
        });

        let view_id = view.entity_id();
        let observation = cx.observe(&view, move |this, _view, cx| {
            this.handle_chat_changed(view_id, cx);
        });

        self.sessions.push(WorkspaceSession {
            title,
            kind: SessionKind::Chat(ChatSession {
                record_id: record.id,
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
            }),
        });
        self.activate_session(self.sessions.len() - 1, window, cx);
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

    /// Closes a chat tab and forgets it, so it does not come back on
    /// restart.
    fn close_chat(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(chat) = self.sessions.get(index).and_then(WorkspaceSession::chat) else {
            return;
        };
        let view = chat.view.clone();
        let record_id = chat.record_id.clone();
        self.sessions.remove(index);

        let close_task = view.update(cx, |view, cx| {
            view.as_connected()
                .map(|connected| connected.close_all_sessions(cx))
        });
        if let Some(close_task) = close_task {
            close_task.detach();
        }
        let state = mnemonic_bridge::mnemonic_state(cx);
        if let Err(err) = chat_session_repository::delete(&state.db, &record_id) {
            log::error!("Failed to delete chat session: {err}");
        }

        self.after_session_removed(index, window, cx);
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
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
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
                        "{}: auto-fix gave up after {AUTO_FIX_MAX_ATTEMPTS} attempts — needs you",
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
                "CI status on this PR needs fixing (auto-fix attempt {}/{}): {}. \
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
                    "{}: CI failing — claude fixing ({}/{})",
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

    fn render_status_chip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.agent_is_running() || self.any_chat_generating(cx);
        let (color, text) = if running {
            (cx.theme().status().success, "Running")
        } else {
            (cx.theme().colors().text_muted, "Waiting")
        };

        h_flex()
            .px_1p5()
            .py_0p5()
            .gap_1()
            .items_center()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(div().w(px(6.)).h(px(6.)).rounded_full().bg(color))
            .child(
                Label::new(text)
                    .size(LabelSize::XSmall)
                    .color(if running { Color::Default } else { Color::Muted }),
            )
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

        self.sessions.remove(index);
        self.open_chat(record, None, window, cx);
        let last = self.sessions.len() - 1;
        let reopened = self.sessions.remove(last);
        self.sessions.insert(index, reopened);
        self.activate_session(index, window, cx);
    }

    fn render_chat(&self, chat: &ChatSession, cx: &mut Context<Self>) -> impl IntoElement {
        let failed = chat.view.read(cx).has_load_error();
        let record_id = chat.record_id.clone();
        let agent = chat.agent;
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
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.restart_chat(record_id.clone(), window, cx);
                                })),
                        )
                        .child(
                            Button::new(
                                "chat-open-cli-instead",
                                format!("Open {} CLI instead", agent.label()),
                            )
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| match agent {
                                ChatAgent::Claude => this.add_claude_terminal(window, cx),
                                ChatAgent::Codex => this.add_codex_terminal(window, cx),
                            })),
                        ),
                )
            })
            .child(div().flex_grow(1.0).min_h(px(0.)).child(chat.view.clone()))
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
        v_flex()
            .key_context("MnemonicComposer")
            .on_action(cx.listener(Self::send_to_agent))
            .on_action(cx.listener(Self::cycle_agent_mode_action))
            .capture_action(cx.listener(Self::handle_composer_paste))
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .p_2()
            .gap_1p5()
            .w_full()
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        Button::new(
                            "composer-mode",
                            format!("Mode: {}", self.agent_mode_label()),
                        )
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::XSmall)
                        .tooltip(Tooltip::text(
                            "Cycle the focused agent's mode: sends shift+tab, \
                             the same as pressing shift+tab in the terminal",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cycle_agent_mode(cx);
                        })),
                    )
                    .child({
                        let this = cx.weak_entity();
                        let models: &[(&str, &str)] = match self.active_provider() {
                            AgentProvider::Claude => CLAUDE_MODELS,
                            AgentProvider::Codex => CODEX_MODELS,
                        };
                        let models = models.to_vec();
                        PopoverMenu::new("composer-model-menu")
                            .anchor(Anchor::BottomLeft)
                            .trigger(
                                Button::new(
                                    "composer-model",
                                    format!("{} ⌄", self.model_label()),
                                )
                                .label_size(LabelSize::XSmall)
                                .tooltip(Tooltip::text(
                                    "Model for the focused agent (switches it live) and \
                                     for new sessions",
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
                    })
                    .child({
                        let this = cx.weak_entity();
                        PopoverMenu::new("composer-effort-menu")
                            .anchor(Anchor::BottomLeft)
                            .trigger(
                                Button::new(
                                    "composer-effort",
                                    format!("{} ⌄", self.agent_effort.to_uppercase()),
                                )
                                .label_size(LabelSize::XSmall)
                                .tooltip(Tooltip::text(
                                    "Effort for the focused agent (switches it live) and \
                                     for new sessions",
                                )),
                            )
                            .menu(move |window, cx| {
                                let this = this.clone();
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    for effort in AGENT_EFFORTS {
                                        let this = this.clone();
                                        menu = menu.entry(
                                            effort.to_uppercase(),
                                            None,
                                            move |_, cx| {
                                                this.update(cx, |this, cx| {
                                                    this.set_effort(effort, cx);
                                                })
                                                .ok();
                                            },
                                        );
                                    }
                                    menu
                                }))
                            })
                    }),
            )
            .child(
                div()
                    .id("composer-card")
                    .w_full()
                    .h(self.composer_height)
                    .p_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .overflow_y_scroll()
                    .child(self.composer.clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .child(
                        Label::new("bypass permissions on")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("interrupt-agent", "Interrupt")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.interrupt_agent(cx);
                                    })),
                            )
                            .child(
                                Button::new("send-to-agent", "Send")
                                    .style(ButtonStyle::Filled)
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.send_now(window, cx);
                                    })),
                            ),
                    ),
            )
    }

    fn render_terminal_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut tabs = h_flex()
            .px_1()
            .h(px(30.))
            .gap_0p5()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border);

        for (index, session) in self.sessions.iter().enumerate() {
            let title = session.title.clone();
            let is_chat = session.chat().is_some();
            let active = index == self.active_session;
            tabs = tabs.child(
                h_flex()
                    .id(("workspace-session-tab", index))
                    .gap_0p5()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(active, |el| {
                        el.bg(cx.theme().colors().ghost_element_selected)
                    })
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_session(index, window, cx);
                    }))
                    .child(Label::new(title).size(LabelSize::XSmall).color(if active {
                        Color::Default
                    } else {
                        Color::Muted
                    }))
                    .child(
                        IconButton::new(("workspace-close-session", index), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text(if is_chat { "Close chat" } else { "Close this terminal" }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if is_chat {
                                    this.close_chat(index, window, cx);
                                } else {
                                    this.close_terminal(index, window, cx);
                                }
                            })),
                    ),
            );
        }

        tabs.child(div().flex_grow(1.0))
            .child(self.render_run_controls(cx))
            .when(!self.auto_fix_state.is_empty(), |el| {
                el.child(
                    Label::new(self.auto_fix_state.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .child(
                Button::new(
                    "auto-fix-toggle",
                    if self.auto_fix_enabled {
                        "Auto-fix: on"
                    } else {
                        "Auto-fix"
                    },
                )
                .style(if self.auto_fix_enabled {
                    ButtonStyle::Tinted(TintColor::Accent)
                } else {
                    ButtonStyle::Subtle
                })
                .label_size(LabelSize::XSmall)
                .tooltip(Tooltip::text(
                    "Poll CI; when checks fail, instruct claude to fix and push until green",
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.toggle_auto_fix(cx);
                })),
            )
            .child(self.render_status_chip(cx))
            .child({
                let this = cx.entity().downgrade();
                PopoverMenu::new("workspace-add-session-menu")
                    .anchor(Anchor::TopRight)
                    .trigger(
                        IconButton::new("workspace-add-session", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New session in this workspace")),
                    )
                    .menu(move |window, cx| {
                        let this = this.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            let entries: [(&str, NewSession); 5] = [
                                ("Claude chat", NewSession::Chat(ChatAgent::Claude)),
                                ("Codex chat", NewSession::Chat(ChatAgent::Codex)),
                                ("Claude CLI", NewSession::ClaudeCli),
                                ("Codex CLI", NewSession::CodexCli),
                                ("Shell", NewSession::Shell),
                            ];
                            entries.into_iter().fold(menu, |menu, (label, kind)| {
                                let this = this.clone();
                                menu.entry(label, None, move |window, cx| {
                                    this.update(cx, |this, cx| this.add_session(kind, window, cx))
                                        .ok();
                                })
                            })
                        }))
                    })
            })
    }
}

impl Focusable for WorkspaceView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.sessions
            .get(self.active_session)
            .map(|session| session.focus_handle(cx))
            .unwrap_or_else(|| self.focus_handle.clone())
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_content = self.sessions.get(self.active_session).map(|session| {
            match &session.kind {
                SessionKind::Terminal(terminal) => terminal.view.clone().into_any_element(),
                SessionKind::Chat(chat) => self.render_chat(chat, cx).into_any_element(),
            }
        });
        let show_composer = !self.active_is_chat();

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
            .child(self.render_terminal_tabs(cx))
            .child(
                div()
                    .flex_grow(1.0)
                    .w_full()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .map(|el| match active_content {
                        Some(content) => el.child(content),
                        None => el.child(
                            v_flex().size_full().items_center().justify_center().child(
                                Label::new("Starting…")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        ),
                    }),
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
        let project = workspace.project().downgrade();
        let database_id = workspace.database_id();
        host.update(cx, |host, cx| {
            host.set_active(summary, workspace_handle, project, database_id, window, cx);
        });
    }

    fn set_active(
        &mut self,
        summary: &WorkspaceSummary,
        workspace: WeakEntity<Workspace>,
        project: WeakEntity<Project>,
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

            let view = cx.new(|cx| WorkspaceView {
                workspace_id: summary.id.clone(),
                workspace_name: SharedString::from(summary.name.clone()),
                worktree_path,
                workspace,
                project,
                database_id,
                sessions: Vec::new(),
                active_session: 0,
                connection_store: None,
                composer,
                composer_height: px(110.),
                claude_model: "claude-opus-5-5".to_string(),
                codex_model: "gpt-6.1-sol".to_string(),
                agent_effort: "high".to_string(),
                claude_mode_index: INITIAL_CLAUDE_MODE_INDEX,
                codex_mode_index: INITIAL_CODEX_MODE_INDEX,
                review_provider: AgentProvider::Codex,
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
                focus_handle: cx.focus_handle(),
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
            view.update(cx, |view, cx| view.load_run_config(cx));
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

    /// Sets the review provider for a workspace view.
    pub fn set_review_provider_for_workspace(
        workspace: &mut Workspace,
        workspace_id: &str,
        provider: &str,
        cx: &mut Context<Workspace>,
    ) {
        let target = match provider {
            "claude" => AgentProvider::Claude,
            _ => AgentProvider::Codex,
        };
        for pane in workspace.panes().to_vec() {
            let host = pane
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<WorkspaceHost>());
            if let Some(host) = host {
                if let Some(view) = host.read(cx).views.get(workspace_id).cloned() {
                    view.update(cx, |view, cx| {
                        view.review_provider = target;
                        cx.notify();
                    });
                }
                return;
            }
        }
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

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
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
