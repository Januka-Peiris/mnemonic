use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use gpui::{
    actions, px, Action, App, AppContext as _, Context, Entity, EntityId, EventEmitter, Focusable,
    FocusHandle, IntoElement, PathPromptOptions, Pixels, Render, SharedString, WeakEntity, Window,
};
use mnemonic_core::models::{CreateWorkspaceInput, DiscoveredRepository, WorkspaceSummary};
use mnemonic_core::repositories::repository_repository;
use mnemonic_core::services::{repo_scanner_service, workspace_service};
use task::{RevealStrategy, Shell, SpawnInTerminal, TaskId};
use terminal_view::terminal_panel::TerminalPanel;
use ui::{Tooltip, h_flex, v_flex, Icon, IconButton, IconName, Label, prelude::*};
use workspace::dock::{DockPosition, PanelEvent};
use workspace::{Panel, Workspace};

actions!(mnemonic_workspace_panel, [ToggleFocus]);

const WORKSPACE_PANEL_KEY: &str = "MnemonicWorkspacePanel";

pub struct WorkspacePanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    repositories: Vec<DiscoveredRepository>,
    workspaces: Vec<WorkspaceSummary>,
    collapsed_projects: Vec<String>,
    selected_workspace_id: Option<String>,
    open_terminals: HashMap<String, EntityId>,
}

impl WorkspacePanel {
    pub fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let mut panel = Self {
                focus_handle: cx.focus_handle(),
                workspace: workspace.weak_handle(),
                repositories: Vec::new(),
                workspaces: Vec::new(),
                collapsed_projects: Vec::new(),
                selected_workspace_id: None,
                open_terminals: HashMap::new(),
            };
            panel.refresh(cx);
            panel
        })
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            Self::new(workspace, window, cx)
        })
    }

    fn refresh(&mut self, cx: &App) {
        let state = mnemonic_bridge::mnemonic_state(cx);
        match workspace_service::list_workspaces(&state) {
            Ok(ws) => self.workspaces = ws,
            Err(e) => log::error!("Failed to list workspaces: {e}"),
        }
        match repository_repository::list(&state.db) {
            Ok(repos) => self.repositories = repos,
            Err(e) => log::error!("Failed to list repositories: {e}"),
        }
    }

    fn grouped_workspaces(&self) -> BTreeMap<String, Vec<&WorkspaceSummary>> {
        let mut groups: BTreeMap<String, Vec<&WorkspaceSummary>> = BTreeMap::new();
        for repo in &self.repositories {
            groups.entry(repo.name.clone()).or_default();
        }
        for ws in &self.workspaces {
            let project = if ws.repo.is_empty() {
                "ungrouped".to_string()
            } else {
                ws.repo.clone()
            };
            groups.entry(project).or_default().push(ws);
        }
        groups
    }

    fn toggle_project(&mut self, project: &str) {
        if let Some(pos) = self.collapsed_projects.iter().position(|p| p == project) {
            self.collapsed_projects.remove(pos);
        } else {
            self.collapsed_projects.push(project.to_string());
        }
    }

    fn select_workspace(
        &mut self,
        ws_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_workspace_id = Some(ws_id.clone());
        cx.notify();

        if let Some(existing_entity_id) = self.open_terminals.get(&ws_id).copied() {
            if let Some(workspace) = self.workspace.upgrade() {
                let found = workspace.update(cx, |workspace, _cx| {
                    let pane = workspace.active_pane().clone();
                    let idx = pane.read(_cx).items().enumerate().find_map(|(idx, item)| {
                        (item.item_id() == existing_entity_id).then_some(idx)
                    });
                    idx.map(|idx| (pane, idx))
                });
                if let Some((pane, idx)) = found {
                    pane.update(cx, |pane, cx| {
                        pane.activate_item(idx, true, true, window, cx);
                    });
                    return;
                }
            }
            self.open_terminals.remove(&ws_id);
        }

        self.launch_terminal_for_workspace(&ws_id, window, cx);
    }

    fn launch_terminal_for_workspace(
        &mut self,
        ws_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ws = self.workspaces.iter().find(|ws| ws.id == ws_id);
        let ws_name = ws.map(|w| w.name.clone()).unwrap_or_default();
        let cwd = ws
            .and_then(|ws| {
                ws.workspace_root_path
                    .as_deref()
                    .or(ws.repository_path.as_deref())
                    .or(ws.selected_worktree_path.as_deref())
            })
            .map(PathBuf::from)
            .filter(|p| p.exists());

        let workspace = self.workspace.clone();
        let ws_id_owned = ws_id.to_string();
        let spawn_task = SpawnInTerminal {
            id: TaskId(format!("mnemonic-{ws_id}")),
            full_label: ws_name.clone(),
            label: ws_name,
            command: Some("claude".to_string()),
            args: vec!["--dangerously-skip-permissions".to_string()],
            command_label: "claude --dangerously-skip-permissions".to_string(),
            cwd,
            shell: Shell::System,
            reveal: RevealStrategy::Always,
            use_new_terminal: true,
            allow_concurrent_runs: true,
            ..Default::default()
        };

        cx.spawn_in(window, async move |this, cx| {
            let result = workspace
                .update_in(cx, |workspace, window, cx| {
                    TerminalPanel::add_center_terminal(
                        workspace,
                        window,
                        cx,
                        move |project, cx| project.create_terminal_task(spawn_task, cx),
                    )
                })
                .ok();

            if let Some(task) = result {
                if let Ok(weak_terminal) = task.await {
                    if let Some(entity_id) = weak_terminal.upgrade().map(|t| t.entity_id()) {
                        this.update(cx, |this, cx| {
                            this.open_terminals.insert(ws_id_owned, entity_id);
                            cx.notify();
                        })
                        .ok();
                    }
                }
            }
        })
        .detach();
    }

    fn add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths_receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });

        cx.spawn_in(window, async move |this, cx| {
            let paths = paths_receiver.await.ok().and_then(|r| r.ok()).flatten();
            let Some(paths) = paths else {
                return;
            };
            let Some(path) = paths.first() else {
                return;
            };

            let now = chrono::Utc::now().to_rfc3339();
            match repo_scanner_service::build_repository(path, &now) {
                Ok(repo) => {
                    this.read_with(cx, |_this, cx| {
                        let state = mnemonic_bridge::mnemonic_state(cx);
                        repository_repository::upsert(&state.db, &repo).ok();
                    })
                    .ok();

                    this.update(cx, |this, cx| {
                        this.refresh(cx);
                        cx.notify();
                    })
                    .ok();

                    log::info!("Added project: {}", path.display());
                }
                Err(e) => {
                    log::error!("Failed to register repository: {e}");
                }
            }
        })
        .detach();
    }

    fn add_worktree(
        &mut self,
        project_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let repo = self
            .repositories
            .iter()
            .find(|r| r.name == project_name)
            .cloned();

        let workspace = self.workspace.clone();

        cx.spawn_in(window, async move |this, cx| {
            let Some(repo) = repo else {
                log::error!("Repository not found for project: {project_name}");
                return;
            };

            let timestamp = chrono::Utc::now().format("%m%d-%H%M%S").to_string();
            let workspace_name = format!("{}-{}", repo.name, timestamp);
            let branch_name = format!("jay/{}", workspace_name);

            let result = this.read_with(cx, |_this, cx| {
                let state = mnemonic_bridge::mnemonic_state(cx);
                let input = CreateWorkspaceInput {
                    name: workspace_name,
                    repo: repo.name.clone(),
                    base_branch: "main".to_string(),
                    branch: Some(branch_name),
                    agent: "claude".to_string(),
                    task_prompt: String::new(),
                    open_in_cursor: false,
                    run_tests: false,
                    create_pr: true,
                    repository_id: Some(repo.id.clone()),
                    selected_worktree_id: None,
                    selected_branch: None,
                    parent_workspace_id: None,
                    source_workspace_id: None,
                    derived_from_branch: None,
                };
                workspace_service::create_workspace(&state, input)
            });

            match result {
                Ok(Ok(detail)) => {
                    log::info!("Created workspace: {}", detail.summary.id);

                    this.update(cx, |this, cx| {
                        this.refresh(cx);
                        cx.notify();
                    })
                    .ok();

                    let ws_name = detail.summary.name.clone();
                    let ws_id = detail.summary.id.clone();
                    let cwd = if detail.worktree_path.is_empty() {
                        detail
                            .summary
                            .workspace_root_path
                            .map(PathBuf::from)
                    } else {
                        Some(PathBuf::from(&detail.worktree_path))
                    }
                    .filter(|p| p.exists());

                    let spawn_task = SpawnInTerminal {
                        id: TaskId(format!("mnemonic-{ws_id}")),
                        full_label: ws_name.clone(),
                        label: ws_name,
                        command: Some("claude".to_string()),
                        args: vec!["--dangerously-skip-permissions".to_string()],
                        command_label: "claude --dangerously-skip-permissions".to_string(),
                        cwd,
                        shell: Shell::System,
                        reveal: RevealStrategy::Always,
                        use_new_terminal: true,
                        allow_concurrent_runs: true,
                        ..Default::default()
                    };

                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            TerminalPanel::add_center_terminal(
                                workspace,
                                window,
                                cx,
                                move |project, cx| project.create_terminal_task(spawn_task, cx),
                            )
                            .detach_and_log_err(cx);
                        })
                        .ok();
                }
                Ok(Err(e)) => log::error!("Failed to create workspace: {e}"),
                Err(e) => log::error!("Failed to access state: {e}"),
            }
        })
        .detach();
    }

    fn render_history_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("history-bar")
            .px_2()
            .h(px(30.))
            .gap_1p5()
            .items_center()
            .w_full()
            .cursor_pointer()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(|_this, _, _window, _cx| {
                log::info!("History clicked");
            }))
            .child(
                Icon::new(IconName::Clock)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new("History")
                    .size(LabelSize::Default)
                    .color(Color::Default),
            )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_2()
            .h(px(28.))
            .justify_between()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("Projects")
                    .size(LabelSize::Default)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("filter-projects", IconName::Filter)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Filter"))
                            .on_click(cx.listener(|_this, _, _window, _cx| {
                                log::info!("Filter clicked");
                            })),
                    )
                    .child(
                        IconButton::new("add-project", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New Project"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_project(window, cx);
                            })),
                    ),
            )
    }

    fn render_project_group(
        &self,
        project: &str,
        workspaces: &[&WorkspaceSummary],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let collapsed = self.collapsed_projects.contains(&project.to_string());
        let count = workspaces.len();
        let project_owned = project.to_string();
        let chevron_icon = if collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };

        let project_for_add = project.to_string();
        let mut group = v_flex().w_full();

        group = group.child(
            h_flex()
                .id(SharedString::from(format!("project-{project}")))
                .px_2()
                .h(px(28.))
                .gap_1()
                .items_center()
                .w_full()
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.toggle_project(&project_owned);
                    cx.notify();
                }))
                .child(
                    Icon::new(chevron_icon)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(project.to_string())
                        .size(LabelSize::Default)
                        .color(Color::Default),
                )
                .child(
                    Label::new(count.to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(div().flex_grow(1.0))
                .child(
                    IconButton::new(
                        SharedString::from(format!("add-worktree-{project}")),
                        IconName::Plus,
                    )
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("New Worktree"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_worktree(project_for_add.clone(), window, cx);
                    })),
                ),
        );

        if !collapsed {
            for ws in workspaces {
                group = group.child(self.render_workspace_row(ws, cx));
            }
        }

        group
    }

    fn render_workspace_row(
        &self,
        ws: &WorkspaceSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self
            .selected_workspace_id
            .as_ref()
            .is_some_and(|id| id == &ws.id);
        let ws_id = ws.id.clone();

        h_flex()
            .id(SharedString::from(ws.id.clone()))
            .pl(px(28.))
            .pr_2()
            .h(px(24.))
            .gap_1p5()
            .items_center()
            .w_full()
            .cursor_pointer()
            .rounded_md()
            .when(is_selected, |el| {
                el.bg(cx.theme().colors().ghost_element_selected)
            })
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_workspace(ws_id.clone(), window, cx);
            }))
            .child(
                Icon::new(IconName::FileGit)
                    .size(IconSize::XSmall)
                    .color(if is_selected {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .child(
                Label::new(ws.name.clone())
                    .size(LabelSize::Default)
                    .color(Color::Default)
                    .single_line(),
            )
    }

    fn render_empty(&self) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                Label::new("No projects yet")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }
}

impl Focusable for WorkspacePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorkspacePanel {}

impl Panel for WorkspacePanel {
    fn persistent_name() -> &'static str {
        "MnemonicWorkspacePanel"
    }

    fn panel_key() -> &'static str {
        WORKSPACE_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(260.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::Server)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Projects")
    }

    fn icon_label(&self, _window: &Window, _cx: &App) -> Option<String> {
        let count = self.workspaces.len();
        if count > 0 {
            Some(count.to_string())
        } else {
            None
        }
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        1
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }
}

impl Render for WorkspacePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh(cx);

        let groups = self.grouped_workspaces();
        let has_workspaces = !groups.is_empty();

        let mut content = v_flex()
            .id("workspace-panel")
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .child(self.render_history_bar(cx))
            .child(self.render_header(cx));

        if has_workspaces {
            let mut list = v_flex().py_0p5().overflow_y_hidden().flex_grow(1.0);
            for (project, workspaces) in &groups {
                list = list.child(self.render_project_group(project, workspaces, cx));
            }
            content = content.child(list);
        } else {
            content = content.child(self.render_empty());
        }

        content
    }
}
