use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use editor::Editor;
use gpui::{
    Anchor, App, AppContext as _, Context, DragMoveEvent, Empty, Entity, EventEmitter, Focusable,
    FocusHandle, IntoElement, Pixels, Render, SharedString, Subscription, WeakEntity, Window,
    actions, px,
};
use mnemonic_core::models::WorkspaceSummary;
use mnemonic_core::services::terminal_service;
use project::Project;
use task::{RevealStrategy, Shell, SpawnInTerminal, TaskId};
use terminal::Terminal;
use terminal_view::TerminalView;
use ui::{
    Button, ButtonStyle, ContextMenu, Icon, IconButton, IconName, Label, PopoverMenu, TintColor,
    Tooltip, h_flex, prelude::*, v_flex,
};
use workspace::item::{Item, ItemEvent};
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace, WorkspaceId};

/// Marker for auto-fix loop toasts.
struct MnemonicAutoFixToast;

actions!(
    mnemonic_composer,
    [
        /// Send the composer contents to the workspace's agent terminal.
        Send
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

/// Auto-fix loop: how often to poll GitHub for check results.
const AUTO_FIX_POLL_INTERVAL: Duration = Duration::from_secs(45);
/// Auto-fix loop: minimum gap between fix prompts, so the agent isn't
/// re-prompted while its previous fix is still building on CI.
const AUTO_FIX_MIN_GAP: Duration = Duration::from_secs(180);
/// Auto-fix loop: give up after this many fix attempts.
const AUTO_FIX_MAX_ATTEMPTS: u32 = 5;

/// Model string passed to claude (also valid for its /model command) and
/// the chip label shown in the composer. [1m] = 1M-context variant.
const AGENT_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-8", "Opus 4.8"),
    ("claude-opus-4-7", "Opus 4.7"),
    ("claude-opus-4-6", "Opus 4.6"),
    ("claude-opus-4-6[1m]", "Opus 4.6 1M"),
    ("claude-fable-5", "Fable 5"),
    ("claude-fable-5[1m]", "Fable 5 1M"),
    ("claude-sonnet-4-6", "Sonnet 4.6"),
    ("claude-sonnet-4-6[1m]", "Sonnet 4.6 1M"),
    ("claude-haiku-4-5", "Haiku 4.5"),
];

/// Effort levels for the effort dropdown. xhigh and max are only supported
/// by some models; claude reports it visibly if the combination is invalid.
const AGENT_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

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
    terminals: Vec<Entity<TerminalView>>,
    terminal_titles: Vec<SharedString>,
    active_terminal: usize,
    composer: Entity<Editor>,
    composer_height: Pixels,
    agent_model: String,
    agent_effort: String,
    agent_bypass: bool,
    last_agent_activity: Option<Instant>,
    idle_check_scheduled: bool,
    auto_fix_enabled: bool,
    auto_fix_attempts: u32,
    last_auto_fix_at: Option<Instant>,
    auto_fix_state: SharedString,
    auto_fix_task: Option<gpui::Task<()>>,
    _terminal_subscriptions: Vec<Subscription>,
    focus_handle: FocusHandle,
}

impl WorkspaceView {
    /// Starts the workspace's primary agent terminal, attached to its
    /// persistent tmux session.
    fn spawn_agent_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.spawn_claude_terminal("main", "claude", None, None, window, cx);
    }

    /// Opens an additional claude session in this workspace, with its own
    /// persistent tmux session.
    fn add_claude_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let claude_count = self
            .terminal_titles
            .iter()
            .filter(|title| title.starts_with("claude"))
            .count();
        let session = format!("claude-{}", claude_count + 1);
        let title = format!("claude {}", claude_count + 1);
        self.spawn_claude_terminal(&session, &title, None, None, window, cx);
    }

    /// Opens (or refocuses) the review session: a separate claude on Sonnet
    /// that is asked to review the workspace's diff.
    pub(crate) fn start_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self
            .terminal_titles
            .iter()
            .position(|title| title.as_ref() == "review")
        {
            self.active_terminal = index;
            if let Some(view) = self.terminals.get(index) {
                let focus_handle = view.focus_handle(cx);
                window.focus(&focus_handle, cx);
            }
            cx.notify();
            return;
        }

        let prompt = "Review the changes in this worktree. Run `git status` and diff against \
                      the base branch (try `git diff origin/main...HEAD`, falling back to the \
                      repo's default branch). Assess correctness, security, performance, and \
                      style. Give a structured review: blocking issues first, then suggestions, \
                      then what looks good. Do not make code changes unless asked."
            .to_string();
        self.spawn_claude_terminal(
            "review",
            "review",
            Some("claude-sonnet-4-6".to_string()),
            Some(prompt),
            window,
            cx,
        );
    }

    /// Starts a claude terminal attached to the given persistent tmux
    /// session of this workspace, optionally typing an initial prompt into
    /// it once it has had a moment to boot.
    fn spawn_claude_terminal(
        &mut self,
        session: &str,
        title: &str,
        model_override: Option<String>,
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
        let model = model_override.unwrap_or_else(|| self.agent_model.clone());
        let effort = self.agent_effort.clone();
        let bypass = self.agent_bypass;

        cx.spawn_in(window, async move |this, cx| {
            // Command resolution may shell out to tmux; keep it off the UI thread.
            let (command, args) = cx
                .background_spawn({
                    let workspace_id = workspace_id.clone();
                    let session = session.clone();
                    let cwd = cwd.clone();
                    async move {
                        let mut agent_args = Vec::new();
                        if bypass {
                            agent_args.push("--dangerously-skip-permissions".to_string());
                        }
                        agent_args.push("--model".to_string());
                        agent_args.push(model);
                        agent_args.push("--effort".to_string());
                        agent_args.push(effort);
                        match &cwd {
                            Some(dir) => terminal_service::embedded_workspace_terminal_command(
                                &workspace_id,
                                &session,
                                dir,
                                "claude",
                                &agent_args,
                            ),
                            None => ("claude".to_string(), agent_args),
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
                    this.update_in(cx, |this, window, cx| {
                        this.push_terminal(&title, terminal, window, cx);
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
        let title = format!("shell {}", self.terminals.len());

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
                        this.push_terminal(&title, terminal, window, cx);
                    })
                    .ok();
                }
                Err(e) => log::error!("Failed to open terminal: {e}"),
            }
        })
        .detach();
    }

    fn push_terminal(
        &mut self,
        title: &str,
        terminal: Entity<Terminal>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The first terminal is the agent's; track its output to derive the
        // running/waiting status chip.
        if self.terminals.is_empty() {
            let subscription = cx.subscribe(&terminal, |this, _terminal, event, cx| {
                if matches!(event, terminal::Event::Wakeup) {
                    this.last_agent_activity = Some(Instant::now());
                    mnemonic_bridge::record_agent_activity(&this.workspace_id);
                    this.schedule_idle_check(cx);
                    cx.notify();
                }
            });
            self._terminal_subscriptions.push(subscription);
        }

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
        self.terminals.push(view.clone());
        self.terminal_titles
            .push(SharedString::from(title.to_string()));
        self.active_terminal = self.terminals.len() - 1;
        let focus_handle = view.focus_handle(cx);
        window.focus(&focus_handle, cx);
        cx.notify();
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
        let text = self.composer.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }
        self.dispatch_to_agent(text, cx);
        self.composer.update(cx, |composer, cx| composer.clear(window, cx));
    }

    /// Writes an instruction into the agent terminal: text first, then the
    /// submitting Enter as a separate input event (Forge's dispatch pattern).
    /// Targets the active tab when it's a claude session, otherwise the
    /// primary agent terminal.
    pub(crate) fn dispatch_to_agent(&mut self, text: String, cx: &mut Context<Self>) {
        let active_is_claude = self
            .terminal_titles
            .get(self.active_terminal)
            .is_some_and(|title| title.starts_with("claude"));
        let agent_view = if active_is_claude {
            self.terminals.get(self.active_terminal)
        } else {
            self.terminals.first()
        };
        let Some(agent_view) = agent_view else {
            return;
        };
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
            if self.agent_is_running() {
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

    /// Applies a model choice: live-switches the focused claude session via
    /// /model, and becomes the launch flag for new sessions.
    fn set_model(&mut self, model: &str, cx: &mut Context<Self>) {
        self.agent_model = model.to_string();
        self.dispatch_to_agent(format!("/model {model}"), cx);
        cx.notify();
    }

    fn model_label(&self) -> String {
        AGENT_MODELS
            .iter()
            .find(|(model, _)| *model == self.agent_model)
            .map(|(_, label)| label.to_string())
            .unwrap_or_else(|| self.agent_model.clone())
    }

    /// Applies an effort choice: live-switches via /effort, and becomes the
    /// launch flag for new sessions.
    fn set_effort(&mut self, effort: &str, cx: &mut Context<Self>) {
        self.agent_effort = effort.to_string();
        self.dispatch_to_agent(format!("/effort {effort}"), cx);
        cx.notify();
    }

    /// Sends Escape to the agent terminal, which interrupts Claude Code
    /// mid-generation.
    fn interrupt_agent(&mut self, cx: &mut Context<Self>) {
        let Some(agent_view) = self.terminals.first() else {
            return;
        };
        let terminal = agent_view.read(cx).terminal().clone();
        terminal.update(cx, |terminal, _| {
            terminal.input(b"\x1b".to_vec());
        });
    }

    fn render_status_chip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.agent_is_running();
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
                self.composer.update(cx, |composer, cx| {
                    composer.insert(&format!("{relative_path} "), window, cx);
                });
            }
            Err(err) => log::error!("Failed to save pasted image: {err}"),
        }
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("MnemonicComposer")
            .on_action(cx.listener(Self::send_to_agent))
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
                            "composer-bypass",
                            if self.agent_bypass { "Bypass: on" } else { "Bypass: off" },
                        )
                        .style(if self.agent_bypass {
                            ButtonStyle::Tinted(TintColor::Warning)
                        } else {
                            ButtonStyle::Subtle
                        })
                        .label_size(LabelSize::XSmall)
                        .tooltip(Tooltip::text(
                            "Whether NEW claude sessions skip permission prompts \
                             (running sessions: shift+tab in the terminal)",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.agent_bypass = !this.agent_bypass;
                            cx.notify();
                        })),
                    )
                    .child({
                        let this = cx.weak_entity();
                        PopoverMenu::new("composer-model-menu")
                            .anchor(Anchor::BottomLeft)
                            .trigger(
                                Button::new(
                                    "composer-model",
                                    format!("{} ⌄", self.model_label()),
                                )
                                .label_size(LabelSize::XSmall)
                                .tooltip(Tooltip::text(
                                    "Model for the focused claude (switches it live) and \
                                     for new sessions",
                                )),
                            )
                            .menu(move |window, cx| {
                                let this = this.clone();
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    for (model, label) in AGENT_MODELS {
                                        let this = this.clone();
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
                                    "Effort for the focused claude (switches it live) and \
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

        for index in 0..self.terminals.len() {
            let title = self
                .terminal_titles
                .get(index)
                .cloned()
                .unwrap_or_else(|| SharedString::from("terminal"));
            let active = index == self.active_terminal;
            tabs = tabs.child(
                div()
                    .id(("workspace-terminal-tab", index))
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(active, |el| {
                        el.bg(cx.theme().colors().ghost_element_selected)
                    })
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.active_terminal = index;
                        if let Some(view) = this.terminals.get(index) {
                            let focus_handle = view.focus_handle(cx);
                            window.focus(&focus_handle, cx);
                        }
                        cx.notify();
                    }))
                    .child(Label::new(title).size(LabelSize::XSmall).color(if active {
                        Color::Default
                    } else {
                        Color::Muted
                    })),
            );
        }

        tabs.child(div().flex_grow(1.0))
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
            .child(
                Button::new("workspace-add-claude", "+ Claude")
                    .label_size(LabelSize::XSmall)
                    .tooltip(Tooltip::text(
                        "New claude session in this workspace (own persistent tmux session)",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.add_claude_terminal(window, cx);
                    })),
            )
            .child(
                IconButton::new("workspace-add-terminal", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("New shell in this workspace"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.add_shell_terminal(window, cx);
                    })),
            )
    }
}

impl Focusable for WorkspaceView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.terminals
            .get(self.active_terminal)
            .map(|view| view.focus_handle(cx))
            .unwrap_or_else(|| self.focus_handle.clone())
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_terminal = self.terminals.get(self.active_terminal).cloned();

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
                    .map(|el| match active_terminal {
                        Some(view) => el.child(view),
                        None => el.child(
                            v_flex().size_full().items_center().justify_center().child(
                                Label::new("Starting claude…")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        ),
                    }),
            )
            .child(self.render_resize_handle(cx))
            .child(self.render_composer(cx))
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
                    "Send instruction to claude — Enter to send, Shift+Enter for newline",
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
                terminals: Vec::new(),
                terminal_titles: Vec::new(),
                active_terminal: 0,
                composer,
                composer_height: px(110.),
                agent_model: "claude-opus-4-8".to_string(),
                agent_effort: "high".to_string(),
                agent_bypass: true,
                last_agent_activity: None,
                idle_check_scheduled: false,
                auto_fix_enabled: false,
                auto_fix_attempts: 0,
                last_auto_fix_at: None,
                auto_fix_state: SharedString::default(),
                auto_fix_task: None,
                _terminal_subscriptions: Vec::new(),
                focus_handle: cx.focus_handle(),
            });
            view.update(cx, |view, cx| view.spawn_agent_terminal(window, cx));
            self.views.insert(summary.id.clone(), view);
        }

        self.active = Some(summary.id.clone());
        if let Some(view) = self.views.get(&summary.id) {
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
                    host.views.remove(workspace_id);
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
