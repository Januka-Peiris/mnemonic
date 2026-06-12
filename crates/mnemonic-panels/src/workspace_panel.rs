use std::collections::BTreeMap;
use std::path::PathBuf;

use gpui::{
    actions, px, Action, App, AppContext as _, Context, Entity, EventEmitter, Focusable,
    FocusHandle, IntoElement, PathPromptOptions, Pixels, Render, SharedString, WeakEntity, Window,
};
use mnemonic_core::models::{CreateWorkspaceInput, DiscoveredRepository, WorkspaceSummary};
use mnemonic_core::repositories::repository_repository;
use mnemonic_core::services::{repo_scanner_service, workspace_service};
use ui::{Tooltip, h_flex, v_flex, Icon, IconButton, IconName, Label, prelude::*};
use workspace::dock::{DockPosition, PanelEvent};
use workspace::{Panel, Workspace};

use crate::workspace_view::WorkspaceHost;

actions!(mnemonic_workspace_panel, [ToggleFocus]);

const WORKSPACE_PANEL_KEY: &str = "MnemonicWorkspacePanel";

pub struct WorkspacePanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    repositories: Vec<DiscoveredRepository>,
    workspaces: Vec<WorkspaceSummary>,
    collapsed_projects: Vec<String>,
    selected_workspace_id: Option<String>,
    revealed_project_panel: bool,
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
                revealed_project_panel: false,
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
        cx.set_global(crate::ActiveWorkspace(Some(ws_id.clone())));
        cx.notify();

        let Some(summary) = self.workspaces.iter().find(|ws| ws.id == ws_id).cloned() else {
            return;
        };
        if let Some(workspace) = self.workspace.upgrade() {
            let reveal_project_panel = !self.revealed_project_panel;
            self.revealed_project_panel = true;
            workspace.update(cx, |workspace, cx| {
                WorkspaceHost::activate_workspace(workspace, &summary, window, cx);
                Self::point_project_at_worktree(workspace, &summary, cx);
                // The project panel starts closed when the app launches
                // without a folder; reveal it on the first selection, then
                // leave the dock under user control.
                if reveal_project_panel {
                    workspace.open_panel::<project_panel::ProjectPanel>(window, cx);
                }
            });
        }
    }

    /// Switches the Zed project's visible worktree to the selected
    /// workspace's directory, so the file tree, editors, and git diffs all
    /// follow the workspace selection.
    fn point_project_at_worktree(
        workspace: &mut Workspace,
        summary: &WorkspaceSummary,
        cx: &mut Context<Workspace>,
    ) {
        let Some(path) = summary
            .workspace_root_path
            .as_deref()
            .or(summary.repository_path.as_deref())
            .or(summary.selected_worktree_path.as_deref())
            .map(PathBuf::from)
            .filter(|path| path.exists())
        else {
            return;
        };

        let project = workspace.project().clone();
        project.update(cx, |project, cx| {
            let existing: Vec<_> = project
                .visible_worktrees(cx)
                .map(|worktree| {
                    let worktree = worktree.read(cx);
                    (worktree.id(), worktree.abs_path().to_path_buf())
                })
                .collect();
            if existing.len() == 1 && existing[0].1 == path {
                return;
            }
            for (worktree_id, _) in existing {
                project.remove_worktree(worktree_id, cx);
            }
            project.create_worktree(&path, true, cx).detach_and_log_err(cx);
        });
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

        let existing_names: Vec<String> = self.workspaces.iter().map(|ws| ws.name.clone()).collect();

        cx.spawn_in(window, async move |this, cx| {
            let Some(repo) = repo else {
                log::error!("Repository not found for project: {project_name}");
                return;
            };

            let workspace_name = generate_workspace_name(&existing_names);
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

                    this.update_in(cx, |this, window, cx| {
                        this.refresh(cx);
                        this.select_workspace(detail.summary.id.clone(), window, cx);
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

const NAME_ADJECTIVES: &[&str] = &[
    "amber", "bold", "brave", "bright", "brisk", "calm", "clever", "cobalt", "coral", "crisp",
    "curious", "daring", "deft", "eager", "early", "fleet", "gentle", "gilded", "glad", "golden",
    "happy", "hardy", "hidden", "humble", "ivory", "jade", "keen", "kind", "lively", "lucky",
    "lunar", "mellow", "merry", "misty", "noble", "nimble", "opal", "patient", "plucky", "proud",
    "quiet", "rapid", "rosy", "rustic", "sable", "sage", "scarlet", "sharp", "silent", "silver",
    "sleek", "smart", "snowy", "solar", "spry", "steady", "stout", "sunny", "swift", "tidy",
    "velvet", "vivid", "wandering", "wise",
];

const NAME_ANIMALS: &[&str] = &[
    "badger", "bear", "beaver", "bison", "bobcat", "condor", "coyote", "crane", "deer", "dingo",
    "dolphin", "eagle", "egret", "elk", "falcon", "ferret", "finch", "fox", "gazelle", "gecko",
    "gibbon", "hare", "hawk", "heron", "ibex", "jackal", "jaguar", "kestrel", "kiwi", "koala",
    "lark", "lemur", "lynx", "macaw", "marmot", "marten", "mole", "moose", "narwhal", "newt",
    "ocelot", "orca", "osprey", "otter", "owl", "panda", "panther", "pelican", "petrel", "puffin",
    "puma", "quail", "rabbit", "raven", "robin", "seal", "sparrow", "stork", "swan", "tapir",
    "tern", "vole", "walrus", "wren",
];

/// Conductor-style two-word workspace names ("patient-cipher"), seeded from
/// the clock so no rand dependency is needed. Falls back to a numeric suffix
/// in the unlikely case the random picks keep colliding.
fn generate_workspace_name(existing: &[String]) -> String {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let mut next = move || {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) as usize
    };

    for _ in 0..32 {
        let name = format!(
            "{}-{}",
            NAME_ADJECTIVES[next() % NAME_ADJECTIVES.len()],
            NAME_ANIMALS[next() % NAME_ANIMALS.len()]
        );
        if !existing.iter().any(|existing| existing == &name) {
            return name;
        }
    }

    let base = format!(
        "{}-{}",
        NAME_ADJECTIVES[next() % NAME_ADJECTIVES.len()],
        NAME_ANIMALS[next() % NAME_ANIMALS.len()]
    );
    let mut counter = 2;
    loop {
        let candidate = format!("{base}-{counter}");
        if !existing.iter().any(|existing| existing == &candidate) {
            return candidate;
        }
        counter += 1;
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
