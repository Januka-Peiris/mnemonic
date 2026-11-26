use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use editor::Editor;
use gpui::{
    Action, AnyElement, AnyWindowHandle, App, AppContext as _, AsyncApp, ClipboardItem, Context,
    DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, PathPromptOptions,
    Pixels, PromptLevel, Render, SharedString, Subscription, Task, WeakEntity, Window, actions, px,
};
use mnemonic_bridge::AgentAttention;
use mnemonic_core::models::{
    ActivityItem, ArchivedWorkspace, ChatSessionRecord, DiscoveredRepository, WorkspaceDetail,
    WorkspaceSummary,
};
use mnemonic_core::repositories::{
    activity_repository, chat_session_repository, repository_repository,
    repository_settings_repository, settings_repository, workspace_repository,
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
const HIDDEN_PROJECTS_KEY: &str = "panel_hidden_projects";
const PR_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(120);
const CHANGED_FILES_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
const PROJECT_BASE_BRANCH_KEY: &str = "base_branch";
const PROJECT_SETUP_SCRIPT_KEY: &str = "setup_script";
const PROJECT_RUN_SCRIPT_KEY: &str = workspace_script_service::PROJECT_RUN_SCRIPT_KEY;
const PROJECT_INIT_FILES_KEY: &str = "workspace_init_files";
const WORKSPACE_ORDER_KEY: &str = "panel_workspace_order";
const PINNED_WORKSPACES_KEY: &str = "panel_pinned_workspaces";
const WORKSPACE_LABELS_KEY: &str = "panel_workspace_labels";
use mnemonic_core::services::team_files_service::TEAM_COPY_SOURCE;
use mnemonic_core::services::{
    environment_service, pr_draft_service, repo_scanner_service, workspace_port_service, workspace_script_service,
    workspace_service,
};
use settings::Settings as _;
use ui::{
    Button, ButtonStyle, ContextMenu, ContextMenuEntry, Icon, IconButton, IconName, Label,
    PopoverMenu, Tooltip, h_flex, prelude::*, right_click_menu, v_flex,
};
use util::ResultExt as _;
use workspace::dock::{DockPosition, PanelEvent};
use workspace::notifications::NotificationId;
use workspace::{ModalView, Panel, Toast, Workspace};

use crate::workspace_view::{ChatAgent, WorkspaceHost};
use crate::workspaces_settings::WorkspacesSettings;

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

/// Marker for workspace link toasts.
struct MnemonicLinkToast;

const WORKSPACE_PANEL_KEY: &str = "MnemonicWorkspacePanel";

pub struct WorkspacePanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    repositories: Vec<DiscoveredRepository>,
    workspaces: Vec<WorkspaceSummary>,
    archived: Vec<ArchivedWorkspace>,
    /// Pairs of workspaces linked together, so each one's agents can use the
    /// other's files.
    workspace_links: Vec<(String, String)>,
    /// Projects whose archived workspaces are listed.
    expanded_archives: Vec<String>,
    collapsed_projects: Vec<String>,
    /// Projects left out of the list until shown again from "Hidden (n)".
    hidden_projects: Option<Vec<String>>,
    showing_hidden_projects: bool,
    changed_files_refresh: Option<Task<()>>,
    selected_workspace_id: Option<String>,
    showing_history: bool,
    activities: Vec<ActivityItem>,
    workspace_order: HashMap<String, Vec<String>>,
    project_order: Vec<String>,
    /// Pinned workspace ids, in the order the Pinned section lists them.
    pinned_workspaces: Option<Vec<String>>,
    workspace_labels: Option<HashMap<String, String>>,
}

impl WorkspacePanel {
    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let panel = cx.new(|cx| {
            let mut panel = Self {
                focus_handle: cx.focus_handle(),
                workspace: workspace.weak_handle(),
                repositories: Vec::new(),
                workspaces: Vec::new(),
                archived: Vec::new(),
                workspace_links: Vec::new(),
                expanded_archives: Vec::new(),
                collapsed_projects: Vec::new(),
                hidden_projects: None,
                showing_hidden_projects: false,
                changed_files_refresh: None,
                selected_workspace_id: None,
                showing_history: false,
                activities: Vec::new(),
                workspace_order: HashMap::new(),
                project_order: Vec::new(),
                pinned_workspaces: None,
                workspace_labels: None,
            };
            panel.refresh(cx);
            panel
        });

        crate::workspace_view::register_window(workspace.weak_handle(), window.window_handle(), cx);
        if let Some(team) = crate::team::team_share(cx) {
            panel.update(cx, |_, cx| {
                cx.observe(&team, |_, _, cx| cx.notify()).detach();
            });
        }

        // Keeps every workspace's PR state current, not only the selected
        // one's, so a merge on GitHub shows in the list without opening it.
        panel.update(cx, |_, cx| {
            cx.spawn(async move |this, cx| {
                loop {
                    let Ok(workspace_ids) = this.read_with(cx, |this, _| {
                        this.workspaces
                            .iter()
                            .map(|workspace| workspace.id.clone())
                            .collect::<Vec<_>>()
                    }) else {
                        break;
                    };
                    let changed = match mnemonic_bridge::mnemonic_state_cloned() {
                        Some(state) => {
                            cx.background_spawn(async move {
                                let mut changed = false;
                                for workspace_id in workspace_ids {
                                    match pr_draft_service::get_workspace_pr_status(
                                        &state,
                                        &workspace_id,
                                    ) {
                                        Ok(status) => {
                                            changed |= mnemonic_bridge::record_pr_status(
                                                &workspace_id,
                                                &status,
                                            );
                                        }
                                        Err(err) => log::debug!(
                                            "PR status for {workspace_id} unavailable: {err}"
                                        ),
                                    }
                                }
                                changed
                            })
                            .await
                        }
                        None => false,
                    };
                    if changed {
                        cx.update(|cx| cx.refresh_windows());
                    }
                    cx.background_executor()
                        .timer(PR_STATUS_POLL_INTERVAL)
                        .await;
                }
            })
            .detach();
        });

        // Reloads the list from the database and repaints, so the attention
        // dots track live agent activity. Rendering itself never reads the
        // database.
        panel.update(cx, |_, cx| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    let updated = this.update(cx, |this, cx| {
                        this.refresh(cx);
                        cx.notify();
                    });
                    if updated.is_err() {
                        break;
                    }
                }
            })
            .detach();
        });

        // Only the selected workspace is re-diffed; the others keep their
        // last counts until they are opened again.
        panel.update(cx, |_, cx| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(CHANGED_FILES_REFRESH_INTERVAL)
                        .await;
                    if this
                        .update(cx, |this, cx| this.refresh_selected_changed_files(cx))
                        .is_err()
                    {
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

    pub(crate) fn refresh(&mut self, cx: &App) {
        let state = mnemonic_bridge::mnemonic_state(cx);
        match workspace_service::list_workspaces(&state) {
            Ok(ws) => self.workspaces = ws,
            Err(e) => log::error!("Failed to list workspaces: {e}"),
        }
        match workspace_service::list_archived_workspaces(&state) {
            Ok(archived) => self.archived = archived,
            Err(e) => log::error!("Failed to list archived workspaces: {e}"),
        }
        match repository_repository::list(&state.db) {
            Ok(repos) => self.repositories = repos,
            Err(e) => log::error!("Failed to list repositories: {e}"),
        }
        match workspace_service::list_workspace_links(&state) {
            Ok(links) => {
                mnemonic_bridge::set_linked_workspaces(
                    links
                        .iter()
                        .flat_map(|(first, second)| [first.clone(), second.clone()])
                        .collect(),
                );
                self.workspace_links = links;
            }
            Err(e) => log::error!("Failed to list workspace links: {e}"),
        }
        // Load persisted ordering.
        if self.project_order.is_empty() {
            if let Ok(Some(json)) = settings_repository::get_value(&state.db, PROJECT_ORDER_KEY) {
                if let Ok(order) = serde_json::from_str::<Vec<String>>(&json) {
                    self.project_order = order;
                }
            }
        }
        if self.hidden_projects.is_none() {
            let hidden = settings_repository::get_value(&state.db, HIDDEN_PROJECTS_KEY)
                .log_err()
                .flatten()
                .and_then(|json| serde_json::from_str::<Vec<String>>(&json).log_err());
            self.hidden_projects = Some(hidden.unwrap_or_default());
        }
        if self.workspace_order.is_empty() {
            if let Ok(Some(json)) = settings_repository::get_value(&state.db, WORKSPACE_ORDER_KEY) {
                if let Ok(order) = serde_json::from_str::<HashMap<String, Vec<String>>>(&json) {
                    self.workspace_order = order;
                }
            }
        }
        if self.pinned_workspaces.is_none() {
            let pinned = settings_repository::get_value(&state.db, PINNED_WORKSPACES_KEY)
                .log_err()
                .flatten()
                .and_then(|json| serde_json::from_str::<Vec<String>>(&json).log_err());
            self.pinned_workspaces = Some(pinned.unwrap_or_default());
        }
        if self.workspace_labels.is_none() {
            let labels = settings_repository::get_value(&state.db, WORKSPACE_LABELS_KEY)
                .log_err()
                .flatten()
                .and_then(|json| serde_json::from_str::<HashMap<String, String>>(&json).log_err());
            self.workspace_labels = Some(labels.unwrap_or_default());
        }
    }

    fn is_pinned(&self, workspace_id: &str) -> bool {
        self.pinned_workspaces
            .as_ref()
            .is_some_and(|pinned| pinned.iter().any(|id| id == workspace_id))
    }

    /// The open workspaces that are pinned, in pinned order.
    fn pinned_workspace_summaries(&self) -> Vec<&WorkspaceSummary> {
        self.pinned_workspaces
            .iter()
            .flatten()
            .filter_map(|id| self.workspaces.iter().find(|ws| &ws.id == id))
            .collect()
    }

    fn workspace_label(&self, workspace_id: &str) -> Option<&str> {
        self.workspace_labels
            .as_ref()
            .and_then(|labels| labels.get(workspace_id))
            .map(String::as_str)
    }

    fn save_pinned_workspaces(&self, cx: &App) {
        let state = mnemonic_bridge::mnemonic_state(cx);
        match serde_json::to_string(self.pinned_workspaces.as_ref().unwrap_or(&Vec::new())) {
            Ok(json) => {
                settings_repository::set_value(&state.db, PINNED_WORKSPACES_KEY, &json).log_err();
            }
            Err(err) => log::error!("Failed to save pinned workspaces: {err}"),
        }
    }

    fn set_workspace_pinned(&mut self, workspace_id: &str, pinned: bool, cx: &mut Context<Self>) {
        let existing_ids: Vec<&str> = self.workspaces.iter().map(|ws| ws.id.as_str()).collect();
        let list = self.pinned_workspaces.get_or_insert_with(Vec::new);
        // Archived and deleted workspaces drop out of the pin list here.
        list.retain(|id| id != workspace_id && existing_ids.contains(&id.as_str()));
        if pinned {
            list.push(workspace_id.to_string());
        }
        self.save_pinned_workspaces(cx);
        cx.notify();
    }

    fn reorder_pinned(&mut self, dragged_id: &str, target_id: &str, cx: &App) {
        let mut order: Vec<String> = self
            .pinned_workspace_summaries()
            .iter()
            .map(|ws| ws.id.clone())
            .collect();
        if !move_onto(&mut order, dragged_id, target_id) {
            return;
        }
        self.pinned_workspaces = Some(order);
        self.save_pinned_workspaces(cx);
    }

    fn set_workspace_label(&mut self, workspace_id: &str, label: &str, cx: &mut Context<Self>) {
        let labels = self.workspace_labels.get_or_insert_with(HashMap::new);
        let label = label.trim();
        if label.is_empty() {
            labels.remove(workspace_id);
        } else {
            labels.insert(workspace_id.to_string(), label.to_string());
        }
        let state = mnemonic_bridge::mnemonic_state(cx);
        match serde_json::to_string(labels) {
            Ok(json) => {
                settings_repository::set_value(&state.db, WORKSPACE_LABELS_KEY, &json).log_err();
            }
            Err(err) => log::error!("Failed to save workspace labels: {err}"),
        }
        cx.notify();
    }

    /// Moves a workspace one place up or down within the section it is
    /// listed in: the Pinned section, or its project.
    fn move_workspace(&mut self, workspace_id: &str, up: bool, cx: &mut Context<Self>) {
        let order: Vec<String> = if self.is_pinned(workspace_id) {
            self.pinned_workspace_summaries()
                .iter()
                .map(|ws| ws.id.clone())
                .collect()
        } else {
            let Some(project) = self
                .workspaces
                .iter()
                .find(|ws| ws.id == workspace_id)
                .map(workspace_project)
            else {
                return;
            };
            self.grouped_workspaces()
                .into_iter()
                .find(|(name, _)| *name == project)
                .map(|(_, workspaces)| workspaces.iter().map(|ws| ws.id.clone()).collect())
                .unwrap_or_default()
        };
        let Some(index) = order.iter().position(|id| id == workspace_id) else {
            return;
        };
        let neighbor = if up {
            index.checked_sub(1)
        } else {
            Some(index + 1).filter(|next| *next < order.len())
        };
        let Some(neighbor_id) = neighbor.and_then(|neighbor| order.get(neighbor)).cloned() else {
            return;
        };
        if self.is_pinned(workspace_id) {
            self.reorder_pinned(workspace_id, &neighbor_id, cx);
        } else if let Some(project) = self
            .workspaces
            .iter()
            .find(|ws| ws.id == workspace_id)
            .map(workspace_project)
        {
            self.reorder_workspace(workspace_id, &project, &neighbor_id, cx);
        }
        cx.notify();
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
            if self.is_pinned(&ws.id) {
                continue;
            }
            groups.entry(workspace_project(ws)).or_default().push(ws);
        }
        for (project, workspaces) in &mut groups {
            if let Some(order) = self.workspace_order.get(project) {
                workspaces.sort_by_key(|ws| saved_order_rank(order, &ws.id));
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
        let Some(mut order) = self
            .grouped_workspaces()
            .into_iter()
            .find(|(name, _)| name == project)
            .map(|(_, workspaces)| {
                workspaces
                    .iter()
                    .map(|ws| ws.id.clone())
                    .collect::<Vec<_>>()
            })
        else {
            return;
        };
        if !move_onto(&mut order, dragged_id, target_id) {
            return;
        }
        self.workspace_order.insert(project.to_string(), order);
        self.save_ordering(cx);
    }

    fn is_project_hidden(&self, project: &str) -> bool {
        self.hidden_projects
            .as_ref()
            .is_some_and(|hidden| hidden.iter().any(|name| name == project))
    }

    fn set_project_hidden(&mut self, project: &str, hidden: bool, cx: &mut Context<Self>) {
        let list = self.hidden_projects.get_or_insert_with(Vec::new);
        list.retain(|name| name != project);
        if hidden {
            list.push(project.to_string());
        }
        if list.is_empty() {
            self.showing_hidden_projects = false;
        }
        let state = mnemonic_bridge::mnemonic_state(cx);
        match serde_json::to_string(list) {
            Ok(json) => {
                settings_repository::set_value(&state.db, HIDDEN_PROJECTS_KEY, &json).log_err();
            }
            Err(err) => log::error!("Failed to save hidden projects: {err}"),
        }
        cx.notify();
    }

    /// Takes the project out of the list. Only its registration is removed:
    /// nothing on disk changes, and adding the folder again brings it back.
    fn remove_project(&mut self, project: String, cx: &mut Context<Self>) {
        let Some(repository_id) = self
            .repositories
            .iter()
            .find(|repository| repository.name == project)
            .map(|repository| repository.id.clone())
        else {
            return;
        };
        let state = mnemonic_bridge::mnemonic_state(cx);
        if let Err(err) = repo_scanner_service::remove_repository(&state, &repository_id) {
            self.show_archive_toast(format!("Couldn't remove {project}: {err}"), cx);
            return;
        }
        self.project_order.retain(|name| name != &project);
        self.collapsed_projects.retain(|name| name != &project);
        self.save_ordering(cx);
        if self.is_project_hidden(&project) {
            self.set_project_hidden(&project, false, cx);
        }
        self.refresh(cx);
        cx.notify();
    }

    /// Re-diffs the selected workspace in the background unless a refresh
    /// is already running.
    fn refresh_selected_changed_files(&mut self, cx: &mut Context<Self>) {
        if self.changed_files_refresh.is_some() {
            return;
        }
        let (Some(workspace_id), Some(state)) = (
            self.selected_workspace_id.clone(),
            mnemonic_bridge::mnemonic_state_cloned(),
        ) else {
            return;
        };
        self.changed_files_refresh = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let workspace_id = workspace_id.clone();
                    async move {
                        workspace_service::refresh_workspace_changed_files(&state, &workspace_id)
                    }
                })
                .await;
            if let Err(err) = result {
                log::debug!("Changed files for {workspace_id} not refreshed: {err}");
            }
            this.update(cx, |this, cx| {
                this.changed_files_refresh = None;
                this.refresh(cx);
                cx.notify();
            })
            .log_err();
        }));
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
        if WorkspaceHost::show_in_window_already_hosting(&self.workspace, &ws_id, cx) {
            return;
        }
        self.selected_workspace_id = Some(ws_id.clone());
        crate::ActiveWorkspace::set(self.workspace.entity_id(), Some(ws_id.clone()), cx);
        self.refresh_selected_changed_files(cx);
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

    /// Opens a teammate's shared workspace in this window, in place of the
    /// selected local workspace.
    pub(crate) fn select_teammate(
        &mut self,
        host: String,
        workspace_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_workspace_id = None;
        crate::ActiveWorkspace::set(
            self.workspace.entity_id(),
            Some(crate::team_workspace_view::teammate_workspace_key(
                &host,
                &workspace_id,
            )),
            cx,
        );
        cx.notify();
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                WorkspaceHost::activate_teammate(workspace, host, workspace_id, window, cx);
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
            .pinned_workspace_summaries()
            .into_iter()
            .chain(
                self.grouped_workspaces()
                    .into_iter()
                    .filter(|(project, _)| {
                        !self.collapsed_projects.contains(project)
                            && !self.is_project_hidden(project)
                    })
                    .flat_map(|(_, workspaces)| workspaces),
            )
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

    /// Brings an archived workspace back: its worktree moves out of the
    /// archive folder and it reopens with the chats it had.
    fn restore_workspace(
        &mut self,
        workspace_id: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    workspace_service::restore_archived_workspace(&state, &workspace_id)
                })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(summary) => {
                    this.refresh(cx);
                    this.select_workspace(summary.id, window, cx);
                }
                Err(err) => {
                    this.show_archive_toast(format!("{name}: couldn't restore: {err}"), cx);
                }
            })
            .log_err();
        })
        .detach();
    }

    fn open_text_prompt(
        &mut self,
        purpose: WorkspaceTextPrompt,
        workspace_id: String,
        current_text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let panel = cx.weak_entity();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                WorkspaceTextModal::new(panel, purpose, workspace_id, current_text, window, cx)
            });
        });
    }

    /// Renames the workspace off the UI thread, then updates its open view.
    fn rename_workspace(
        &mut self,
        workspace_id: String,
        new_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    workspace_service::rename_workspace(&state, &workspace_id, &new_name)
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(summary) => {
                    this.refresh(cx);
                    if let Some(workspace) = this.workspace.upgrade() {
                        workspace.update(cx, |workspace, cx| {
                            WorkspaceHost::workspace_renamed(
                                workspace,
                                &summary.id,
                                &summary.name,
                                cx,
                            );
                        });
                    }
                    cx.notify();
                }
                Err(err) => this.show_archive_toast(format!("Couldn't rename: {err}"), cx),
            })
            .log_err();
        })
        .detach();
    }

    /// Drops a workspace that no longer exists from the sidebar and this
    /// window.
    pub(crate) fn forget_workspace(&mut self, ws_id: &str, cx: &mut Context<Self>) {
        mnemonic_bridge::clear_agent_activity(ws_id);
        mnemonic_bridge::clear_pr_state(ws_id);
        if self.selected_workspace_id.as_deref() == Some(ws_id) {
            self.selected_workspace_id = None;
            crate::ActiveWorkspace::set(self.workspace.entity_id(), None, cx);
        }
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                WorkspaceHost::remove_workspace_view(workspace, ws_id, cx);
            });
        }
        self.refresh(cx);
        cx.notify();
    }

    /// Runs the configured teardown commands in the workspace's view, stops
    /// the agent session and run commands, frees the workspace's ports, moves
    /// the worktree into the archive folder, keeps the branch. There is no
    /// confirmation because an archived workspace can be restored.
    pub(crate) fn archive_workspace(
        &mut self,
        ws_id: String,
        ws_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A teammate's copy holds the sharer's teardown commands, which this
        // machine never runs; it is deleted instead, after warning about
        // unsent edits.
        let listed_as_team_copy = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == ws_id)
            .map(|workspace| workspace.workspace_source == TEAM_COPY_SOURCE);
        if listed_as_team_copy == Some(true) {
            crate::team_copy::delete_copy(ws_id, window, cx);
            return;
        }
        let panel_window = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
                return;
            };
            if listed_as_team_copy.is_none() {
                // Not in the sidebar's list yet, so its record decides
                // whether it is a teammate's copy.
                let lookup = {
                    let state = state.clone();
                    let ws_id = ws_id.clone();
                    cx.background_spawn(async move {
                        workspace_repository::get_detail(&state.db, &ws_id)
                    })
                    .await
                };
                match lookup {
                    Ok(Some(detail)) if detail.summary.workspace_source == TEAM_COPY_SOURCE => {
                        this.update_in(cx, |_, window, cx| {
                            crate::team_copy::delete_copy(ws_id, window, cx)
                        })
                        .log_err();
                        return;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        this.update(cx, |this, cx| {
                            this.show_archive_toast(format!("{ws_name}: not archived: {err}"), cx)
                        })
                        .log_err();
                        return;
                    }
                }
            }
            let window_handle = cx.window_handle();
            let result = archive_own_workspace(
                panel_window,
                window_handle,
                ws_id.clone(),
                state,
                {
                    let this = this.clone();
                    let ws_id = ws_id.clone();
                    move |window, cx| {
                        this.update(cx, |this, cx| this.select_workspace(ws_id, window, cx))
                            .log_err();
                    }
                },
                {
                    let this = this.clone();
                    move |err, cx| {
                        this.update(cx, |this, cx| {
                            this.show_archive_toast(
                                format!("{ws_name}: teardown not run: {err}; archiving anyway"),
                                cx,
                            );
                        })
                        .log_err();
                    }
                },
                cx,
            )
            .await;

            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => this.forget_workspace(&ws_id, cx),
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

            this.update(cx, |this, cx| {
                if let Err(err) = this.register_project(path, cx) {
                    this.show_archive_toast(format!("Couldn't add {}: {err}", path.display()), cx);
                }
            })
            .log_err();
        })
        .detach();
    }

    fn register_project(&mut self, path: &Path, cx: &mut Context<Self>) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        let repository = repo_scanner_service::build_repository(path, &now)?;
        let state = mnemonic_bridge::mnemonic_state(cx);
        repository_repository::upsert(&state.db, &repository)?;
        self.refresh(cx);
        cx.notify();
        log::info!("Added project: {}", path.display());
        Ok(())
    }

    fn open_github_clone_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let panel = cx.weak_entity();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                GitHubCloneModal::new(panel, window, cx)
            });
        });
    }

    /// Clones the repository into the clone location setting with `gh`, which uses the
    /// signed-in account's credentials and preferred protocol, and adds it as
    /// a project. A folder already holding that repository is added as is.
    fn clone_project_from_github(&mut self, full_name: String, cx: &mut Context<Self>) {
        let Some(name) = full_name.rsplit('/').next().map(str::to_string) else {
            return;
        };
        let location = WorkspacesSettings::get_global(cx).clone_location.clone();
        let location = location.trim();
        let folder = match location.strip_prefix('~') {
            Some(rest) => util::paths::home_dir().join(rest.trim_start_matches('/')),
            None => PathBuf::from(location),
        };
        let destination = folder.join(&name);
        self.show_archive_toast(
            format!("Cloning {full_name} into {}…", destination.display()),
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let destination = destination.clone();
                    let full_name = full_name.clone();
                    async move { clone_with_gh(&full_name, &destination).await }
                })
                .await;
            this.update(cx, |this, cx| {
                let result = result.and_then(|()| this.register_project(&destination, cx));
                let message = match result {
                    Ok(()) => format!("Added {full_name}"),
                    Err(err) => format!("Couldn't add {full_name}: {err}"),
                };
                this.show_archive_toast(message, cx);
            })
            .log_err();
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
        let Some(repo) = self
            .repositories
            .iter()
            .find(|r| r.name == project_name)
            .cloned()
        else {
            log::error!("Repository not found for project: {project_name}");
            return;
        };
        let existing_names = self.workspace_names();
        let (workspace_name, first_message) = match seed {
            Some(seed) => (
                unique_workspace_name(&seed.name, &existing_names),
                Some(seed.message),
            ),
            None => (generate_workspace_name(&existing_names), None),
        };
        let created = self.create_named_workspace(
            repo,
            workspace_name,
            ChatAgent::default_agent(cx),
            window,
            cx,
        );

        cx.spawn_in(window, async move |this, cx| match created.await {
            Ok((detail, _primary_chat_id)) => {
                log::info!("Created workspace: {}", detail.summary.id);
                this.update_in(cx, |this, window, cx| {
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
                .log_err();
            }
            Err(e) => {
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
        })
        .detach();
    }

    fn workspace_names(&self) -> Vec<String> {
        self.workspaces.iter().map(|ws| ws.name.clone()).collect()
    }

    /// Creates a workspace in the repository with a generated name and a
    /// primary chat running `agent`, and lists it in this panel. Resolves to
    /// the workspace and the primary chat's record id. Doesn't select it.
    pub(crate) fn create_project_workspace(
        &mut self,
        repo: DiscoveredRepository,
        agent: ChatAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<(WorkspaceDetail, String), String>> {
        let workspace_name = generate_workspace_name(&self.workspace_names());
        self.create_named_workspace(repo, workspace_name, agent, window, cx)
    }

    fn create_named_workspace(
        &mut self,
        repo: DiscoveredRepository,
        workspace_name: String,
        agent: ChatAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<(WorkspaceDetail, String), String>> {
        let Some(state) = mnemonic_bridge::mnemonic_state_cloned() else {
            return Task::ready(Err("Mnemonic isn't ready".to_string()));
        };
        cx.spawn_in(window, async move |this, cx| {
            // Making the worktree and rescanning the repository run git, so
            // this stays off the UI thread or the whole window freezes.
            let created = cx
                .background_spawn(async move {
                    let input = workspace_service::new_workspace_input(
                        &state,
                        &repo,
                        workspace_name,
                        None,
                        None,
                    )?;
                    let detail = workspace_service::create_workspace(&state, input)?;
                    // New workspaces are chat-first: the saved chat session
                    // makes the workspace view open a chat with the agent
                    // instead of the claude CLI.
                    let record = primary_chat_record(&detail.summary.id, agent);
                    if let Err(err) = chat_session_repository::insert(&state.db, &record) {
                        log::error!("Failed to save initial chat session: {err}");
                    }
                    Ok((detail, record.id))
                })
                .await;
            if created.is_ok() {
                this.update(cx, |this, cx| {
                    this.refresh(cx);
                    cx.notify();
                })
                .log_err();
            }
            created
        })
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

    /// Links or unlinks two workspaces and says what that changes.
    fn set_workspace_link(
        &mut self,
        workspace_id: &str,
        other_workspace_id: &str,
        linked: bool,
        cx: &mut Context<Self>,
    ) {
        let state = mnemonic_bridge::mnemonic_state(cx);
        let result = if linked {
            workspace_service::link_workspaces(&state, workspace_id, other_workspace_id)
        } else {
            workspace_service::unlink_workspaces(&state, workspace_id, other_workspace_id)
        };
        let name_of = |id: &str| {
            self.workspaces
                .iter()
                .find(|workspace| workspace.id == id)
                .map_or_else(|| id.to_string(), |workspace| workspace.name.clone())
        };
        let message = match result {
            Ok(()) if linked => format!(
                "Linked {} and {}. Sessions opened in either from now on can use the other's \
                 files; reopen a chat that is already open to pick it up.",
                name_of(workspace_id),
                name_of(other_workspace_id)
            ),
            Ok(()) => format!(
                "Unlinked {} and {}.",
                name_of(workspace_id),
                name_of(other_workspace_id)
            ),
            Err(err) => {
                log::error!("Failed to change the workspace link: {err}");
                format!("Couldn't change the link: {err}")
            }
        };
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<MnemonicLinkToast>(), message),
                    cx,
                );
            });
        }
        self.refresh(cx);
        cx.notify();
    }

    pub(crate) fn open_project_settings(
        &mut self,
        project_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(repository) = self
            .repositories
            .iter()
            .find(|repository| repository.name == project_name)
            .cloned()
        else {
            log::error!("Repository not found for project: {project_name}");
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let weak_workspace = cx.weak_entity();
            workspace.toggle_modal(window, cx, |window, cx| {
                ProjectSettingsModal::new(weak_workspace, &repository, window, cx)
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
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("Projects")
                    .size(LabelSize::Default)
                    .color(Color::Muted),
            )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_1()
            .h(px(32.))
            .flex_none()
            .justify_between()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child({
                let this = cx.weak_entity();
                PopoverMenu::new("add-project-menu")
                    .anchor(gpui::Anchor::BottomLeft)
                    .trigger(
                        Button::new("add-project", "Add project")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .color(Color::Muted)
                            .start_icon(
                                Icon::new(IconName::Plus)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .menu(move |window, cx| {
                        let this = this.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            menu.entry("Local Folder…", None, {
                                let this = this.clone();
                                move |window, cx| {
                                    this.update(cx, |this, cx| this.add_project(window, cx))
                                        .log_err();
                                }
                            })
                            .entry("From GitHub…", None, move |window, cx| {
                                this.update(cx, |this, cx| {
                                    this.open_github_clone_prompt(window, cx)
                                })
                                .log_err();
                            })
                        }))
                    })
            })
            .child(
                IconButton::new("open-workspaces-settings", IconName::Settings)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Workspaces settings"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(
                            Box::new(zed_actions::OpenSettingsAt {
                                path: "workspaces.branch_prefix".to_string(),
                                target: None,
                            }),
                            cx,
                        );
                    }),
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
        let project_for_settings = project.to_string();
        let project_for_hide = project.to_string();
        let project_for_remove = project.to_string();
        let has_repository = self
            .repositories
            .iter()
            .any(|repository| repository.name == project);
        let open_workspace_count = self
            .workspaces
            .iter()
            .filter(|ws| workspace_project(ws) == project)
            .count();
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
                .child({
                    let this = cx.weak_entity();
                    PopoverMenu::new(SharedString::from(format!("project-menu-{project}")))
                        .anchor(gpui::Anchor::TopRight)
                        .trigger(
                            IconButton::new(
                                SharedString::from(format!("project-menu-trigger-{project}")),
                                IconName::Ellipsis,
                            )
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Project options")),
                        )
                        .menu(move |window, cx| {
                            let this = this.clone();
                            let project_for_issue = project_for_issue.clone();
                            let project_for_settings = project_for_settings.clone();
                            let project_for_hide = project_for_hide.clone();
                            let project_for_remove = project_for_remove.clone();
                            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                                menu.entry("New from Issue or PR…", None, {
                                    let this = this.clone();
                                    move |window, cx| {
                                        this.update(cx, |this, cx| {
                                            this.open_issue_prompt(
                                                project_for_issue.clone(),
                                                window,
                                                cx,
                                            );
                                        })
                                        .log_err();
                                    }
                                })
                                .when(has_repository, |menu| {
                                    let this = this.clone();
                                    menu.entry("Project Settings…", None, move |window, cx| {
                                        this.update(cx, |this, cx| {
                                            this.open_project_settings(
                                                project_for_settings.clone(),
                                                window,
                                                cx,
                                            );
                                        })
                                        .log_err();
                                    })
                                })
                                .separator()
                                .entry("Hide Project", None, {
                                    let this = this.clone();
                                    move |_, cx| {
                                        this.update(cx, |this, cx| {
                                            this.set_project_hidden(&project_for_hide, true, cx);
                                        })
                                        .log_err();
                                    }
                                })
                                .when(has_repository, |menu| {
                                    // Its workspaces would be left without the
                                    // project they are created and refreshed from.
                                    let label = if open_workspace_count == 0 {
                                        "Remove Project"
                                    } else {
                                        "Remove Project (archive its workspaces first)"
                                    };
                                    menu.item(
                                        ContextMenuEntry::new(label)
                                            .disabled(open_workspace_count > 0)
                                            .handler(move |_, cx| {
                                                this.update(cx, |this, cx| {
                                                    this.remove_project(
                                                        project_for_remove.clone(),
                                                        cx,
                                                    );
                                                })
                                                .log_err();
                                            }),
                                    )
                                })
                            }))
                        })
                })
                .child(
                    IconButton::new(
                        SharedString::from(format!("add-worktree-{project}")),
                        IconName::Plus,
                    )
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("New workspace"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_worktree(project_for_add.clone(), None, window, cx);
                    })),
                ),
        );

        if !collapsed {
            for ws in workspaces {
                group = group.child(self.render_workspace_row(ws, project, false, cx));
            }
            let archived: Vec<&ArchivedWorkspace> = self
                .archived
                .iter()
                .filter(|entry| {
                    let repo = entry.detail.summary.repo.as_str();
                    (if repo.is_empty() { "ungrouped" } else { repo }) == project
                })
                .collect();
            if !archived.is_empty() {
                group = group.child(self.render_archived_section(project, &archived, cx));
            }
        }

        group
    }

    fn render_pinned_section(
        &self,
        pinned: &[&WorkspaceSummary],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut section = v_flex().w_full().child(
            h_flex()
                .px_2()
                .h(px(28.))
                .gap_1()
                .items_center()
                .w_full()
                .child(
                    Icon::new(IconName::Pin)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new("Pinned").size(LabelSize::Default))
                .child(
                    Label::new(pinned.len().to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        );
        for ws in pinned {
            let project = workspace_project(ws);
            section = section.child(self.render_workspace_row(ws, &project, true, cx));
        }
        section
    }

    /// A collapsed "Hidden (n)" row below the projects that expands to the
    /// hidden projects, each with a button to show it again.
    fn render_hidden_projects(
        &self,
        hidden: &[String],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let expanded = self.showing_hidden_projects;
        let mut section = v_flex().w_full().child(
            h_flex()
                .id("hidden-projects")
                .px_2()
                .h(px(28.))
                .gap_1()
                .items_center()
                .w_full()
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                .on_click(cx.listener(|this, _, _window, cx| {
                    this.showing_hidden_projects = !this.showing_hidden_projects;
                    cx.notify();
                }))
                .child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
                )
                .child(
                    Label::new(format!("Hidden ({})", hidden.len()))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        );
        if expanded {
            for project in hidden {
                let project_for_show = project.clone();
                section = section.child(
                    h_flex()
                        .pl(px(28.))
                        .pr_2()
                        .h(px(22.))
                        .gap_1p5()
                        .items_center()
                        .w_full()
                        .child(
                            div().flex_grow(1.0).overflow_hidden().child(
                                Label::new(project.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .single_line(),
                            ),
                        )
                        .child(
                            IconButton::new(
                                SharedString::from(format!("show-project-{project}")),
                                IconName::Eye,
                            )
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Show project"))
                            .on_click(cx.listener(
                                move |this, _, _window, cx| {
                                    this.set_project_hidden(&project_for_show, false, cx);
                                },
                            )),
                        ),
                );
            }
        }
        section
    }

    /// A collapsed "Archived (n)" row under a project's workspaces that
    /// expands to its archived workspaces, each with a restore button.
    fn render_archived_section(
        &self,
        project: &str,
        archived: &[&ArchivedWorkspace],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let expanded = self.expanded_archives.iter().any(|name| name == project);
        let project_owned = project.to_string();
        let mut section = v_flex().w_full().child(
            h_flex()
                .id(SharedString::from(format!("archived-{project}")))
                .pl(px(28.))
                .pr_2()
                .h(px(22.))
                .gap_1()
                .items_center()
                .w_full()
                .cursor_pointer()
                .rounded_md()
                .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    match this
                        .expanded_archives
                        .iter()
                        .position(|name| name == &project_owned)
                    {
                        Some(position) => {
                            this.expanded_archives.remove(position);
                        }
                        None => this.expanded_archives.push(project_owned.clone()),
                    }
                    cx.notify();
                }))
                .child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
                )
                .child(
                    Label::new(format!("Archived ({})", archived.len()))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        );
        if expanded {
            for entry in archived {
                let workspace_id = entry.detail.summary.id.clone();
                let name = entry.detail.summary.name.clone();
                section = section.child(
                    h_flex()
                        .pl(px(44.))
                        .pr_2()
                        .h(px(22.))
                        .gap_1p5()
                        .items_center()
                        .w_full()
                        .child(
                            Icon::new(IconName::Archive)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            div().flex_grow(1.0).overflow_hidden().child(
                                Label::new(name.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .single_line(),
                            ),
                        )
                        .child(
                            IconButton::new(
                                SharedString::from(format!("restore-{workspace_id}")),
                                IconName::RotateCcw,
                            )
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Restore workspace"))
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.restore_workspace(
                                        workspace_id.clone(),
                                        name.clone(),
                                        window,
                                        cx,
                                    );
                                },
                            )),
                        ),
                );
            }
        }
        section
    }

    fn render_workspace_row(
        &self,
        ws: &WorkspaceSummary,
        project: &str,
        in_pinned_section: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let workspace_label = self.workspace_label(&ws.id).map(str::to_string);
        let menu_label = workspace_label.clone();
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
        let additions: u32 = ws.changed_files.iter().map(|file| file.additions).sum();
        let deletions: u32 = ws.changed_files.iter().map(|file| file.deletions).sum();
        let linked_ids = workspace_service::linked_workspace_ids(&self.workspace_links, &ws.id);
        // Every other workspace, as (id, "project / name", linked), for the
        // Link Workspace submenu.
        let mut link_candidates: Vec<(String, String, bool)> = self
            .workspaces
            .iter()
            .filter(|other| other.id != ws.id)
            .map(|other| {
                (
                    other.id.clone(),
                    format!("{} / {}", other.repo, other.name),
                    linked_ids.contains(&other.id.as_str()),
                )
            })
            .collect();
        link_candidates.sort_by(|a, b| a.1.cmp(&b.1));
        let linked_names: Vec<&str> = link_candidates
            .iter()
            .filter(|(_, _, linked)| *linked)
            .map(|(_, label, _)| label.as_str())
            .collect();
        let linked_tooltip =
            (!linked_names.is_empty()).then(|| format!("Linked to {}", linked_names.join(", ")));

        let row_group = SharedString::from(format!("workspace-row-{}", ws.id));
        let pr_state = mnemonic_bridge::pr_state(&ws.id);
        let pr_color = pr_state
            .as_deref()
            .and_then(|state| mnemonic_bridge::pr_state_color(state, cx));
        let merged_color = pr_color.filter(|_| pr_state.as_deref() == Some("merged"));
        let shared = crate::team::is_workspace_shared(&ws.id, cx);
        let shared_indicator = crate::team::render_shared_indicator(&ws.id, cx);
        let row = h_flex()
            .id(SharedString::from(ws.id.clone()))
            .group(row_group.clone())
            .pl(px(28.))
            .pr_2()
            .h(px(24.))
            .gap_1p5()
            .items_center()
            .w_full()
            .cursor_pointer()
            .rounded_md()
            .when_some(merged_color, |el, color| el.bg(color.opacity(0.12)))
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
                    if in_pinned_section {
                        this.reorder_pinned(&dragged.id, &drop_target_id, cx);
                    } else {
                        this.reorder_workspace(
                            &dragged.id,
                            &drop_target_project,
                            &drop_target_id,
                            cx,
                        );
                    }
                    cx.notify();
                },
            ))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_workspace(ws_id.clone(), window, cx);
            }))
            .child(self.render_agent_indicator(&ws.id, cx))
            .child({
                let icon_color = match pr_color {
                    Some(color) => Color::Custom(color),
                    None if is_selected => Color::Default,
                    None => Color::Muted,
                };
                let icon = div()
                    .id(SharedString::from(format!("pr-state-{}", ws.id)))
                    .child(
                        Icon::new(IconName::FileGit)
                            .size(IconSize::XSmall)
                            .color(icon_color),
                    );
                match pr_state.as_deref() {
                    Some(state) => {
                        let label = mnemonic_bridge::pr_state_label(state);
                        let tooltip = match mnemonic_bridge::pr_number(&ws.id) {
                            Some(number) => format!("PR #{number}: {label}"),
                            None => format!("PR: {label}"),
                        };
                        icon.tooltip(Tooltip::text(tooltip))
                    }
                    None => icon,
                }
            })
            .child(
                h_flex()
                    .flex_grow(1.0)
                    .min_w_0()
                    .gap_1()
                    .overflow_hidden()
                    .child(
                        Label::new(ws.name.clone())
                            .size(LabelSize::Default)
                            .color(Color::Default)
                            .single_line(),
                    )
                    .when(in_pinned_section, |el| {
                        el.child(
                            Label::new(project.to_string())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line(),
                        )
                    }),
            )
            .when_some(workspace_label, |el, label| {
                el.child(
                    div()
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .bg(cx.theme().colors().element_background)
                        .child(
                            Label::new(label)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line(),
                        ),
                )
            })
            .when_some(shared_indicator, |el, indicator| el.child(indicator))
            .when_some(linked_tooltip, |el, tooltip| {
                el.child(
                    div()
                        .id(SharedString::from(format!("linked-{}", ws.id)))
                        .child(
                            Icon::new(IconName::Link)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .tooltip(Tooltip::text(tooltip)),
                )
            })
            .when(additions > 0 || deletions > 0, |el| {
                el.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Label::new(format!("+{additions}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Created),
                        )
                        .child(
                            Label::new(format!("-{deletions}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Deleted),
                        ),
                )
            })
            .child(
                IconButton::new(
                    SharedString::from(format!("archive-{}", ws.id)),
                    IconName::Archive,
                )
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .visible_on_hover(row_group)
                .tooltip(Tooltip::text("Archive workspace"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.archive_workspace(
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
                let link_candidates = link_candidates.clone();
                let menu_label = menu_label.clone();
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
                    .entry(
                        if shared {
                            "Stop Sharing with Team"
                        } else {
                            "Share with Team"
                        },
                        None,
                        {
                            let workspace_id = workspace_id.clone();
                            move |_, cx| {
                                crate::team::set_workspace_shared(&workspace_id, !shared, cx);
                            }
                        },
                    )
                    .when(!link_candidates.is_empty(), |menu| {
                        menu.submenu("Link Workspace", {
                            let this = this.clone();
                            let workspace_id = workspace_id.clone();
                            move |menu, _, _| {
                                link_candidates.iter().fold(
                                    menu,
                                    |menu, (other_workspace_id, label, linked)| {
                                        let this = this.clone();
                                        let workspace_id = workspace_id.clone();
                                        let other_workspace_id = other_workspace_id.clone();
                                        let linked = *linked;
                                        menu.toggleable_entry(
                                            label.clone(),
                                            linked,
                                            IconPosition::Start,
                                            None,
                                            move |_, cx| {
                                                this.update(cx, |this, cx| {
                                                    this.set_workspace_link(
                                                        &workspace_id,
                                                        &other_workspace_id,
                                                        !linked,
                                                        cx,
                                                    );
                                                })
                                                .log_err();
                                            },
                                        )
                                    },
                                )
                            }
                        })
                    })
                    .entry("Rename…", None, {
                        let this = this.clone();
                        let workspace_id = workspace_id.clone();
                        let workspace_name = workspace_name.clone();
                        move |window, cx| {
                            let this = this.clone();
                            let workspace_id = workspace_id.clone();
                            let workspace_name = workspace_name.clone();
                            // Opened once the menu has closed and given focus
                            // back, so the prompt keeps it.
                            window.defer(cx, move |window, cx| {
                                this.update(cx, |this, cx| {
                                    this.open_text_prompt(
                                        WorkspaceTextPrompt::Rename,
                                        workspace_id,
                                        workspace_name,
                                        window,
                                        cx,
                                    );
                                })
                                .log_err();
                            });
                        }
                    })
                    .separator()
                    .entry(
                        if in_pinned_section {
                            "Unpin"
                        } else {
                            "Pin to Top"
                        },
                        None,
                        {
                            let this = this.clone();
                            let workspace_id = workspace_id.clone();
                            move |_, cx| {
                                this.update(cx, |this, cx| {
                                    this.set_workspace_pinned(
                                        &workspace_id,
                                        !in_pinned_section,
                                        cx,
                                    );
                                })
                                .log_err();
                            }
                        },
                    )
                    .entry(
                        if menu_label.is_some() {
                            "Edit Label…"
                        } else {
                            "Add Label…"
                        },
                        None,
                        {
                            let this = this.clone();
                            let workspace_id = workspace_id.clone();
                            let current_label = menu_label.clone().unwrap_or_default();
                            move |window, cx| {
                                let this = this.clone();
                                let workspace_id = workspace_id.clone();
                                let current_label = current_label.clone();
                                window.defer(cx, move |window, cx| {
                                    this.update(cx, |this, cx| {
                                        this.open_text_prompt(
                                            WorkspaceTextPrompt::Label,
                                            workspace_id,
                                            current_label,
                                            window,
                                            cx,
                                        );
                                    })
                                    .log_err();
                                });
                            }
                        },
                    )
                    .when(menu_label.is_some(), |menu| {
                        let this = this.clone();
                        let workspace_id = workspace_id.clone();
                        menu.entry("Remove Label", None, move |_, cx| {
                            this.update(cx, |this, cx| {
                                this.set_workspace_label(&workspace_id, "", cx);
                            })
                            .log_err();
                        })
                    })
                    .entry("Move Up", None, {
                        let this = this.clone();
                        let workspace_id = workspace_id.clone();
                        move |_, cx| {
                            this.update(cx, |this, cx| {
                                this.move_workspace(&workspace_id, true, cx);
                            })
                            .log_err();
                        }
                    })
                    .entry("Move Down", None, {
                        let this = this.clone();
                        let workspace_id = workspace_id.clone();
                        move |_, cx| {
                            this.update(cx, |this, cx| {
                                this.move_workspace(&workspace_id, false, cx);
                            })
                            .log_err();
                        }
                    })
                    .separator()
                    .entry("Archive", None, move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.archive_workspace(
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
                // Not animated: an animation re-renders the whole panel on
                // every frame for as long as any agent runs.
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
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
            .w_full()
            .flex_grow(1.0)
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

/// The chat a new workspace opens with, in place of the claude CLI.
fn workspace_project(workspace: &WorkspaceSummary) -> String {
    if workspace.repo.is_empty() {
        "ungrouped".to_string()
    } else {
        workspace.repo.clone()
    }
}

/// Workspaces missing from the saved order are newer than it, so they rank
/// first, keeping the newest-first order the database lists them in.
fn saved_order_rank(order: &[String], workspace_id: &str) -> usize {
    order
        .iter()
        .position(|id| id == workspace_id)
        .map_or(0, |position| position + 1)
}

/// Moves `dragged_id` to where `target_id` is, shifting the ones between.
/// Returns false when there is nothing to move.
fn move_onto(order: &mut Vec<String>, dragged_id: &str, target_id: &str) -> bool {
    if dragged_id == target_id {
        return false;
    }
    let (Some(from), Some(to)) = (
        order.iter().position(|id| id == dragged_id),
        order.iter().position(|id| id == target_id),
    ) else {
        return false;
    };
    let id = order.remove(from);
    order.insert(to, id);
    true
}

pub(crate) fn primary_chat_record(workspace_id: &str, agent: ChatAgent) -> ChatSessionRecord {
    ChatSessionRecord {
        id: uuid::Uuid::new_v4().to_string(),
        workspace_id: workspace_id.to_string(),
        agent: agent.agent_id().to_string(),
        title: agent.label().to_string(),
        acp_session_id: None,
        is_review: false,
        is_primary: true,
        position: 0,
    }
}

/// Conductor-style two-word workspace names ("patient-cipher"), seeded from
/// the clock so no rand dependency is needed. Falls back to a numeric suffix
/// in the unlikely case the random picks keep colliding.
pub(crate) fn generate_workspace_name(existing: &[String]) -> String {
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
pub(crate) fn unique_workspace_name(name: &str, existing: &[String]) -> String {
    let taken = |candidate: &str| existing.iter().any(|existing| existing == candidate);
    if !taken(name) {
        return name.to_string();
    }
    (2..)
        .map(|counter| format!("{name}-{counter}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| name.to_string())
}

/// `owner/repo` from `owner/repo`, a github.com URL (with or without
/// `.git`), or an SSH remote like `git@github.com:owner/repo.git`.
fn parse_github_repository(text: &str) -> Option<String> {
    let text = text.trim().trim_end_matches('/');
    let path = text
        .strip_prefix("git@github.com:")
        .or_else(|| {
            let without_scheme = text.split_once("://").map_or(text, |(_, rest)| rest);
            without_scheme
                .strip_prefix("www.github.com/")
                .or_else(|| without_scheme.strip_prefix("github.com/"))
        })
        .unwrap_or(text);
    let path = path.trim_end_matches(".git");
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let repository = segments.next()?;
    let valid = |segment: &str| {
        !segment.is_empty()
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (valid(owner) && valid(repository)).then(|| format!("{owner}/{repository}"))
}

async fn clone_with_gh(full_name: &str, destination: &Path) -> Result<(), String> {
    if destination.join(".git").exists() {
        return Ok(());
    }
    if destination.exists() {
        return Err(format!(
            "{} already exists and isn't a git repository",
            destination.display()
        ));
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let gh = environment_service::find_gh_path().unwrap_or_else(|| "gh".to_string());
    let output = util::command::new_command(gh)
        .arg("repo")
        .arg("clone")
        .arg(full_name)
        .arg(destination)
        .output()
        .await
        .map_err(|err| format!("couldn't run gh: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Repositories the signed-in GitHub account can reach, most recently
/// pushed first.
async fn list_github_repositories() -> Result<Vec<String>, String> {
    let gh = environment_service::find_gh_path().unwrap_or_else(|| "gh".to_string());
    let output = util::command::new_command(gh)
        .arg("api")
        .arg("user/repos?per_page=100&sort=pushed&affiliation=owner,collaborator,organization_member")
        .arg("--jq")
        .arg(".[].full_name")
        .output()
        .await
        .map_err(|err| format!("couldn't run gh: {err}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Number of matching repositories listed under the GitHub prompt.
const GITHUB_REPOSITORY_MATCHES: usize = 8;

/// "Add project > From GitHub": type `owner/repo` or a URL, or pick one of
/// your repositories.
struct GitHubCloneModal {
    editor: Entity<Editor>,
    panel: WeakEntity<WorkspacePanel>,
    repositories: Option<Result<Vec<String>, String>>,
    error: Option<SharedString>,
    _subscription: Subscription,
    _load: Task<()>,
}

impl GitHubCloneModal {
    fn new(panel: WeakEntity<WorkspacePanel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("owner/repo or https://github.com/owner/repo", window, cx);
            editor
        });
        let subscription = cx.subscribe(&editor, |this, _, event: &editor::EditorEvent, cx| {
            if matches!(event, editor::EditorEvent::BufferEdited) {
                this.error = None;
                cx.notify();
            }
        });
        let load = cx.spawn(async move |this, cx| {
            let repositories = cx.background_spawn(list_github_repositories()).await;
            this.update(cx, |this, cx| {
                this.repositories = Some(repositories);
                cx.notify();
            })
            .log_err();
        });
        Self {
            editor,
            panel,
            repositories: None,
            error: None,
            _subscription: subscription,
            _load: load,
        }
    }

    fn matches(&self, cx: &App) -> Vec<String> {
        let Some(Ok(repositories)) = &self.repositories else {
            return Vec::new();
        };
        let query = self.editor.read(cx).text(cx).trim().to_lowercase();
        repositories
            .iter()
            .filter(|repository| repository.to_lowercase().contains(&query))
            .take(GITHUB_REPOSITORY_MATCHES)
            .cloned()
            .collect()
    }

    fn clone_repository(&mut self, full_name: String, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.clone_project_from_github(full_name, cx))
            .log_err();
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx);
        let chosen = parse_github_repository(&text)
            .or_else(|| self.matches(cx).into_iter().next());
        match chosen {
            Some(full_name) => self.clone_repository(full_name, cx),
            None => {
                self.error = Some("Type owner/repo, a GitHub URL, or pick a repository".into());
                cx.notify();
            }
        }
    }
}

impl EventEmitter<DismissEvent> for GitHubCloneModal {}

impl ModalView for GitHubCloneModal {}

impl Focusable for GitHubCloneModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for GitHubCloneModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let matches = self.matches(cx);
        let hint = match (&self.error, &self.repositories) {
            (Some(error), _) => Label::new(error.clone()).color(Color::Error),
            (None, None) => Label::new("Loading your repositories…").color(Color::Muted),
            (None, Some(Err(err))) => {
                Label::new(format!("Couldn't list repositories: {err}")).color(Color::Muted)
            }
            (None, Some(Ok(_))) => {
                Label::new("Clones into the folder set in Settings > Workspaces and adds it as a project.")
                    .color(Color::Muted)
            }
        };
        v_flex()
            .key_context("GitHubCloneModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(px(480.))
            .overflow_hidden()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.editor.clone()),
            )
            .child(
                v_flex()
                    .p_1()
                    .children(matches.into_iter().enumerate().map(|(index, full_name)| {
                        h_flex()
                            .id(("github-repository", index))
                            .px_2()
                            .h(px(26.))
                            .gap_1p5()
                            .items_center()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                            .child(
                                Icon::new(IconName::Github)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(full_name.clone()).size(LabelSize::Small))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.clone_repository(full_name.clone(), cx);
                            }))
                    })),
            )
            .child(
                h_flex()
                    .bg(cx.theme().colors().editor_background)
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .w_full()
                    .p_2()
                    .child(hint.size(LabelSize::Small)),
            )
    }
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

/// Per-project settings: where new workspaces branch from and what is
/// prepared in them. Saved per repository, next to the app-wide ones in
/// Settings > Workspaces.
struct ProjectSettingsModal {
    workspace: WeakEntity<Workspace>,
    repository_id: String,
    project_name: String,
    base_branch: Entity<Editor>,
    setup_script: Entity<Editor>,
    run_script: Entity<Editor>,
    init_files: Entity<Editor>,
    error: Option<SharedString>,
}

impl ProjectSettingsModal {
    fn new(
        workspace: WeakEntity<Workspace>,
        repository: &DiscoveredRepository,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = mnemonic_bridge::mnemonic_state(cx);
        let mut error = None;
        let mut load = |key: &str| {
            match repository_settings_repository::get_value(&state.db, &repository.id, key) {
                Ok(value) => value.unwrap_or_default(),
                Err(err) => {
                    error = Some(SharedString::from(format!(
                        "Couldn't load the saved settings: {err}"
                    )));
                    String::new()
                }
            }
        };
        let base_branch_value = load(PROJECT_BASE_BRANCH_KEY);
        let setup_script_value = load(PROJECT_SETUP_SCRIPT_KEY);
        let run_script_value = load(PROJECT_RUN_SCRIPT_KEY);
        let init_files_value = load(PROJECT_INIT_FILES_KEY);

        let default_base_branch = WorkspacesSettings::get_global(cx)
            .default_base_branch
            .clone();
        let base_branch = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(&default_base_branch, window, cx);
            editor.set_text(base_branch_value, window, cx);
            editor
        });
        let setup_script = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 8, window, cx);
            editor.set_placeholder_text("pnpm install", window, cx);
            editor.set_text(setup_script_value, window, cx);
            editor
        });
        let run_script = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 8, window, cx);
            editor.set_placeholder_text("pnpm dev", window, cx);
            editor.set_text(run_script_value, window, cx);
            editor
        });
        let init_files = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 8, window, cx);
            editor.set_placeholder_text(".env.local", window, cx);
            editor.set_text(init_files_value, window, cx);
            editor
        });

        Self {
            workspace,
            repository_id: repository.id.clone(),
            project_name: repository.name.clone(),
            base_branch,
            setup_script,
            run_script,
            init_files,
            error,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
    }

    fn secondary_confirm(
        &mut self,
        _: &menu::SecondaryConfirm,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save(cx);
    }

    /// Saves every field, clearing the ones left empty so the app-wide
    /// defaults apply again.
    fn save(&mut self, cx: &mut Context<Self>) {
        let state = mnemonic_bridge::mnemonic_state(cx);
        let fields = [
            (PROJECT_BASE_BRANCH_KEY, self.base_branch.read(cx).text(cx)),
            (PROJECT_SETUP_SCRIPT_KEY, self.setup_script.read(cx).text(cx)),
            (PROJECT_RUN_SCRIPT_KEY, self.run_script.read(cx).text(cx)),
            (PROJECT_INIT_FILES_KEY, self.init_files.read(cx).text(cx)),
        ];
        for (key, value) in fields {
            let value = value.trim();
            let result = if value.is_empty() {
                repository_settings_repository::delete_value(&state.db, &self.repository_id, key)
            } else {
                repository_settings_repository::set_value(
                    &state.db,
                    &self.repository_id,
                    key,
                    value,
                )
            };
            if let Err(err) = result {
                self.error = Some(SharedString::from(format!("Couldn't save: {err}")));
                cx.notify();
                return;
            }
        }
        self.workspace
            .update(cx, |workspace, cx| {
                WorkspaceHost::reload_run_configs(workspace, cx);
            })
            .log_err();
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for ProjectSettingsModal {}

impl ModalView for ProjectSettingsModal {}

impl Focusable for ProjectSettingsModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.base_branch.focus_handle(cx)
    }
}

impl Render for ProjectSettingsModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().colors().border_variant;
        let field_background = cx.theme().colors().editor_background;
        let field = |title: &'static str, hint: &'static str, editor: &Entity<Editor>| {
            v_flex()
                .gap_1()
                .child(Label::new(title).size(LabelSize::Small))
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(border)
                        .bg(field_background)
                        .child(editor.clone()),
                )
                .child(
                    Label::new(hint)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
        };

        v_flex()
            .key_context("ProjectSettingsModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::secondary_confirm))
            .elevation_3(cx)
            .w(rems(34.))
            .p_3()
            .gap_3()
            .child(Label::new(format!("{} settings", self.project_name)))
            .child(field(
                "Base branch",
                "New workspaces in this project start from this branch. Leave empty to use \
                 the default from Settings > Workspaces.",
                &self.base_branch,
            ))
            .child(field(
                "Setup script",
                "Runs in a terminal in each new workspace.",
                &self.setup_script,
            ))
            .child(field(
                "Run script",
                "Started by the workspace's Run button, in a terminal at the bottom. Leave \
                 empty to use the \"run\" commands from .mnemonic/config.json.",
                &self.run_script,
            ))
            .child(field(
                "Files to copy",
                "Copied from the project root into each new workspace, one path per line.",
                &self.init_files,
            ))
            .child(
                Label::new(
                    "Archive commands are read from .mnemonic/config.json in the repository.",
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    .items_center()
                    .child(match &self.error {
                        Some(error) => Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                        None => Label::new(""),
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("project-settings-cancel", "Cancel")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(DismissEvent);
                                    })),
                            )
                            .child(
                                Button::new("project-settings-save", "Save")
                                    .style(ButtonStyle::Filled)
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save(cx);
                                    })),
                            ),
                    ),
            )
    }
}

#[derive(Clone, Copy)]
enum WorkspaceTextPrompt {
    Rename,
    Label,
}

struct WorkspaceTextModal {
    editor: Entity<Editor>,
    panel: WeakEntity<WorkspacePanel>,
    purpose: WorkspaceTextPrompt,
    workspace_id: String,
}

impl WorkspaceTextModal {
    fn new(
        panel: WeakEntity<WorkspacePanel>,
        purpose: WorkspaceTextPrompt,
        workspace_id: String,
        current_text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            if matches!(purpose, WorkspaceTextPrompt::Label) {
                editor.set_placeholder_text("Label, e.g. review or blocked", window, cx);
            }
            editor.set_text(current_text, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor
        });
        Self {
            editor,
            panel,
            purpose,
            workspace_id,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx);
        let workspace_id = self.workspace_id.clone();
        match self.purpose {
            WorkspaceTextPrompt::Rename => {
                if !text.trim().is_empty() {
                    self.panel
                        .update(cx, |panel, cx| {
                            panel.rename_workspace(workspace_id, text, window, cx);
                        })
                        .log_err();
                }
            }
            WorkspaceTextPrompt::Label => {
                self.panel
                    .update(cx, |panel, cx| {
                        panel.set_workspace_label(&workspace_id, &text, cx);
                    })
                    .log_err();
            }
        }
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for WorkspaceTextModal {}

impl ModalView for WorkspaceTextModal {}

impl Focusable for WorkspaceTextModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for WorkspaceTextModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = match self.purpose {
            WorkspaceTextPrompt::Rename => {
                "Rename the workspace. Its branch is renamed too, until it has \
                 been pushed or has a PR."
            }
            WorkspaceTextPrompt::Label => "Label the workspace. Leave empty to remove the label.",
        };
        v_flex()
            .key_context("RenameWorkspaceModal")
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
                    .child(Label::new(hint).size(LabelSize::Small).color(Color::Muted)),
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
        let pinned = self.pinned_workspace_summaries();
        let has_workspaces = !groups.is_empty() || !pinned.is_empty();

        if has_workspaces {
            let mut list = v_flex().py_0p5().overflow_y_hidden().flex_grow(1.0);
            if !pinned.is_empty() {
                list = list.child(self.render_pinned_section(&pinned, cx));
            }
            let mut hidden = Vec::new();
            for (project, workspaces) in &groups {
                if self.is_project_hidden(project) {
                    hidden.push(project.clone());
                } else {
                    list = list.child(self.render_project_group(project, &workspaces, cx));
                }
            }
            if !hidden.is_empty() {
                list = list.child(self.render_hidden_projects(&hidden, cx));
            }
            list = list.children(crate::team::render_team_section(self.workspace.clone(), cx));
            content = content.child(list);
        } else {
            content = content
                .child(self.render_empty())
                .children(crate::team::render_team_section(self.workspace.clone(), cx));
        }

        content.child(self.render_footer(cx))
    }
}

/// Archives one of your own workspaces, as the sidebar and the agent tools
/// both do: runs its teardown commands in its view in `window`, stops its run
/// commands, frees its ports, moves the worktree into the archive folder and
/// drops its chat records. `prepare_view` makes sure the view exists in
/// `window` before the teardown runs, and is only called when there is one.
/// A teardown that can't be prepared goes to `teardown_failed` and the
/// archive goes ahead.
pub(crate) async fn archive_own_workspace(
    window: WeakEntity<Workspace>,
    window_handle: AnyWindowHandle,
    ws_id: String,
    state: mnemonic_core::state::AppState,
    prepare_view: impl FnOnce(&mut Window, &mut App),
    teardown_failed: impl FnOnce(String, &mut App),
    cx: &mut AsyncApp,
) -> Result<(), String> {
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
            let teardown_task = cx
                .update_window(window_handle, |_, window_context, cx| {
                    prepare_view(window_context, cx);
                    window.upgrade()?.update(cx, |workspace, cx| {
                        WorkspaceHost::run_teardown_for_workspace(
                            workspace,
                            &ws_id,
                            root,
                            plans,
                            window_context,
                            cx,
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
        Err(err) => cx.update(|cx| teardown_failed(err, cx)),
    }
    if let Some(workspace) = window.upgrade() {
        cx.update(|cx| {
            workspace.update(cx, |workspace, cx| {
                WorkspaceHost::kill_run_commands_for_workspace(workspace, &ws_id, cx);
            })
        });
    }
    cx.background_spawn(async move {
        // Ports are matched by process cwd, so they must be freed before the
        // worktree moves into the archive folder.
        for warning in free_workspace_ports(&state, &ws_id) {
            log::warn!("Archive {ws_id}: {warning}");
        }
        workspace_service::archive_workspace(&state, &ws_id)?;
        if let Err(err) = chat_session_repository::delete_for_workspace(&state.db, &ws_id) {
            log::error!("Failed to delete chat sessions for {ws_id}: {err}");
        }
        Ok::<(), String>(())
    })
    .await
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
    fn a_new_workspace_opens_a_chat_with_the_chosen_agent() {
        let record = primary_chat_record("ws-1", ChatAgent::Codex);

        assert_eq!(record.workspace_id, "ws-1");
        assert_eq!(record.agent, ChatAgent::Codex.agent_id());
        assert!(!crate::workspace_view::needs_first_chat(&[record]));
    }

    #[test]
    fn parses_github_repository_references() {
        let expected = Some("acme/app".to_string());
        assert_eq!(parse_github_repository("acme/app"), expected);
        assert_eq!(parse_github_repository("https://github.com/acme/app"), expected);
        assert_eq!(parse_github_repository("https://github.com/acme/app.git/"), expected);
        assert_eq!(parse_github_repository("git@github.com:acme/app.git"), expected);
        assert_eq!(
            parse_github_repository("https://github.com/acme/app/pull/12"),
            expected
        );
        assert_eq!(parse_github_repository("app"), None);
        assert_eq!(parse_github_repository("acme/my app"), None);
    }

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
    fn new_workspaces_list_above_a_saved_order() {
        let saved = vec!["old-a".to_string(), "old-b".to_string()];
        let mut listed = vec!["old-b", "newest", "old-a", "newer"];
        listed.sort_by_key(|id| saved_order_rank(&saved, id));
        assert_eq!(listed, vec!["newest", "newer", "old-a", "old-b"]);
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

