use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    actions, px, Action, App, AppContext as _, Context, Entity, EventEmitter, Focusable,
    FocusHandle, IntoElement, PathPromptOptions, Pixels, PromptLevel, Render, SharedString,
    WeakEntity, Window,
};
use mnemonic_core::models::{
    ActivityItem, ChatSessionRecord, CreateWorkspaceInput, DiscoveredRepository, WorkspaceSummary,
};
use mnemonic_core::repositories::{
    activity_repository, chat_session_repository, repository_repository, settings_repository,
};
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
    showing_history: bool,
    activities: Vec<ActivityItem>,
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
            workspace.update(cx, |workspace, cx| {
                WorkspaceHost::activate_workspace(workspace, &summary, window, cx);
                Self::point_project_at_worktree(workspace, &summary, cx);
            });
        }
    }

    /// Archive after confirmation: stops the agent session, removes the
    /// worktree from disk (clean worktrees only), keeps the branch.
    fn confirm_archive(
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
                "Stops the agent session and moves the worktree into the archive folder. \
                 Nothing is deleted.",
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
            let result = {
                let ws_id = ws_id.clone();
                cx.background_spawn(async move {
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
        let ws_id_for_archive = ws.id.clone();
        let ws_name_for_archive = ws.name.clone();

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
            .child({
                // Live attention dot: green while claude is producing
                // output, amber when it has gone quiet (waiting on you),
                // muted when no session has run yet.
                let dot_color = match mnemonic_bridge::last_agent_activity(&ws.id) {
                    Some(at) if at.elapsed() < Duration::from_secs(4) => {
                        cx.theme().status().success
                    }
                    Some(_) => cx.theme().status().warning,
                    None => cx.theme().colors().text_muted,
                };
                div().w(px(7.)).h(px(7.)).rounded_full().bg(dot_color)
            })
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
                list = list.child(self.render_project_group(project, workspaces, cx));
            }
            content = content.child(list);
        } else {
            content = content.child(self.render_empty());
        }

        content
    }
}
