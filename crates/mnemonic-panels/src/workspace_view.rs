use std::path::PathBuf;
use std::time::Duration;

use editor::Editor;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Focusable, FocusHandle, IntoElement,
    Render, SharedString, WeakEntity, Window, actions, px,
};
use mnemonic_core::models::WorkspaceSummary;
use mnemonic_core::services::terminal_service;
use project::Project;
use task::{RevealStrategy, Shell, SpawnInTerminal, TaskId};
use terminal::Terminal;
use terminal_view::TerminalView;
use ui::{Icon, IconButton, IconName, Label, Tooltip, h_flex, prelude::*, v_flex};
use workspace::item::Item;
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
    focus_handle: FocusHandle,
}

impl WorkspaceView {
    /// Activates the existing view for this workspace if any pane holds one,
    /// otherwise creates it and starts the agent terminal inside it.
    pub fn activate_or_open(
        workspace: &mut Workspace,
        summary: &WorkspaceSummary,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let panes = workspace.panes().to_vec();
        for pane in panes {
            let index = pane.read(cx).items().position(|item| {
                item.downcast::<WorkspaceView>()
                    .is_some_and(|view| view.read(cx).workspace_id == summary.id)
            });
            if let Some(index) = index {
                pane.update(cx, |pane, cx| {
                    pane.activate_item(index, true, true, window, cx);
                });
                return;
            }
        }

        let worktree_path = summary
            .workspace_root_path
            .as_deref()
            .or(summary.repository_path.as_deref())
            .or(summary.selected_worktree_path.as_deref())
            .map(PathBuf::from)
            .filter(|path| path.exists());

        let workspace_handle = workspace.weak_handle();
        let project = workspace.project().downgrade();
        let database_id = workspace.database_id();

        let composer = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 6, window, cx);
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
            workspace: workspace_handle,
            project,
            database_id,
            terminals: Vec::new(),
            terminal_titles: Vec::new(),
            active_terminal: 0,
            composer,
            focus_handle: cx.focus_handle(),
        });
        view.update(cx, |view, cx| view.spawn_agent_terminal(window, cx));
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

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

    /// Sends the composer contents to the agent terminal (the first tab),
    /// using Forge's text-then-Enter dispatch so the TUI submits it.
    fn send_to_agent(&mut self, _: &Send, window: &mut Window, cx: &mut Context<Self>) {
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

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("MnemonicComposer")
            .on_action(cx.listener(Self::send_to_agent))
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .p_2()
            .w_full()
            .child(self.composer.clone())
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

        tabs.child(div().flex_grow(1.0)).child(
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

impl EventEmitter<()> for WorkspaceView {}

impl Item for WorkspaceView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.workspace_name.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileGit))
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_terminal = self.terminals.get(self.active_terminal).cloned();

        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
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
            .child(self.render_composer(cx))
    }
}
