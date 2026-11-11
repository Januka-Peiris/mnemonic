use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use editor::Editor;
use gpui::{
    Action, AnyElement, App, AppContext as _, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, IntoElement, PathPromptOptions, Pixels, PromptLevel,
    Render, SharedString, WeakEntity, Window, actions, px,
};
use mnemonic_bridge::AgentAttention;
use mnemonic_core::models::{
    ActivityItem, ChatSessionRecord, CreateWorkspaceInput, DiscoveredRepository, WorkspaceSummary,
};
use mnemonic_core::repositories::{
    activity_repository, chat_session_repository, repository_repository, settings_repository,
};

#[derive(Clone)]
#[allow(dead_code)]
struct DraggedWorkspace {
    id: String,
    project: String,
}

#[derive(Clone)]
struct DraggedProject {
    name: String,
}

struct DragGhostView {
    label: String,
}

impl Render for DragGhostView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(cx.theme().colors().ghost_element_selected)
            .child(Label::new(self.label.clone()).size(LabelSize::Small))
    }
}

const PROJECT_ORDER_KEY: &str = "panel_project_order";
const WORKSPACE_ORDER_KEY: &str = "panel_workspace_order";
use mnemonic_core::services::{
    environment_service, repo_scanner_service, workspace_port_service, workspace_script_service,
    workspace_service,
};
use ui::{
    CommonAnimationExt as _, ContextMenu, Icon, IconButton, IconName, Label, Tooltip, h_flex,
    prelude::*, right_click_menu, v_flex,
};
use util::ResultExt as _;
use workspace::dock::{DockPosition, PanelEvent};
use workspace::notifications::NotificationId;
use workspace::{ModalView, Panel, Toast, Workspace};

use crate::workspace_view::WorkspaceHost;

actions!(
    mnemonic_workspace_panel,
    [
        ToggleFocus,
        /// Switch to the next workspace in the Projects panel.
        SelectNextWorkspace,
        /// Switch to the previous workspace in the Projects panel.
        SelectPreviousWorkspace
    ]
);

/// Marker for archive toasts.
struct MnemonicArchiveToast;

const WORKSPACE_PANEL_KEY: &str = "MnemonicWorkspacePanel";

pub struct WorkspacePanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    repositories: Vec<DiscoveredRepository>,
    workspaces: Vec<WorkspaceSummary>,
    collapsed_projects: Vec<String>,
    selected_workspace_id: Option<String>,
    showing_history: bool,
    activities: Vec<ActivityItem>,
    workspace_order: HashMap<String, Vec<String>>,
    project_order: Vec<String>,
}

impl WorkspacePanel {
    pub fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let panel = cx.new(|cx| {
            let mut panel = Self {
                focus_handle: cx.focus_handle(),
                workspace: workspace.weak_handle(),
                repositories: Vec::new(),
                workspaces: Vec::new(),
                collapsed_projects: Vec::new(),
                selected_workspace_id: None,
                showing_history: false,
                activities: Vec::new(),
                workspace_order: HashMap::new(),
                project_order: Vec::new(),
            };
            panel.refresh(cx);
            panel
        });

        // Periodic repaint so the attention dots track live agent activity.
        panel.update(cx, |_, cx| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            })
            .detach();
        });

        panel
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
        // Load persisted ordering.
        if self.project_order.is_empty() {
            if let Ok(Some(json)) = settings_repository::get_value(&state.db, PROJECT_ORDER_KEY) {
                if let Ok(order) = serde_json::from_str::<Vec<String>>(&json) {
                    self.project_order = order;
                }
            }
        }
        if self.workspace_order.is_empty() {
            if let Ok(Some(json)) = settings_repository::get_value(&state.db, WORKSPACE_ORDER_KEY) {
                if let Ok(order) = serde_json::from_str::<HashMap<String, Vec<String>>>(&json) {
                    self.workspace_order = order;
                }
            }
        }
    }

    fn save_ordering(&self, cx: &App) {
        let state = mnemonic_bridge::mnemonic_state(cx);
        if let Ok(json) = serde_json::to_string(&self.project_order) {
            let _ = settings_repository::set_value(&state.db, PROJECT_ORDER_KEY, &json);
        }
        if let Ok(json) = serde_json::to_string(&self.workspace_order) {
            let _ = settings_repository::set_value(&state.db, WORKSPACE_ORDER_KEY, &json);
        }
    }

    fn grouped_workspaces(&self) -> Vec<(String, Vec<&WorkspaceSummary>)> {
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
        // Apply custom workspace ordering within each group.
        for (project, workspaces) in &mut groups {
            if let Some(order) = self.workspace_order.get(project) {
                workspaces.sort_by_key(|ws| {
                    order.iter().position(|id| id == &ws.id).unwrap_or(usize::MAX)
                });
            }
        }
        // Apply custom project ordering.
        let mut result: Vec<(String, Vec<&WorkspaceSummary>)> = groups.into_iter().collect();
        if !self.project_order.is_empty() {
            result.sort_by_key(|(name, _)| {
                self.project_order
                    .iter()
                    .position(|p| p == name)
                    .unwrap_or(usize::MAX)
            });
        }
        result
    }

    /// Ensures `project_order` contains all current project names,
    /// appending any missing ones at the end.
    fn sync_project_order(&mut self) {
        let mut all_projects: Vec<String> = Vec::new();
        for repo in &self.repositories {
            if !all_projects.contains(&repo.name) {
                all_projects.push(repo.name.clone());
            }
        }
        for ws in &self.workspaces {
            let p = if ws.repo.is_empty() {
                "ungrouped".to_string()
            } else {
                ws.repo.clone()
            };
            if !all_projects.contains(&p) {
                all_projects.push(p);
            }
        }
        if self.project_order.is_empty() {
            self.project_order = all_projects;
        } else {
            for p in all_projects {
                if !self.project_order.contains(&p) {
                    self.project_order.push(p);
                }
            }
        }
    }

    fn reorder_project(&mut self, dragged: &str, target: &str, cx: &App) {
        if dragged == target {
            return;
        }
        self.sync_project_order();
        let Some(from) = self.project_order.iter().position(|p| p == dragged) else {
            return;
        };
        let Some(to) = self.project_order.iter().position(|p| p == target) else {
            return;
        };
        let name = self.project_order.remove(from);
        self.project_order.insert(to, name);
        self.save_ordering(cx);
    }

    fn reorder_workspace(&mut self, dragged_id: &str, project: &str, target_id: &str, cx: &App) {
        if dragged_id == target_id {
            return;
        }
        // Build current workspace IDs for this project group.
        let current_ids: Vec<String> = self
            .workspaces
            .iter()
            .filter(|ws| {
                let p = if ws.repo.is_empty() { "ungrouped" } else { &ws.repo };
                p == project
            })
            .map(|ws| ws.id.clone())
            .collect();
        let order = self
            .workspace_order
            .entry(project.to_string())
            .or_insert_with(|| current_ids.clone());
        // Append any new workspace IDs not yet in the order list.
        for id in &current_ids {
            if !order.contains(id) {
                order.push(id.clone());
            }
        }
        let Some(from) = order.iter().position(|id| id == dragged_id) else {
            return;
        };
        let Some(to) = order.iter().position(|id| id == target_id) else {
            return;
        };
        let id = order.remove(from);
        order.insert(to, id);
        self.save_ordering(cx);
    }

    fn toggle_project(&mut self, project: &str) {
        if let Some(pos) = self.collapsed_projects.iter().position(|p| p == project) {
            self.collapsed_projects.remove(pos);
        } else {
            self.collapsed_projects.push(project.to_string());
        }
    }

    pub(crate) fn select_workspace(
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
            workspace.update(cx, |workspace, cx| {
                WorkspaceHost::activate_workspace(workspace, &summary, window, cx);
                Self::point_project_at_worktree(workspace, &summary, cx);
            });
        }
    }

    /// Selects the workspace after or before the selected one, in the order
    /// the panel lists them, wrapping around. Collapsed projects are skipped.
    pub fn select_adjacent_workspace(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let visible_ids: Vec<String> = self
            .grouped_workspaces()
            .into_iter()
            .filter(|(project, _)| !self.collapsed_projects.contains(project))
            .flat_map(|(_, workspaces)| workspaces)
            .map(|workspace| workspace.id.clone())
            .collect();
        if visible_ids.is_empty() {
            return;
        }
        let selected = self
            .selected_workspace_id
            .as_ref()
            .and_then(|selected| visible_ids.iter().position(|id| id == selected));
        let next = match (selected, forward) {
            (Some(index), true) => (index + 1) % visible_ids.len(),
            (Some(index), false) => (index + visible_ids.len() - 1) % visible_ids.len(),
            (None, true) => 0,
            (None, false) => visible_ids.len() - 1,
        };
        if let Some(id) = visible_ids.get(next) {
            self.select_workspace(id.clone(), window, cx);
        }
    }

    /// Archive after confirmation: runs the configured teardown commands in
    /// the workspace's view, stops the agent session and run commands, frees
    /// the workspace's ports, moves the worktree into the archive folder,
    /// keeps the branch.
    pub(crate) fn confirm_archive(
        &mut self,
        ws_id: String,
        ws_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Archive {ws_name}?"),
            Some(
                "Runs any teardown commands from .mnemonic/config.json, stops the agent \
                 session and run commands, frees the workspace's ports, and moves the \
                 worktree into the archive folder. Nothing is deleted.",
            ),
            &["Archive", "Cancel"],
            cx,
        );

        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
                return;
            };
            let teardown = {
                let state = state.clone();
                let ws_id = ws_id.clone();
                cx.background_spawn(async move {
                    workspace_script_service::prepare_workspace_teardown_commands(&state, &ws_id)
                })
                .await
            };
            match teardown {
                Ok((root, plans)) if !plans.is_empty() => {
                    let teardown_task = this
                        .update_in(cx, |this, window, cx| {
                            this.select_workspace(ws_id.clone(), window, cx);
                            let workspace = this.workspace.upgrade()?;
                            workspace.update(cx, |workspace, cx| {
                                WorkspaceHost::run_teardown_for_workspace(
                                    workspace, &ws_id, root, plans, window, cx,
                                )
                            })
                        })
                        .ok()
                        .flatten();
                    match teardown_task {
                        Some(task) => task.await,
                        None => log::warn!("Archive {ws_id}: no open view to run teardown in"),
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    this.update(cx, |this, cx| {
                        this.show_archive_toast(
                            format!("{ws_name}: teardown not run: {err}; archiving anyway"),
                            cx,
                        );
                    })
                    .log_err();
                }
            }
            this.update(cx, |this, cx| {
                if let Some(workspace) = this.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        WorkspaceHost::kill_run_commands_for_workspace(workspace, &ws_id, cx);
                    });
                }
            })
            .ok();
            let result = {
                let ws_id = ws_id.clone();
                cx.background_spawn(async move {
                    // Ports are matched by process cwd, so they must be freed
                    // before the worktree moves into the archive folder.
                    for warning in free_workspace_ports(&state, &ws_id) {
                        log::warn!("Archive {ws_id}: {warning}");
                    }
                    workspace_service::archive_workspace(&state, &ws_id)?;
                    if let Err(err) =
                        chat_session_repository::delete_for_workspace(&state.db, &ws_id)
                    {
                        log::error!("Failed to delete chat sessions for {ws_id}: {err}");
                    }
                    Ok::<(), String>(())
                })
                .await
            };

            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        mnemonic_bridge::clear_agent_activity(&ws_id);
                        mnemonic_bridge::clear_pr_state(&ws_id);
                        if this.selected_workspace_id.as_deref() == Some(ws_id.as_str()) {
                            this.selected_workspace_id = None;
                            cx.set_global(crate::ActiveWorkspace(None));
                        }
                        if let Some(workspace) = this.workspace.upgrade() {
                            workspace.update(cx, |workspace, cx| {
                                WorkspaceHost::remove_workspace_view(workspace, &ws_id, cx);
                            });
                        }
                        this.refresh(cx);
                        cx.notify();
                    }
                    Err(err) => {
                        let _dismissed = window.prompt(
                            PromptLevel::Critical,
                            "Couldn't archive workspace",
                            Some(&err),
                            &["OK"],
                            cx,
                        );
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn show_archive_toast(&self, message: String, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<MnemonicArchiveToast>(), message),
                    cx,
                );
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

    /// Creates a workspace in the project. A seed names it after an issue
    /// and sends the issue to its Claude chat as an app instruction, which
    /// never renames the branch.
    fn add_worktree(
        &mut self,
        project_name: String,
        seed: Option<WorkspaceSeed>,
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

            let (workspace_name, first_message) = match seed {
                Some(seed) => (
                    unique_workspace_name(&seed.name, &existing_names),
                    Some(seed.message),
                ),
                None => (generate_workspace_name(&existing_names), None),
            };

            let result = this.read_with(cx, |_this, cx| {
                let state = mnemonic_bridge::mnemonic_state(cx);
                // Settings keys (DB): branch_prefix (default: OS username)
                // and default_base_branch (default: main).
                let branch_prefix = settings_repository::get_value(&state.db, "branch_prefix")
                    .ok()
                    .flatten()
                    .map(|value| value.trim().trim_matches('/').to_string())
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(default_branch_prefix);
                let base_branch =
                    settings_repository::get_value(&state.db, "default_base_branch")
                        .ok()
                        .flatten()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| "main".to_string());
                let branch_name = format!("{branch_prefix}/{workspace_name}");
                let input = CreateWorkspaceInput {
                    name: workspace_name,
                    repo: repo.name.clone(),
                    base_branch,
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
                    // New workspaces are chat-first: the saved chat session
                    // makes the workspace view open a Claude chat instead of
                    // the claude CLI.
                    if let Some(state) = mnemonic_bridge::mnemonic_state_cloned() {
                        let record = ChatSessionRecord {
                            id: uuid::Uuid::new_v4().to_string(),
                            workspace_id: detail.summary.id.clone(),
                            agent: "claude-acp".to_string(),
                            title: "Claude".to_string(),
                            acp_session_id: None,
                            is_review: false,
                            is_primary: true,
                            position: 0,
                        };
                        if let Err(err) = chat_session_repository::insert(&state.db, &record) {
                            log::error!("Failed to save initial chat session: {err}");
                        }
                    }

                    this.update_in(cx, |this, window, cx| {
                        this.refresh(cx);
                        this.select_workspace(detail.summary.id.clone(), window, cx);
                        let (Some(message), Some(workspace)) =
                            (first_message, this.workspace.upgrade())
                        else {
                            return;
                        };
                        let dispatched = workspace.update(cx, |workspace, cx| {
                            WorkspaceHost::dispatch_to_workspace_agent(
                                workspace,
                                &detail.summary.id,
                                message,
                                cx,
                            )
                        });
                        if !dispatched {
                            log::error!("No chat received the issue for {}", detail.summary.id);
                        }
                    })
                    .ok();
                }
                Ok(Err(e)) => {
                    log::error!("Failed to create workspace: {e}");
                    if first_message.is_some() {
                        this.update_in(cx, |_, window, cx| {
                            let _dismissed = window.prompt(
                                PromptLevel::Critical,
                                "Couldn't create workspace",
                                Some(&e),
                                &["OK"],
                                cx,
                            );
                        })
                        .log_err();
                    }
                }
                Err(e) => log::error!("Failed to access state: {e}"),
            }
        })
        .detach();
    }

    fn open_issue_prompt(
        &mut self,
        project_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let panel = cx.weak_entity();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                IssuePromptModal::new(panel, project_name, window, cx)
            });
        });
    }

    /// Fetches the issue or PR off the UI thread, then creates a workspace
    /// seeded with it.
    fn add_worktree_from_issue(
        &mut self,
        project_name: String,
        reference: IssueReference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo_path) = self
            .repositories
            .iter()
            .find(|repo| repo.name == project_name)
            .map(|repo| repo.path.clone())
        else {
            log::error!("Repository not found for project: {project_name}");
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let seed = cx
                .background_spawn(async move { reference.resolve(&repo_path) })
                .await;
            this.update_in(cx, |this, window, cx| match seed {
                Ok(seed) => this.add_worktree(project_name, Some(seed), window, cx),
                Err(err) => {
                    let _dismissed = window.prompt(
                        PromptLevel::Critical,
                        "Couldn't load the issue",
                        Some(&err),
                        &["OK"],
                        cx,
                    );
                }
            })
            .log_err();
        })
        .detach();
    }

    fn render_history_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let showing = self.showing_history;
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
            .on_click(cx.listener(|this, _, _window, cx| {
                this.showing_history = !this.showing_history;
                if this.showing_history {
                    let state = mnemonic_bridge::mnemonic_state(cx);
                    match activity_repository::list(&state.db) {
                        Ok(items) => this.activities = items,
                        Err(e) => log::error!("Failed to load history: {e}"),
                    }
                }
                cx.notify();
            }))
            .child(
                Icon::new(if showing {
                    IconName::ChevronLeft
                } else {
                    IconName::Clock
                })
                .size(IconSize::Small)
                .color(Color::Muted),
            )
            .child(
                Label::new(if showing { "Back to projects" } else { "History" })
                    .size(LabelSize::Default)
                    .color(Color::Default),
            )
    }

    fn render_history(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex()
            .id("history-list")
            .py_0p5()
            .flex_grow(1.0)
            .overflow_y_scroll();

        if self.activities.is_empty() {
            return list.child(
                h_flex().px_2().py_2().child(
                    Label::new("No activity yet")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            );
        }

        for activity in self.activities.iter().take(100) {
            let level_color = match activity.level.as_str() {
                "success" => cx.theme().status().success,
                "warning" => cx.theme().status().warning,
                "error" => cx.theme().status().error,
                _ => cx.theme().status().info,
            };
            let repo_line = match &activity.branch {
                Some(branch) if !branch.is_empty() => {
                    format!("{} · {}", activity.repo, branch)
                }
                _ => activity.repo.clone(),
            };

            list = list.child(
                v_flex()
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .w_full()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(div().w(px(6.)).h(px(6.)).rounded_full().bg(level_color))
                            .child(
                                div().flex_grow(1.0).overflow_hidden().child(
                                    Label::new(activity.event.clone())
                                        .size(LabelSize::Small)
                                        .single_line(),
                                ),
                            )
                            .child(
                                Label::new(activity.timestamp.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        Label::new(repo_line)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line(),
                    )
                    .when_some(activity.details.clone(), |el, details| {
                        el.child(
                            Label::new(details)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line(),
                        )
                    }),
            );
        }

        list
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
        let project_for_issue = project.to_string();
        let drag_data = DraggedProject {
            name: project.to_string(),
        };
        let drop_target_project = project.to_string();
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
                .on_drag(drag_data, {
                    let project_name = project.to_string();
                    move |_, _, _, cx| {
                        cx.new(|_| DragGhostView {
                            label: project_name.clone(),
                        })
                    }
                })
                .drag_over::<DraggedProject>(|style, _, _, cx| {
                    style.bg(cx.theme().colors().drop_target_background)
                })
                .on_drop(cx.listener(
                    move |this, dragged: &DraggedProject, _window, cx| {
                        this.reorder_project(&dragged.name, &drop_target_project, cx);
                        cx.notify();
                    },
                ))
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
                        SharedString::from(format!("add-worktree-from-issue-{project}")),
                        IconName::Github,
                    )
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("New from issue / PR…"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.open_issue_prompt(project_for_issue.clone(), window, cx);
                    })),
                )
                .child(
                    IconButton::new(
                        SharedString::from(format!("add-worktree-{project}")),
                        IconName::Plus,
                    )
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("New Worktree"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_worktree(project_for_add.clone(), None, window, cx);
                    })),
                ),
        );

        if !collapsed {
            for ws in workspaces {
                group = group.child(self.render_workspace_row(ws, project, cx));
            }
        }

        group
    }

    fn render_workspace_row(
        &self,
        ws: &WorkspaceSummary,
        project: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self
            .selected_workspace_id
            .as_ref()
            .is_some_and(|id| id == &ws.id);
        let ws_id = ws.id.clone();
        let ws_id_for_archive = ws.id.clone();
        let ws_name_for_archive = ws.name.clone();
        let drag_data = DraggedWorkspace {
            id: ws.id.clone(),
            project: project.to_string(),
        };
        let drop_target_id = ws.id.clone();
        let drop_target_project = project.to_string();
        let menu_id = SharedString::from(format!("workspace-menu-{}", ws.id));
        let menu_workspace_id = ws.id.clone();
        let menu_workspace_name = ws.name.clone();
        let menu_branch = ws.branch.clone();
        let menu_path = ws
            .workspace_root_path
            .clone()
            .or_else(|| ws.repository_path.clone())
            .or_else(|| ws.selected_worktree_path.clone());
        let this = cx.weak_entity();

        let row = h_flex()
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
            .on_drag(drag_data, {
                let ws_name = ws.name.clone();
                move |_, _, _, cx| {
                    cx.new(|_| DragGhostView {
                        label: ws_name.clone(),
                    })
                }
            })
            .drag_over::<DraggedWorkspace>(|style, _, _, cx| {
                style.bg(cx.theme().colors().drop_target_background)
            })
            .on_drop(cx.listener(
                move |this, dragged: &DraggedWorkspace, _window, cx| {
                    this.reorder_workspace(
                        &dragged.id,
                        &drop_target_project,
                        &drop_target_id,
                        cx,
                    );
                    cx.notify();
                },
            ))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_workspace(ws_id.clone(), window, cx);
            }))
            .child(self.render_agent_indicator(&ws.id, cx))
            .child({
                // Git icon colored by PR state: amber=open, green=ready to
                // merge, red=conflicts/failed checks, accent=merged.
                let icon_color = match mnemonic_bridge::pr_state(&ws.id).as_deref() {
                    Some("merged") => Color::Accent,
                    Some("ready") => Color::Success,
                    Some("conflicts") | Some("failed") => Color::Error,
                    Some("open") => Color::Warning,
                    _ => {
                        if is_selected {
                            Color::Default
                        } else {
                            Color::Muted
                        }
                    }
                };
                Icon::new(IconName::FileGit)
                    .size(IconSize::XSmall)
                    .color(icon_color)
            })
            .child(
                div().flex_grow(1.0).overflow_hidden().child(
                    Label::new(ws.name.clone())
                        .size(LabelSize::Default)
                        .color(Color::Default)
                        .single_line(),
                ),
            )
            .child(
                IconButton::new(
                    SharedString::from(format!("archive-{}", ws.id)),
                    IconName::Archive,
                )
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("Archive workspace"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.confirm_archive(
                        ws_id_for_archive.clone(),
                        ws_name_for_archive.clone(),
                        window,
                        cx,
                    );
                })),
            );

        right_click_menu(menu_id)
            .trigger(move |_, _, _| row)
            .menu(move |window, cx| {
                let this = this.clone();
                let workspace_id = menu_workspace_id.clone();
                let workspace_name = menu_workspace_name.clone();
                let branch = menu_branch.clone();
                let path = menu_path.clone();
                ContextMenu::build(window, cx, move |mut menu, _, _| {
                    if let Some(path) = path {
                        menu = menu
                            .entry("Reveal in Finder", None, {
                                let path = path.clone();
                                move |_, cx| cx.reveal_path(std::path::Path::new(&path))
                            })
                            .entry("Copy Path", None, move |_, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                            });
                    }
                    menu.entry("Copy Branch Name", None, move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(branch.clone()));
                    })
                    .separator()
                    .entry("Archive…", None, move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.confirm_archive(
                                workspace_id.clone(),
                                workspace_name.clone(),
                                window,
                                cx,
                            );
                        })
                        .log_err();
                    })
                })
            })
    }

    /// Error (red) and needs-attention (accent) alerts stay until the
    /// workspace is opened and win over the running spinner; otherwise a
    /// muted dot.
    fn render_agent_indicator(&self, workspace_id: &str, cx: &App) -> AnyElement {
        let running = self.workspace.upgrade().is_some_and(|workspace| {
            WorkspaceHost::workspace_agent_busy(workspace.read(cx), workspace_id, cx)
        });
        let (indicator, tooltip) = match mnemonic_bridge::agent_attention(workspace_id) {
            Some(AgentAttention::Error) => (
                div()
                    .size(px(7.))
                    .rounded_full()
                    .bg(cx.theme().status().error)
                    .into_any_element(),
                "Agent stopped with an error",
            ),
            Some(AgentAttention::NeedsAttention) => (
                div()
                    .size(px(7.))
                    .rounded_full()
                    .bg(cx.theme().colors().text_accent)
                    .into_any_element(),
                "Agent finished or needs your input",
            ),
            None if running => (
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .with_keyed_rotate_animation(
                        SharedString::from(format!("agent-running-{workspace_id}")),
                        2,
                    )
                    .into_any_element(),
                "Agent running",
            ),
            None => (
                div()
                    .size(px(7.))
                    .rounded_full()
                    .bg(cx.theme().colors().text_muted)
                    .into_any_element(),
                "Agent idle",
            ),
        };
        h_flex()
            .id(SharedString::from(format!(
                "agent-indicator-{workspace_id}"
            )))
            .size(px(12.))
            .flex_none()
            .justify_center()
            .tooltip(Tooltip::text(tooltip))
            .child(indicator)
            .into_any_element()
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

/// Branch prefix fallback: the OS username, sanitized for git ref syntax.
fn default_branch_prefix() -> String {
    std::env::var("USER")
        .ok()
        .map(|user| {
            user.trim()
                .to_lowercase()
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                        ch
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
        })
        .filter(|user| !user.is_empty())
        .unwrap_or_else(|| "mnemonic".to_string())
}

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

/// Whether a name is one `generate_workspace_name` produces, i.e. the
/// workspace still has its placeholder name.
pub(crate) fn is_generated_workspace_name(name: &str) -> bool {
    let mut parts = name.split('-');
    let (Some(adjective), Some(animal)) = (parts.next(), parts.next()) else {
        return false;
    };
    let suffix_is_valid = match parts.next() {
        None => true,
        Some(counter) => {
            parts.next().is_none()
                && !counter.is_empty()
                && counter.chars().all(|character| character.is_ascii_digit())
        }
    };
    suffix_is_valid && NAME_ADJECTIVES.contains(&adjective) && NAME_ANIMALS.contains(&animal)
}

const SLUG_STOP_WORDS: &[&str] = &[
    "a", "also", "an", "and", "are", "as", "at", "be", "but", "by", "can", "could", "do", "does", "for",
    "from", "hello", "hey", "hi", "i", "im", "in", "into", "is", "it", "its", "just", "let",
    "lets", "me", "my", "need", "of", "ok", "okay", "on", "or", "our", "please", "pls", "should",
    "so", "some", "that", "the", "then", "these", "this", "those", "to", "us", "want", "we",
    "will", "with", "would", "you", "your",
];
const SLUG_MAX_WORDS: usize = 5;
const SLUG_MAX_CHARS: usize = 40;

/// A branch-name slug for a workspace's first message, e.g. "Can you fix the
/// model dropdown filter?" becomes "fix-model-dropdown-filter". None when
/// nothing meaningful is left, or for slash commands like "/model opus".
pub(crate) fn branch_slug_from_message(text: &str) -> Option<String> {
    let text = text.trim();
    if text.starts_with('/') {
        return None;
    }
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|character| !matches!(character, '\'' | '\u{2019}'))
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect();

    let mut slug = String::new();
    let meaningful_words = normalized
        .split_whitespace()
        .filter(|word| !SLUG_STOP_WORDS.contains(word))
        .take(SLUG_MAX_WORDS);
    for word in meaningful_words {
        if slug.is_empty() {
            slug.extend(word.chars().take(SLUG_MAX_CHARS));
        } else if slug.len() + 1 + word.len() <= SLUG_MAX_CHARS {
            slug.push('-');
            slug.push_str(word);
        } else {
            break;
        }
    }

    let slug = slug.trim_matches('-').to_string();
    Some(slug).filter(|slug| !slug.is_empty())
}

/// A workspace started from an issue: its name (also the branch's last
/// segment) and the instruction sent to its Claude chat.
#[derive(Debug, PartialEq, Eq)]
struct WorkspaceSeed {
    name: String,
    message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitHubItemKind {
    Issue,
    PullRequest,
}

impl GitHubItemKind {
    fn gh_command(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::PullRequest => "pr",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::PullRequest => "pull request",
        }
    }
}

/// What the user typed into "New from issue / PR".
#[derive(Debug, PartialEq, Eq)]
enum IssueReference {
    /// A GitHub issue or PR URL, passed to `gh` as is.
    GitHub { kind: GitHubItemKind, url: String },
    /// `#123` or `123`: an issue, or failing that a PR, in the project's
    /// repository.
    GitHubNumber(u64),
    /// A Linear issue URL. Nothing is fetched: the title comes from the
    /// URL's slug.
    Linear {
        url: String,
        identifier: String,
        title: String,
    },
}

impl IssueReference {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if let Ok(number) = text.trim_start_matches('#').parse::<u64>() {
            return Some(Self::GitHubNumber(number));
        }
        let url = text.split(['?', '#']).next()?.trim_end_matches('/');
        let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
        let mut segments = without_scheme.split('/');
        let host = segments.next()?.trim_start_matches("www.");
        let segments: Vec<&str> = segments.collect();
        match (host, segments.as_slice()) {
            ("github.com", [_owner, _repo, kind, number, ..]) if number.parse::<u64>().is_ok() => {
                let kind = match *kind {
                    "issues" => GitHubItemKind::Issue,
                    "pull" => GitHubItemKind::PullRequest,
                    _ => return None,
                };
                Some(Self::GitHub {
                    kind,
                    url: url.to_string(),
                })
            }
            ("linear.app", [_team, "issue", identifier, rest @ ..]) if !identifier.is_empty() => {
                let title = rest
                    .first()
                    .map(|slug| slug.replace('-', " "))
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| identifier.to_string());
                Some(Self::Linear {
                    url: url.to_string(),
                    identifier: identifier.to_string(),
                    title,
                })
            }
            _ => None,
        }
    }

    /// Fetches a GitHub item's title and body with `gh`; blocking.
    fn resolve(self, repo_path: &str) -> Result<WorkspaceSeed, String> {
        match self {
            Self::GitHub { kind, url } => fetch_github_item(kind, &url, repo_path),
            Self::GitHubNumber(number) => {
                let target = number.to_string();
                fetch_github_item(GitHubItemKind::Issue, &target, repo_path).or_else(|issue_err| {
                    fetch_github_item(GitHubItemKind::PullRequest, &target, repo_path)
                        .map_err(|_| issue_err)
                })
            }
            Self::Linear {
                url,
                identifier,
                title,
            } => Ok(issue_seed(
                &identifier.to_lowercase(),
                &title,
                "Linear issue",
                &url,
                "",
            )),
        }
    }
}

fn fetch_github_item(
    kind: GitHubItemKind,
    target: &str,
    repo_path: &str,
) -> Result<WorkspaceSeed, String> {
    let gh = environment_service::find_gh_path().unwrap_or_else(|| "gh".to_string());
    let output = std::process::Command::new(&gh)
        .args([
            kind.gh_command(),
            "view",
            target,
            "--json",
            "title,body,number,url",
        ])
        .current_dir(repo_path)
        .output()
        .map_err(|err| format!("Couldn't run gh: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "gh {} view {target} failed: {}",
            kind.gh_command(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let item: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("Couldn't read gh output: {err}"))?;
    let field = |name: &str| {
        item.get(name)
            .and_then(|value| value.as_str())
            .unwrap_or("")
    };
    let number = item
        .get("number")
        .and_then(|value| value.as_u64())
        .ok_or_else(|| "gh returned no issue number".to_string())?;
    Ok(issue_seed(
        &number.to_string(),
        field("title"),
        &format!("GitHub {}", kind.label()),
        field("url"),
        field("body"),
    ))
}

/// Names the workspace `<id>-<slug of title>` and asks the agent to
/// implement the item.
fn issue_seed(id: &str, title: &str, kind: &str, url: &str, body: &str) -> WorkspaceSeed {
    let name = match branch_slug_from_message(title) {
        Some(slug) => format!("{id}-{slug}"),
        None => id.to_string(),
    };
    let mut message = format!("Please implement this {kind}: {title}\n{url}");
    if !body.trim().is_empty() {
        message.push_str("\n\n");
        message.push_str(body.trim());
    }
    WorkspaceSeed { name, message }
}

/// `name`, or `name-2`, `name-3`... when a workspace already uses it.
fn unique_workspace_name(name: &str, existing: &[String]) -> String {
    let taken = |candidate: &str| existing.iter().any(|existing| existing == candidate);
    if !taken(name) {
        return name.to_string();
    }
    (2..)
        .map(|counter| format!("{name}-{counter}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| name.to_string())
}

/// One-line prompt for "New from issue / PR".
struct IssuePromptModal {
    editor: Entity<Editor>,
    panel: WeakEntity<WorkspacePanel>,
    project_name: String,
    error: Option<SharedString>,
}

impl IssuePromptModal {
    fn new(
        panel: WeakEntity<WorkspacePanel>,
        project_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(
                "https://github.com/owner/repo/issues/123 or #123",
                window,
                cx,
            );
            editor
        });
        Self {
            editor,
            panel,
            project_name,
            error: None,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx);
        if text.trim().is_empty() {
            cx.emit(DismissEvent);
            return;
        }
        let Some(reference) = IssueReference::parse(&text) else {
            self.error = Some("Not a GitHub issue or PR URL, #number, or Linear issue URL".into());
            cx.notify();
            return;
        };
        let project_name = self.project_name.clone();
        self.panel
            .update(cx, |panel, cx| {
                panel.add_worktree_from_issue(project_name, reference, window, cx);
            })
            .log_err();
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for IssuePromptModal {}

impl ModalView for IssuePromptModal {}

impl Focusable for IssuePromptModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for IssuePromptModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = match &self.error {
            Some(error) => Label::new(error.clone())
                .size(LabelSize::Small)
                .color(Color::Error),
            None => Label::new(format!(
                "New workspace in {} from a GitHub issue or PR, or a Linear issue URL.",
                self.project_name
            ))
            .size(LabelSize::Small)
            .color(Color::Muted),
        };
        v_flex()
            .key_context("IssuePromptModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w_96()
            .overflow_hidden()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .bg(cx.theme().colors().editor_background)
                    .rounded_b_sm()
                    .w_full()
                    .p_2()
                    .child(hint),
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

        let mut content = v_flex()
            .id("workspace-panel")
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .child(self.render_history_bar(cx));

        if self.showing_history {
            return content.child(self.render_history(cx));
        }

        content = content.child(self.render_header(cx));

        let groups = self.grouped_workspaces();
        let has_workspaces = !groups.is_empty();

        if has_workspaces {
            let mut list = v_flex().py_0p5().overflow_y_hidden().flex_grow(1.0);
            for (project, workspaces) in &groups {
                list = list.child(self.render_project_group(project, &workspaces, cx));
            }
            content = content.child(list);
        } else {
            content = content.child(self.render_empty());
        }

        content
    }
}

/// Kills every process listening on a port from inside the workspace's
/// worktree, returning what could not be killed.
fn free_workspace_ports(state: &mnemonic_core::state::AppState, workspace_id: &str) -> Vec<String> {
    workspace_port_service::invalidate_workspace_port_cache(workspace_id);
    let ports = match workspace_port_service::list_workspace_ports(state, workspace_id) {
        Ok(ports) => ports,
        Err(err) => return vec![format!("Couldn't list workspace ports: {err}")],
    };
    ports
        .into_iter()
        .filter_map(|port| {
            workspace_port_service::kill_workspace_port_process(
                state,
                workspace_id,
                port.port,
                port.pid,
            )
            .err()
            .map(|err| format!("Couldn't free port {}: {err}", port.port))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_message_becomes_descriptive_branch_slug() {
        assert_eq!(
            branch_slug_from_message("Hey, can you please fix the model dropdown filter?"),
            Some("fix-model-dropdown-filter".to_string())
        );
        assert_eq!(
            branch_slug_from_message(
                "Let's add retry logic to the uploader and also log every failure loudly"
            ),
            Some("add-retry-logic-uploader-log".to_string())
        );
        assert_eq!(
            branch_slug_from_message("Refactor internationalization configuration loading"),
            Some("refactor-internationalization".to_string())
        );
        assert_eq!(branch_slug_from_message("ok, just do it"), None);
        assert_eq!(branch_slug_from_message("/model claude-opus-5-5"), None);
    }

    #[test]
    fn issue_input_is_recognized() {
        assert_eq!(
            IssueReference::parse("https://github.com/acme/app/issues/42#issuecomment-1"),
            Some(IssueReference::GitHub {
                kind: GitHubItemKind::Issue,
                url: "https://github.com/acme/app/issues/42".to_string(),
            })
        );
        assert_eq!(
            IssueReference::parse("github.com/acme/app/pull/7/files"),
            Some(IssueReference::GitHub {
                kind: GitHubItemKind::PullRequest,
                url: "github.com/acme/app/pull/7/files".to_string(),
            })
        );
        assert_eq!(
            IssueReference::parse(" #123 "),
            Some(IssueReference::GitHubNumber(123))
        );
        assert_eq!(
            IssueReference::parse("https://linear.app/acme/issue/ENG-12/fix-login-redirect"),
            Some(IssueReference::Linear {
                url: "https://linear.app/acme/issue/ENG-12/fix-login-redirect".to_string(),
                identifier: "ENG-12".to_string(),
                title: "fix login redirect".to_string(),
            })
        );
        assert_eq!(IssueReference::parse("https://github.com/acme/app"), None);
        assert_eq!(IssueReference::parse("fix the login page"), None);
    }

    #[test]
    fn issue_workspace_is_named_after_number_and_title() {
        let seed = issue_seed(
            "42",
            "Login redirect loops on Safari",
            "GitHub issue",
            "https://github.com/acme/app/issues/42",
            "Steps to reproduce",
        );
        assert_eq!(seed.name, "42-login-redirect-loops-safari");
        assert!(
            seed.message
                .contains("https://github.com/acme/app/issues/42")
        );
        assert!(seed.message.ends_with("Steps to reproduce"));
        assert!(!is_generated_workspace_name(&seed.name));
        assert_eq!(
            unique_workspace_name("42-login", &["42-login".to_string()]),
            "42-login-2"
        );
    }

    #[test]
    fn only_generator_names_count_as_auto_named() {
        assert!(is_generated_workspace_name("crisp-panda"));
        assert!(is_generated_workspace_name("crisp-panda-3"));
        assert!(!is_generated_workspace_name("fix-model-dropdown-filter"));
        assert!(!is_generated_workspace_name("crisp-panda-fix"));
        assert!(!is_generated_workspace_name("crisp"));
    }
}

