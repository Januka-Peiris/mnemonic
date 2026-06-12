use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use editor::Editor;
use gpui::{
    App, AppContext as _, Context, DragMoveEvent, Empty, Entity, EventEmitter, Focusable,
    FocusHandle, IntoElement, Pixels, Render, SharedString, Subscription, WeakEntity, Window,
    actions, px,
};
use mnemonic_core::models::WorkspaceSummary;
use mnemonic_core::services::terminal_service;
use project::Project;
use task::{RevealStrategy, Shell, SpawnInTerminal, TaskId};
use terminal::Terminal;
use terminal_view::TerminalView;
use ui::{Button, ButtonStyle, Icon, IconButton, IconName, Label, Tooltip, h_flex, prelude::*, v_flex};
use workspace::item::{Item, ItemEvent};
use workspace::{Workspace, WorkspaceId};

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
    last_agent_activity: Option<Instant>,
    idle_check_scheduled: bool,
    _terminal_subscriptions: Vec<Subscription>,
    focus_handle: FocusHandle,
}

impl WorkspaceView {
    /// Starts the workspace's primary agent terminal, attached to its
    /// persistent tmux session.
    fn spawn_agent_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace_id = self.workspace_id.clone();
        let workspace_name = self.workspace_name.to_string();
        let cwd = self.worktree_path.clone();
        let project = self.project.clone();

        cx.spawn_in(window, async move |this, cx| {
            // Command resolution may shell out to tmux; keep it off the UI thread.
            let (command, args) = cx
                .background_spawn({
                    let workspace_id = workspace_id.clone();
                    let cwd = cwd.clone();
                    async move {
                        let agent_args = vec!["--dangerously-skip-permissions".to_string()];
                        match &cwd {
                            Some(dir) => terminal_service::embedded_workspace_terminal_command(
                                &workspace_id,
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
                id: TaskId(format!("mnemonic-{workspace_id}")),
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
                    this.update_in(cx, |this, window, cx| {
                        this.push_terminal("claude", terminal, window, cx);
                    })
                    .ok();
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
        let Some(agent_view) = self.terminals.first() else {
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

        self.composer.update(cx, |composer, cx| composer.clear(window, cx));
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

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("MnemonicComposer")
            .on_action(cx.listener(Self::send_to_agent))
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .p_2()
            .gap_1p5()
            .w_full()
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
            .child(self.render_status_chip(cx))
            .child(
            IconButton::new("workspace-add-terminal", IconName::Plus)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("New Terminal in Workspace"))
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
                last_agent_activity: None,
                idle_check_scheduled: false,
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
