use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use editor::Editor;
use git_ui::project_diff::{BranchDiff, ProjectDiff};
use gpui::{
    actions, px, Action, Anchor, AnyElement, App, AppContext as _, Context, Entity, EventEmitter,
    Focusable, FocusHandle, IntoElement, Pixels, PromptLevel, Render, ScrollHandle, SharedString,
    Task, WeakEntity, Window,
};
use mnemonic_core::models::{
    ChangedFile, WorkspaceChangeScope, WorkspaceCheckpoint, WorkspaceCheckpointRestorePlan,
    WorkspacePort, WorkspacePrComment, WorkspacePrStatus, WorkspaceSummary,
};
use mnemonic_core::services::{
    checkpoint_service, conflict_detection_service, git_review_service, pr_draft_service,
    review_cockpit_service, workspace_port_service, workspace_service,
};
use project::{Project, git_store::Repository};
use settings::{ReviewAgent, Settings as _};
use ui::{
    h_flex, v_flex, Button, ButtonStyle, ContextMenu, DiffStat, Icon, IconButton, IconName,
    Label, PopoverMenu, TintColor, Tooltip, prelude::*,
};
use std::time::{Duration, Instant};

use workspace::dock::{DockPosition, PanelEvent};
use workspace::notifications::NotificationId;
use workspace::{OpenOptions, Panel, Toast, Workspace};

use crate::ActiveWorkspace;
use crate::workspace_panel::WorkspacePanel;
use crate::workspace_view::WorkspaceHost;
use crate::workspaces_settings::WorkspacesSettings;

actions!(mnemonic_workspace_detail_panel, [ToggleFocus]);

const WORKSPACE_DETAIL_PANEL_KEY: &str = "MnemonicWorkspaceDetailPanel";

#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailTab {
    AllFiles,
    Changes,
    Checks,
    Ports,
    Checkpoints,
}

/// How often the panel re-queries GitHub while a workspace is selected.
const PR_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Faster cadence right after dispatching an instruction (PR creation, CI
/// fix), so the panel catches the result without a manual refresh.
const PR_POLL_FAST_INTERVAL: Duration = Duration::from_secs(15);
/// How long the fast cadence lasts after a dispatch.
const PR_POLL_BOOST: Duration = Duration::from_secs(300);

/// How often the Ports tab rescans while it is showing. Matches the port
/// service's cache lifetime, so each poll is a real scan.
const PORTS_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// How stale viewed marks and cross-workspace overlaps may get while the
/// Changes tab keeps rendering an unchanged list. Catches in-place edits that
/// keep the line counts and edits made in other workspaces.
const CHANGE_ANNOTATIONS_RECHECK_INTERVAL: Duration = Duration::from_secs(10);

/// Above this many changed files the list groups rows under folder headers.
const CHANGE_FOLDER_THRESHOLD: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChangeGroup {
    Uncommitted,
    OnBranch,
}

impl ChangeGroup {
    fn label(self) -> &'static str {
        match self {
            ChangeGroup::Uncommitted => "Uncommitted",
            ChangeGroup::OnBranch => "On branch",
        }
    }
}

enum ChangeListEntry<'a> {
    Group {
        group: ChangeGroup,
        count: usize,
    },
    Folder {
        key: String,
        directory: &'a str,
        count: usize,
        collapsed: bool,
    },
    File {
        file: &'a ChangedFile,
        in_folder: bool,
    },
}

/// Splits a changed path into (directory, file name). Untracked directories
/// arrive from git with a trailing slash.
fn split_change_path(path: &str) -> (&str, &str) {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit_once('/').unwrap_or(("", trimmed))
}

/// The Changes list rows in display order. Without a loaded scope the files
/// are listed ungrouped rather than guessed into a group.
fn change_list_entries<'a>(
    files: &'a [ChangedFile],
    uncommitted_paths: Option<&HashSet<String>>,
    collapsed_folders: &HashSet<String>,
) -> Vec<ChangeListEntry<'a>> {
    let groups: Vec<(Option<ChangeGroup>, Vec<&'a ChangedFile>)> = match uncommitted_paths {
        Some(uncommitted_paths) => {
            let (uncommitted, on_branch): (Vec<_>, Vec<_>) = files
                .iter()
                .partition(|file| uncommitted_paths.contains(&file.path));
            vec![
                (Some(ChangeGroup::Uncommitted), uncommitted),
                (Some(ChangeGroup::OnBranch), on_branch),
            ]
        }
        None => vec![(None, files.iter().collect())],
    };
    let group_by_folder = files.len() > CHANGE_FOLDER_THRESHOLD;

    let mut entries = Vec::new();
    for (group, group_files) in groups {
        if group_files.is_empty() {
            continue;
        }
        if let Some(group) = group {
            entries.push(ChangeListEntry::Group {
                group,
                count: group_files.len(),
            });
        }
        if !group_by_folder {
            entries.extend(group_files.into_iter().map(|file| ChangeListEntry::File {
                file,
                in_folder: false,
            }));
            continue;
        }
        let mut folders: BTreeMap<&'a str, Vec<&'a ChangedFile>> = BTreeMap::new();
        for file in group_files {
            folders
                .entry(split_change_path(&file.path).0)
                .or_default()
                .push(file);
        }
        for (directory, folder_files) in folders {
            if directory.is_empty() {
                entries.extend(folder_files.into_iter().map(|file| ChangeListEntry::File {
                    file,
                    in_folder: false,
                }));
                continue;
            }
            let key = format!("{}:{directory}", group.map_or("", ChangeGroup::label));
            let collapsed = collapsed_folders.contains(&key);
            entries.push(ChangeListEntry::Folder {
                key,
                directory,
                count: folder_files.len(),
                collapsed,
            });
            if !collapsed {
                entries.extend(folder_files.into_iter().map(|file| ChangeListEntry::File {
                    file,
                    in_folder: true,
                }));
            }
        }
    }
    entries
}

/// Marker for PR state-change toasts.
struct MnemonicPrToast;

/// Marker for port action toasts.
struct MnemonicPortToast;

/// Marker for checkpoint toasts.
struct MnemonicCheckpointToast;

/// How many files a restore confirmation names before summarising the rest.
const RESTORE_PROMPT_FILE_LIMIT: usize = 8;

/// Characters of a review comment shown in its one-line preview.
const COMMENT_PREVIEW_CHARS: usize = 140;
/// Characters of each review comment body sent to the agent.
const COMMENT_PROMPT_BODY_CHARS: usize = 4000;

pub struct WorkspaceDetailPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    summary: Option<WorkspaceSummary>,
    tab: DetailTab,
    pr_status: Option<WorkspacePrStatus>,
    pr_busy: bool,
    poll_scheduled: bool,
    /// While set and in the future, poll at the fast cadence.
    poll_fast_until: Option<Instant>,
    /// Feedback line for the last dispatched instruction (sent / failed).
    dispatch_note: Option<SharedString>,
    /// Zed's full project panel, embedded as the "All files" tab so file
    /// browsing, right-click operations, and editing live in one panel.
    file_tree: Option<Entity<project_panel::ProjectPanel>>,
    ports: Vec<WorkspacePort>,
    ports_error: Option<SharedString>,
    ports_busy: bool,
    ports_poll_scheduled: bool,
    checkpoints: Vec<WorkspaceCheckpoint>,
    checkpoints_error: Option<SharedString>,
    checkpoints_loading: bool,
    /// Replacing it cancels a list still in flight, so an older result never
    /// overwrites a newer one.
    checkpoints_task: Option<Task<()>>,
    checkpoint_restoring: bool,
    merging: bool,
    /// Unresolved review threads on the open PR, one entry per thread.
    review_threads: Vec<WorkspacePrComment>,
    review_threads_error: Option<SharedString>,
    review_threads_loading: bool,
    /// Replacing it cancels a fetch still in flight.
    review_threads_task: Option<Task<()>>,
    resolving_thread: Option<String>,
    /// Changed files marked viewed whose content is unchanged since the mark.
    viewed_files: HashSet<String>,
    /// Bumped on every viewed toggle, so a load started before the toggle
    /// cannot overwrite it.
    viewed_generation: u64,
    viewed_writes_pending: usize,
    /// Each write awaits the previous one so quick toggles land in click order.
    viewed_write_task: Option<Task<()>>,
    /// Changed file -> other workspaces of the same repository changing it.
    file_overlaps: BTreeMap<String, Vec<String>>,
    /// Workspace and (path, additions, deletions) of the last annotated list.
    change_annotations_key: Option<(String, Vec<(String, u32, u32)>)>,
    change_annotations_checked_at: Option<Instant>,
    change_annotations_task: Option<Task<()>>,
    /// False until this workspace's viewed marks load, so rows don't flash
    /// as unviewed.
    viewed_loaded: bool,
    /// None until loaded, so files are never shown in the wrong group.
    change_scope: Option<WorkspaceChangeScope>,
    changes_focus_handle: FocusHandle,
    changes_scroll_handle: ScrollHandle,
    /// Highlighted row, by path so it survives list refreshes.
    selected_change: Option<String>,
    collapsed_change_folders: HashSet<String>,
    /// Shows a merged workspace's file list instead of the merged summary.
    show_merged_changes: bool,
}

impl WorkspaceDetailPanel {
    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_handle = workspace.weak_handle();
        let panel = cx.new(|cx| {
            cx.observe_global::<ActiveWorkspace>(|this: &mut Self, cx| {
                let previous = this.summary.as_ref().map(|summary| summary.id.clone());
                this.refresh(cx);
                let current = this.summary.as_ref().map(|summary| summary.id.clone());
                if previous != current {
                    this.pr_status = None;
                    this.refresh_pr_status(cx);
                    this.ports.clear();
                    this.ports_error = None;
                    if this.tab == DetailTab::Ports {
                        this.refresh_ports(true, cx);
                    }
                    this.checkpoints.clear();
                    this.checkpoints_error = None;
                    if this.tab == DetailTab::Checkpoints {
                        this.refresh_checkpoints(cx);
                    }
                    this.clear_review_threads();
                    this.viewed_files.clear();
                    this.file_overlaps.clear();
                    this.change_annotations_key = None;
                    this.change_annotations_checked_at = None;
                    this.change_annotations_task = None;
                    this.viewed_loaded = false;
                    this.change_scope = None;
                    this.selected_change = None;
                    this.collapsed_change_folders.clear();
                    this.show_merged_changes = false;
                }
                cx.notify();
            })
            .detach();

            let mut panel = Self {
                focus_handle: cx.focus_handle(),
                workspace: workspace_handle,
                summary: None,
                tab: DetailTab::Changes,
                pr_status: None,
                pr_busy: false,
                poll_scheduled: false,
                poll_fast_until: None,
                dispatch_note: None,
                file_tree: None,
                ports: Vec::new(),
                ports_error: None,
                ports_busy: false,
                ports_poll_scheduled: false,
                checkpoints: Vec::new(),
                checkpoints_error: None,
                checkpoints_loading: false,
                checkpoints_task: None,
                checkpoint_restoring: false,
                merging: false,
                review_threads: Vec::new(),
                review_threads_error: None,
                review_threads_loading: false,
                review_threads_task: None,
                resolving_thread: None,
                viewed_files: HashSet::new(),
                viewed_generation: 0,
                viewed_writes_pending: 0,
                viewed_write_task: None,
                file_overlaps: BTreeMap::new(),
                change_annotations_key: None,
                change_annotations_checked_at: None,
                change_annotations_task: None,
                viewed_loaded: false,
                change_scope: None,
                changes_focus_handle: cx.focus_handle(),
                changes_scroll_handle: ScrollHandle::new(),
                selected_change: None,
                collapsed_change_folders: HashSet::new(),
                show_merged_changes: false,
            };
            panel.refresh(cx);
            panel
        });

        // Picks up the selected workspace's refreshed changed files and state
        // from the database; rendering itself never reads it.
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

        // Load the embedded file tree asynchronously; ProjectPanel only
        // exposes an async constructor.
        let weak_workspace = workspace.weak_handle();
        let weak_panel = panel.downgrade();
        cx.spawn_in(window, async move |_, cx| {
            match project_panel::ProjectPanel::load(weak_workspace, cx.clone()).await {
                Ok(file_tree) => {
                    weak_panel
                        .update(cx, |panel, cx| {
                            panel.file_tree = Some(file_tree);
                            cx.notify();
                        })
                        .ok();
                }
                Err(err) => log::error!("Failed to load embedded file tree: {err}"),
            }
        })
        .detach();

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
        let selected_id = cx
            .try_global::<ActiveWorkspace>()
            .and_then(|active| active.0.clone());
        let Some(selected_id) = selected_id else {
            self.summary = None;
            return;
        };

        let state = mnemonic_bridge::mnemonic_state(cx);
        match workspace_service::list_workspaces(&state) {
            Ok(workspaces) => {
                self.summary = workspaces.into_iter().find(|ws| ws.id == selected_id);
            }
            Err(e) => log::error!("Failed to list workspaces: {e}"),
        }
    }

    /// Fetches PR + checks state from GitHub (via gh) off the UI thread.
    fn refresh_pr_status(&mut self, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.summary.as_ref().map(|summary| summary.id.clone()) else {
            return;
        };
        let state = mnemonic_bridge::mnemonic_state(cx);
        self.pr_busy = true;
        cx.notify();

        let workspace_id_for_record = workspace_id.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    pr_draft_service::get_workspace_pr_status(&state, &workspace_id)
                })
                .await;
            this.update(cx, |this, cx| {
                this.pr_busy = false;
                match result {
                    Ok(status) => {
                        if mnemonic_bridge::record_pr_status(&workspace_id_for_record, &status) {
                            cx.refresh_windows();
                        }
                        // A status fetched for a workspace the user has since
                        // switched away from must not drive this panel's Merge.
                        if this.current_workspace_id() == Some(workspace_id_for_record.as_str()) {
                            if let Some(message) =
                                Self::transition_message(this.pr_status.as_ref(), &status)
                            {
                                this.show_pr_toast(message, cx);
                            }
                            let open_pr_url = status.url.clone().filter(|_| {
                                status.found && status.state.as_deref() == Some("OPEN")
                            });
                            this.pr_status = Some(status);
                            match open_pr_url {
                                Some(pr_url) => this.refresh_review_threads(pr_url, cx),
                                None => this.clear_review_threads(),
                            }
                        }
                    }
                    Err(err) => log::error!("Failed to fetch PR status: {err}"),
                }
                this.schedule_next_poll(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn current_workspace_id(&self) -> Option<&str> {
        self.summary.as_ref().map(|summary| summary.id.as_str())
    }

    fn clear_review_threads(&mut self) {
        self.review_threads.clear();
        self.review_threads_error = None;
        self.review_threads_loading = false;
        self.review_threads_task = None;
        self.resolving_thread = None;
    }

    /// Fetches the PR's unresolved review threads from GitHub (via gh).
    fn refresh_review_threads(&mut self, pr_url: String, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.current_workspace_id().map(str::to_string) else {
            return;
        };
        let state = mnemonic_bridge::mnemonic_state(cx);
        self.review_threads_loading = true;
        cx.notify();
        self.review_threads_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let workspace_id = workspace_id.clone();
                    async move {
                        review_cockpit_service::list_unresolved_review_threads(
                            &state,
                            &workspace_id,
                            &pr_url,
                        )
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.review_threads_loading = false;
                if this.current_workspace_id() != Some(workspace_id.as_str()) {
                    return;
                }
                match result {
                    Ok(threads) => {
                        this.review_threads = threads;
                        this.review_threads_error = None;
                    }
                    Err(err) => {
                        log::error!("Failed to fetch PR review threads: {err}");
                        this.review_threads_error = Some(SharedString::from(err));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn can_merge(status: &WorkspacePrStatus) -> bool {
        status.found
            && status.state.as_deref() == Some("OPEN")
            && !status.is_draft
            && status.merge_state_status.as_deref() == Some("CLEAN")
            && status.number.is_some()
            && status.head_ref_oid.is_some()
    }

    /// Confirms, then merges the PR pinned to the head commit the panel last
    /// saw, so a push after the user looked makes GitHub refuse the merge.
    fn merge_pr(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.merging {
            return;
        }
        let Some(workspace_id) = self.current_workspace_id().map(str::to_string) else {
            return;
        };
        let Some(status) = self
            .pr_status
            .as_ref()
            .filter(|status| status.workspace_id == workspace_id && Self::can_merge(status))
        else {
            return;
        };
        let (Some(number), Some(head_ref_oid)) = (status.number, status.head_ref_oid.clone())
        else {
            return;
        };
        let title = status.title.clone().unwrap_or_default();
        let pr_url = status.url.clone();
        let state = mnemonic_bridge::mnemonic_state(cx);
        let method = pr_draft_service::configured_merge_method(&state);
        let short_head = head_ref_oid.chars().take(7).collect::<String>();
        let detail = format!(
            "{title}\n\nMethod: {method}\nHead commit: {short_head}\n\nGitHub refuses the merge if \
             commits were pushed after this status was fetched. The branch is kept because this \
             workspace still uses it."
        );
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("Merge PR #{number}?"),
            Some(&detail),
            &["Merge", "Cancel"],
            cx,
        );
        self.merging = true;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                this.update(cx, |this, cx| {
                    this.merging = false;
                    cx.notify();
                })
                .ok();
                return;
            }
            let result = cx
                .background_spawn(async move {
                    pr_draft_service::merge_workspace_pr(
                        &state,
                        &workspace_id,
                        number,
                        pr_url.as_deref(),
                        &head_ref_oid,
                        &method,
                    )
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.merging = false;
                match result {
                    Ok(message) => this.show_pr_toast(message, cx),
                    Err(err) => {
                        log::error!("Failed to merge PR #{number}: {err}");
                        let _dismissed = window.prompt(
                            PromptLevel::Critical,
                            &format!("Couldn't merge PR #{number}"),
                            Some(&err),
                            &["OK"],
                            cx,
                        );
                    }
                }
                this.refresh_pr_status(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn comment_location(comment: &WorkspacePrComment) -> String {
        match (comment.path.as_deref(), comment.line) {
            (Some(path), Some(line)) => format!("{path}:{line}"),
            (Some(path), None) => path.to_string(),
            (None, _) => "PR conversation".to_string(),
        }
    }

    fn comment_preview(body: &str) -> String {
        let first_line = body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("");
        if first_line.chars().count() <= COMMENT_PREVIEW_CHARS {
            first_line.to_string()
        } else {
            let mut preview = first_line
                .chars()
                .take(COMMENT_PREVIEW_CHARS)
                .collect::<String>();
            preview.push_str("...");
            preview
        }
    }

    fn review_comments_prompt(number: Option<i64>, comments: &[&WorkspacePrComment]) -> String {
        let pr = number
            .map(|number| format!("PR #{number}"))
            .unwrap_or_else(|| "this PR".to_string());
        let mut prompt = format!(
            "Address these unresolved review comments on {pr}. For each one, make the change if \
             it is right, or tell me why it should not be changed. Then commit and push."
        );
        for (index, comment) in comments.iter().enumerate() {
            let mut body = comment.body.trim().to_string();
            if body.chars().count() > COMMENT_PROMPT_BODY_CHARS {
                body = body.chars().take(COMMENT_PROMPT_BODY_CHARS).collect();
                body.push_str("\n...[truncated]");
            }
            let thread = comment
                .thread_id
                .as_deref()
                .map(|thread_id| format!(", thread {thread_id}"))
                .unwrap_or_default();
            let outdated = if comment.thread_outdated {
                ", outdated: the code has changed since"
            } else {
                ""
            };
            prompt.push_str(&format!(
                "\n\n{}. {} (by {}{thread}{outdated})\n{body}",
                index + 1,
                Self::comment_location(comment),
                comment.author,
            ));
        }
        prompt
    }

    /// Sends the given review threads (all when `thread_id` is None) to the
    /// workspace agent through the app dispatch path.
    fn send_review_comments_to_agent(&mut self, thread_id: Option<&str>, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.current_workspace_id().map(str::to_string) else {
            return;
        };
        let comments = self
            .review_threads
            .iter()
            .filter(|comment| thread_id.is_none() || comment.thread_id.as_deref() == thread_id)
            .collect::<Vec<_>>();
        if comments.is_empty() {
            return;
        }
        let number = self.pr_status.as_ref().and_then(|status| status.number);
        let prompt = Self::review_comments_prompt(number, &comments);
        let label = if comments.len() == 1 {
            "review comment".to_string()
        } else {
            format!("{} review comments", comments.len())
        };
        self.dispatch_instruction(&workspace_id, prompt, &label, cx);
    }

    fn resolve_review_thread(
        &mut self,
        thread_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.resolving_thread.is_some() {
            return;
        }
        let Some(workspace_id) = self.current_workspace_id().map(str::to_string) else {
            return;
        };
        let Some(pr_url) = self
            .pr_status
            .as_ref()
            .and_then(|status| status.url.clone())
        else {
            return;
        };
        let state = mnemonic_bridge::mnemonic_state(cx);
        self.resolving_thread = Some(thread_id.clone());
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn({
                    let workspace_id = workspace_id.clone();
                    let thread_id = thread_id.clone();
                    async move {
                        review_cockpit_service::resolve_review_thread(
                            &state,
                            &workspace_id,
                            &thread_id,
                        )
                    }
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.resolving_thread = None;
                if this.current_workspace_id() != Some(workspace_id.as_str()) {
                    return;
                }
                match result {
                    Ok(()) => this
                        .review_threads
                        .retain(|comment| comment.thread_id.as_deref() != Some(thread_id.as_str())),
                    Err(err) => {
                        log::error!("Failed to resolve review thread {thread_id}: {err}");
                        let _dismissed = window.prompt(
                            PromptLevel::Critical,
                            "Couldn't resolve the review thread",
                            Some(&err),
                            &["OK"],
                            cx,
                        );
                    }
                }
                this.refresh_review_threads(pr_url, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn failing_count(status: &WorkspacePrStatus) -> usize {
        status
            .checks
            .iter()
            .filter(|check| {
                matches!(
                    check.conclusion.as_deref(),
                    Some("FAILURE") | Some("TIMED_OUT") | Some("CANCELLED") | Some("ACTION_REQUIRED")
                )
            })
            .count()
    }

    fn is_green(status: &WorkspacePrStatus) -> bool {
        status.found
            && !status.checks.is_empty()
            && status
                .checks
                .iter()
                .all(|check| check.status.eq_ignore_ascii_case("completed"))
            && Self::failing_count(status) == 0
    }

    /// A notification-worthy state change between two GitHub polls.
    fn transition_message(
        old: Option<&WorkspacePrStatus>,
        new: &WorkspacePrStatus,
    ) -> Option<String> {
        let old_found = old.is_some_and(|old| old.found);
        if new.found && !old_found {
            return Some(match new.number {
                Some(number) => format!("PR #{number} created"),
                None => "PR created".to_string(),
            });
        }
        if !new.found {
            return None;
        }

        let old_failing = old.map(Self::failing_count).unwrap_or(0);
        let new_failing = Self::failing_count(new);
        if new_failing > old_failing {
            let plural = if new_failing == 1 { "" } else { "s" };
            return Some(format!("{new_failing} CI check{plural} failing"));
        }

        let was_green = old.is_some_and(Self::is_green);
        if Self::is_green(new) && !was_green {
            return Some("All CI checks passed ✓".to_string());
        }
        None
    }

    fn show_pr_toast(&self, message: String, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<MnemonicPrToast>(), message),
                    cx,
                );
            });
        }
    }

    /// Keeps GitHub state fresh while a workspace is selected.
    fn schedule_next_poll(&mut self, cx: &mut Context<Self>) {
        if self.poll_scheduled || self.summary.is_none() {
            return;
        }
        self.poll_scheduled = true;
        let interval = if self
            .poll_fast_until
            .is_some_and(|until| Instant::now() < until)
        {
            PR_POLL_FAST_INTERVAL
        } else {
            PR_POLL_INTERVAL
        };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(interval).await;
            this.update(cx, |this, cx| {
                this.poll_scheduled = false;
                if this.summary.is_some() && !this.pr_busy {
                    this.refresh_pr_status(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn rerun_failed_checks(&mut self, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.summary.as_ref().map(|summary| summary.id.clone()) else {
            return;
        };
        let state = mnemonic_bridge::mnemonic_state(cx);
        self.pr_busy = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    pr_draft_service::rerun_failed_workspace_checks(&state, &workspace_id)
                })
                .await;
            this.update(cx, |this, cx| {
                this.pr_busy = false;
                match result {
                    Ok(message) => log::info!("Re-run requested: {message}"),
                    Err(err) => log::error!("Failed to re-run checks: {err}"),
                }
                this.refresh_pr_status(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Asks the workspace's claude to create the PR, visibly in its
    /// terminal: commit, push the branch, open the PR with gh.
    pub fn create_pr(&mut self, cx: &mut Context<Self>) {
        let Some(summary) = &self.summary else {
            return;
        };

        let prompt = format!(
            "Create a pull request for this workspace. Review what changed, commit any \
             uncommitted work with a sensible message, push the branch ({branch}) to origin, \
             and open a PR against the default branch using `gh pr create` with a clear title \
             and a summary body describing the changes. Report the PR URL when done.",
            branch = summary.branch,
        );

        let workspace_id = summary.id.clone();
        self.dispatch_instruction(&workspace_id, prompt, "PR instruction", cx);
    }

    /// Routes an instruction to the workspace's agent terminal, surfaces the
    /// outcome in the panel, and boosts GitHub polling to catch the result.
    fn dispatch_instruction(
        &mut self,
        workspace_id: &str,
        prompt: String,
        label: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let workspace_id = workspace_id.to_string();
        let label = label.to_string();
        let this_handle = cx.weak_entity();
        // Defer the workspace update to avoid a double-lease panic when this
        // method is called from within a workspace update (e.g. via an action
        // handler registered on Workspace).
        cx.defer(move |cx| {
            let sent = workspace.update(cx, |workspace, cx| {
                WorkspaceHost::dispatch_to_workspace_agent(workspace, &workspace_id, prompt, cx)
            });
            let _ = this_handle.update(cx, |this, cx| {
                this.dispatch_note = Some(if sent {
                    this.poll_fast_until = Some(Instant::now() + PR_POLL_BOOST);
                    this.schedule_next_poll(cx);
                    SharedString::from(format!("Sent {label} to claude ✓"))
                } else {
                    SharedString::from(format!(
                        "Couldn't send {label}: open the workspace first (click its worktree)"
                    ))
                });
                cx.notify();
            });
        });
    }

    /// Asks the workspace's agent to merge the latest base branch in. The
    /// agent does it so that conflicts and uncommitted work are handled where
    /// the user can see them.
    fn update_from_base(&mut self, cx: &mut Context<Self>) {
        let Some(summary) = &self.summary else {
            return;
        };
        let Some(base_ref) = self
            .change_scope
            .as_ref()
            .and_then(|scope| scope.base_ref.clone())
        else {
            return;
        };
        let workspace_id = summary.id.clone();
        let prompt = format!(
            "Bring this branch up to date with `{base_ref}`: run `git fetch origin`, then merge \
             `{base_ref}` into the current branch. If uncommitted changes block the merge, \
             commit them first. If there are conflicts, resolve them keeping the intent of both \
             sides and commit the merge. Do not push."
        );
        self.dispatch_instruction(&workspace_id, prompt, "update from base", cx);
    }

    /// Sends a one-shot "fix the failing CI" instruction into the
    /// workspace's claude terminal, so the fix is visible as it happens.
    fn fix_errors_via_agent(&mut self, cx: &mut Context<Self>) {
        let Some(summary) = &self.summary else {
            return;
        };
        let Some(status) = &self.pr_status else {
            return;
        };

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

        let mut problems = Vec::new();
        if conflicts {
            problems.push("the PR has merge conflicts with the base branch".to_string());
        }
        if !failing.is_empty() {
            problems.push(format!("these CI checks failed: {}", failing.join(", ")));
        }
        if problems.is_empty() {
            return;
        }

        let prompt = format!(
            "CI status on this PR needs fixing: {}. Use `gh pr checks` and \
             `gh run view --log-failed` to inspect the failures (and resolve any \
             merge conflicts by merging the base branch), fix the problems, \
             commit, and push.",
            problems.join("; "),
        );

        let workspace_id = summary.id.clone();
        self.dispatch_instruction(&workspace_id, prompt, "fix instruction", cx);
    }

    fn worktree_root(&self) -> Option<PathBuf> {
        let summary = self.summary.as_ref()?;
        summary
            .workspace_root_path
            .as_deref()
            .or(summary.repository_path.as_deref())
            .or(summary.selected_worktree_path.as_deref())
            .map(PathBuf::from)
            .filter(|path| path.exists())
    }

    fn open_changed_file(
        &mut self,
        relative_path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.worktree_root() else {
            return;
        };
        let absolute_path = root.join(relative_path);
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(absolute_path, OpenOptions::default(), window, cx)
                    .detach_and_log_err(cx);
            });
        }
    }

    fn status_color(&self, status: &str, cx: &Context<Self>) -> gpui::Hsla {
        match status {
            "Active" => cx.theme().status().info,
            "Merged" => cx.theme().status().success,
            "Paused" | "Idle" => cx.theme().status().warning,
            "Failed" | "Error" => cx.theme().status().error,
            _ => cx.theme().colors().text_muted,
        }
    }

    /// One look at the PR derived from GitHub state: label + button tint.
    fn pr_button_appearance(status: &WorkspacePrStatus) -> (String, TintColor) {
        let number = status
            .number
            .map(|number| format!("#{number}"))
            .unwrap_or_default();

        if status.state.as_deref() == Some("MERGED") {
            return (format!("{number} Merged"), TintColor::Success);
        }
        if status.mergeable.as_deref() == Some("CONFLICTING")
            || status.merge_state_status.as_deref() == Some("DIRTY")
        {
            return (format!("{number} Conflicts"), TintColor::Error);
        }
        let any_check_failed = status.checks.iter().any(|check| {
            matches!(
                check.conclusion.as_deref(),
                Some("FAILURE") | Some("TIMED_OUT") | Some("CANCELLED")
            )
        });
        if any_check_failed {
            return (format!("{number} Checks failed"), TintColor::Error);
        }
        if status.merge_state_status.as_deref() == Some("CLEAN") {
            return (format!("{number} Ready to merge"), TintColor::Success);
        }
        (format!("{number} Open"), TintColor::Warning)
    }

    fn render_pr_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.pr_busy;
        match self.pr_status.as_ref().filter(|status| status.found) {
            Some(status) if status.state.as_deref() == Some("MERGED") => {
                let (label, _) = Self::pr_button_appearance(status);
                let url = status.url.clone();
                let color = mnemonic_bridge::pr_state_color("merged", cx)
                    .unwrap_or(cx.theme().status().info);
                h_flex()
                    .id("pr-state-merged")
                    .h(px(22.))
                    .px_2()
                    .gap_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(color.opacity(0.25))
                    .border_1()
                    .border_color(color.opacity(0.6))
                    .child(
                        Icon::new(IconName::PullRequest)
                            .size(IconSize::XSmall)
                            .color(Color::Custom(color)),
                    )
                    .child(
                        Label::new(label)
                            .size(LabelSize::Small)
                            .color(Color::Custom(color)),
                    )
                    .on_click(move |_, _, cx| {
                        if let Some(url) = url.as_deref() {
                            cx.open_url(url);
                        }
                    })
                    .into_any_element()
            }
            Some(status) => {
                let (label, tint) = Self::pr_button_appearance(status);
                let url = status.url.clone();
                Button::new("pr-state", label)
                    .style(ButtonStyle::Tinted(tint))
                    .label_size(LabelSize::Small)
                    .disabled(busy)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if let Some(url) = url.as_deref() {
                            cx.open_url(url);
                        }
                    }))
                    .into_any_element()
            }
            None => {
                let keep_ci_green = WorkspacesSettings::get_global(cx).keep_ci_green;
                Button::new("create-pr", if busy { "Working…" } else { "Create PR" })
                    .style(ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .disabled(busy || self.summary.is_none())
                    .when(keep_ci_green, |button| {
                        button.tooltip(Tooltip::text(
                            "Create PR, then watch CI and have the agent fix failures until \
                             green. Turn this off in Settings > Workspaces",
                        ))
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.create_pr_per_settings(cx);
                    }))
                    .into_any_element()
            }
        }
    }

    /// Creates the PR, also starting the CI watch when the `keep_ci_green`
    /// setting is on.
    pub fn create_pr_per_settings(&mut self, cx: &mut Context<Self>) {
        if WorkspacesSettings::get_global(cx).keep_ci_green {
            self.create_pr_with_auto_fix(cx);
        } else {
            self.create_pr(cx);
        }
    }

    /// Create PR and immediately arm the CI auto-fix loop, so the whole
    /// create → watch checks → fix → push cycle runs hands-off.
    pub fn create_pr_with_auto_fix(&mut self, cx: &mut Context<Self>) {
        self.create_pr(cx);
        let Some(workspace_id) = self.summary.as_ref().map(|summary| summary.id.clone()) else {
            return;
        };
        if let Some(workspace) = self.workspace.upgrade() {
            let workspace_id = workspace_id.clone();
            cx.defer(move |cx| {
                workspace.update(cx, |workspace, cx| {
                    WorkspaceHost::enable_auto_fix_for_workspace(workspace, &workspace_id, cx);
                });
            });
        }
    }

    fn render_header(&self, ws: &WorkspaceSummary, cx: &mut Context<Self>) -> impl IntoElement {
        let status_color = self.status_color(&ws.status, cx);
        let pr_color = mnemonic_bridge::pr_state(&ws.id)
            .and_then(|state| mnemonic_bridge::pr_state_color(&state, cx));
        let ahead = ws.branch_health.ahead_by;
        let behind = self
            .change_scope
            .as_ref()
            .map_or(0, |scope| scope.behind_by);

        v_flex()
            .px_2()
            .py_1p5()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .when_some(pr_color, |el, color| {
                el.bg(color.opacity(0.12)).border_color(color.opacity(0.5))
            })
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(
                                div()
                                    .w(px(8.))
                                    .h(px(8.))
                                    .rounded_full()
                                    .bg(status_color),
                            )
                            .child(
                                Label::new(ws.name.clone())
                                    .size(LabelSize::Default)
                                    .single_line(),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(self.render_pr_button(cx))
                            .when(
                                self.pr_status.as_ref().is_some_and(|status| {
                                    status.workspace_id == ws.id && Self::can_merge(status)
                                }),
                                |el| {
                                    el.child(
                                        Button::new(
                                            "merge-pr",
                                            if self.merging { "Merging…" } else { "Merge" },
                                        )
                                        .style(ButtonStyle::Tinted(TintColor::Success))
                                        .label_size(LabelSize::Small)
                                        .disabled(self.merging || self.pr_busy)
                                        .tooltip(Tooltip::text(
                                            "Merge this PR on GitHub (keeps the branch)",
                                        ))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.merge_pr(window, cx);
                                        })),
                                    )
                                },
                            ),
                    ),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(
                        Icon::new(IconName::GitBranch)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(ws.branch.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .single_line(),
                    )
                    .when(ahead > 0 || behind > 0, |el| {
                        el.child(
                            Label::new(format!("↑{ahead} ↓{behind}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .when(behind > 0, |el| {
                        el.child(
                            Button::new("update-from-base", "Update")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::XSmall)
                                .tooltip(Tooltip::text(
                                    "Ask the agent to merge the latest base branch into this one",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.update_from_base(cx);
                                })),
                        )
                    }),
            )
            .when_some(self.dispatch_note.clone(), |el, note| {
                el.child(
                    Label::new(note)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
    }

    fn render_tabs(&self, ws: &WorkspaceSummary, cx: &mut Context<Self>) -> impl IntoElement {
        let changes_count = ws.changed_files.len();
        let review_agent = match WorkspacesSettings::get_global(cx).review_agent {
            ReviewAgent::Codex => "Codex",
            ReviewAgent::Claude => "Claude",
        };
        let tab_button = |id: &'static str,
                          label: String,
                          tab: DetailTab,
                          current: DetailTab,
                          cx: &mut Context<Self>| {
            div()
                .id(id)
                .px_2()
                .py_0p5()
                .rounded_md()
                .cursor_pointer()
                .when(tab == current, |el| {
                    el.bg(cx.theme().colors().ghost_element_selected)
                })
                .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.select_tab(tab, window, cx);
                }))
                .child(
                    Label::new(label)
                        .size(LabelSize::Small)
                        .color(if tab == current {
                            Color::Default
                        } else {
                            Color::Muted
                        }),
                )
        };

        h_flex()
            .px_1p5()
            .h(px(30.))
            .gap_0p5()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(tab_button(
                "detail-tab-all-files",
                "All files".to_string(),
                DetailTab::AllFiles,
                self.tab,
                cx,
            ))
            .child(tab_button(
                "detail-tab-changes",
                format!("Changes {changes_count}"),
                DetailTab::Changes,
                self.tab,
                cx,
            ))
            .child(tab_button(
                "detail-tab-checks",
                "Checks".to_string(),
                DetailTab::Checks,
                self.tab,
                cx,
            ))
            // Ports and Checkpoints are used less often, so they live in the
            // overflow menu and only take a tab while they have something to
            // show or are open.
            .when(!self.ports.is_empty() || self.tab == DetailTab::Ports, |el| {
                el.child(tab_button(
                    "detail-tab-ports",
                    if self.ports.is_empty() {
                        "Ports".to_string()
                    } else {
                        format!("Ports {}", self.ports.len())
                    },
                    DetailTab::Ports,
                    self.tab,
                    cx,
                ))
            })
            .when(self.tab == DetailTab::Checkpoints, |el| {
                el.child(tab_button(
                    "detail-tab-checkpoints",
                    "Checkpoints".to_string(),
                    DetailTab::Checkpoints,
                    self.tab,
                    cx,
                ))
            })
            .child(div().flex_grow(1.0))
            .child(
                Button::new("start-review", "Review")
                    .label_size(LabelSize::Small)
                    .start_icon(
                        Icon::new(IconName::Eye)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text(format!(
                        "Open a {review_agent} session that reviews your diff for bugs, security \
                         and performance issues. Change the agent in Settings > Workspaces",
                    )))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.start_review(window, cx);
                    })),
            )
            .child(
                IconButton::new("refresh-pr-status", IconName::ArrowCircle)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Refresh GitHub status"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.refresh_pr_status(cx);
                    })),
            )
            .child({
                let this = cx.weak_entity();
                PopoverMenu::new("detail-more-tabs")
                    .anchor(Anchor::TopRight)
                    .trigger(
                        IconButton::new("detail-more-tabs-trigger", IconName::Ellipsis)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Ports and checkpoints")),
                    )
                    .menu(move |window, cx| {
                        let this = this.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            [
                                ("Ports", DetailTab::Ports),
                                ("Checkpoints", DetailTab::Checkpoints),
                            ]
                            .into_iter()
                            .fold(menu, |menu, (label, tab)| {
                                let this = this.clone();
                                menu.entry(label, None, move |window, cx| {
                                    this.update(cx, |this, cx| {
                                        this.select_tab(tab, window, cx);
                                    })
                                    .ok();
                                })
                            })
                        }))
                    })
            })
    }

    fn select_tab(&mut self, tab: DetailTab, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = tab;
        if tab == DetailTab::Changes {
            self.changes_focus_handle.focus(window, cx);
        }
        if tab == DetailTab::Checks && self.pr_status.is_none() && !self.pr_busy {
            self.refresh_pr_status(cx);
        }
        if tab == DetailTab::Ports {
            self.refresh_ports(true, cx);
        }
        if tab == DetailTab::Checkpoints {
            self.refresh_checkpoints(cx);
        }
        cx.notify();
    }

    /// Opens the review session for the selected workspace.
    fn start_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.summary.as_ref().map(|summary| summary.id.clone()) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let started = workspace.update(cx, |workspace, cx| {
            WorkspaceHost::start_review_for_workspace(workspace, &workspace_id, window, cx)
        });
        self.dispatch_note = Some(if started {
            SharedString::from("Review session started ✓")
        } else {
            SharedString::from("Couldn't start review: open the workspace first")
        });
        cx.notify();
    }

    fn change_status_label(status: &str) -> (String, Color) {
        let lowered = status.to_lowercase();
        if lowered.starts_with("a") || lowered.starts_with("u") || lowered.starts_with("?") {
            ("A".to_string(), Color::Created)
        } else if lowered.starts_with("d") {
            ("D".to_string(), Color::Deleted)
        } else {
            ("M".to_string(), Color::Modified)
        }
    }

    fn maybe_refresh_change_annotations(&mut self, cx: &mut Context<Self>) {
        if self.tab != DetailTab::Changes {
            return;
        }
        let Some(summary) = self.summary.as_ref() else {
            return;
        };
        let list_unchanged =
            self.change_annotations_key
                .as_ref()
                .is_some_and(|(workspace_id, files)| {
                    workspace_id == &summary.id
                        && files.len() == summary.changed_files.len()
                        && files.iter().zip(&summary.changed_files).all(
                            |((path, additions, deletions), file)| {
                                path == &file.path
                                    && *additions == file.additions
                                    && *deletions == file.deletions
                            },
                        )
                });
        let recently_checked = self
            .change_annotations_checked_at
            .is_some_and(|checked_at| checked_at.elapsed() < CHANGE_ANNOTATIONS_RECHECK_INTERVAL);
        if list_unchanged && recently_checked {
            return;
        }
        self.refresh_change_annotations(cx);
    }

    /// Loads viewed marks and cross-workspace overlaps off the UI thread.
    fn refresh_change_annotations(&mut self, cx: &mut Context<Self>) {
        let Some(summary) = self.summary.as_ref() else {
            return;
        };
        let workspace_id = summary.id.clone();
        self.change_annotations_key = Some((
            workspace_id.clone(),
            summary
                .changed_files
                .iter()
                .map(|file| (file.path.clone(), file.additions, file.deletions))
                .collect(),
        ));
        self.change_annotations_checked_at = Some(Instant::now());
        // The last pending write triggers a refresh once it lands.
        if self.viewed_writes_pending > 0 {
            return;
        }
        let changed_paths = summary
            .changed_files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        let generation = self.viewed_generation;
        let state = mnemonic_bridge::mnemonic_state(cx);
        self.change_annotations_task = Some(cx.spawn(async move |this, cx| {
            let (viewed, overlaps, scope) = cx
                .background_spawn({
                    let workspace_id = workspace_id.clone();
                    async move {
                        (
                            review_cockpit_service::list_workspace_viewed_files(
                                &state,
                                &workspace_id,
                                &changed_paths,
                            ),
                            conflict_detection_service::files_also_changed_elsewhere(
                                &state.db,
                                &workspace_id,
                            ),
                            git_review_service::get_workspace_change_scope(&state, &workspace_id),
                        )
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.current_workspace_id() != Some(workspace_id.as_str()) {
                    return;
                }
                match overlaps {
                    Ok(overlaps) => this.file_overlaps = overlaps,
                    Err(err) => {
                        log::error!("Failed to find files changed in other workspaces: {err}")
                    }
                }
                match scope {
                    Ok(scope) => this.change_scope = Some(scope),
                    Err(err) => log::error!("Failed to read uncommitted changes: {err}"),
                }
                if generation == this.viewed_generation && this.viewed_writes_pending == 0 {
                    match viewed {
                        Ok(viewed) => {
                            this.viewed_files = viewed;
                            this.viewed_loaded = true;
                        }
                        Err(err) => log::error!("Failed to load viewed files: {err}"),
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Records that the file's current content was opened in the diff. The
    /// mark clears itself once the file's content changes.
    fn mark_file_viewed(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.current_workspace_id().map(str::to_string) else {
            return;
        };
        if !self.viewed_files.insert(path.clone()) {
            return;
        }
        self.viewed_generation += 1;
        self.viewed_writes_pending += 1;
        let state = mnemonic_bridge::mnemonic_state(cx);
        let previous_write = self.viewed_write_task.take();
        self.viewed_write_task = Some(cx.spawn(async move |this, cx| {
            if let Some(previous_write) = previous_write {
                previous_write.await;
            }
            let result = cx
                .background_spawn(async move {
                    review_cockpit_service::set_workspace_file_viewed(
                        &state,
                        &workspace_id,
                        &path,
                        true,
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                this.viewed_writes_pending = this.viewed_writes_pending.saturating_sub(1);
                if let Err(err) = result {
                    log::error!("Failed to update viewed mark: {err}");
                }
                if this.viewed_writes_pending == 0 {
                    this.refresh_change_annotations(cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// The project repository containing the workspace root, innermost first
    /// so a nested repository wins over its parent.
    fn repository_for_root(
        project: &Entity<Project>,
        root: &Path,
        cx: &App,
    ) -> Option<Entity<Repository>> {
        project
            .read(cx)
            .repositories(cx)
            .values()
            .filter(|repository| root.starts_with(&repository.read(cx).work_directory_abs_path))
            .max_by_key(|repository| {
                repository
                    .read(cx)
                    .work_directory_abs_path
                    .components()
                    .count()
            })
            .cloned()
    }

    /// Opens (or reuses) the branch diff against the workspace's base branch,
    /// scrolled to `relative_path` when given. False when it cannot be opened.
    fn open_branch_diff(
        &self,
        relative_path: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(base_ref) = self
            .change_scope
            .as_ref()
            .and_then(|scope| scope.base_ref.clone())
        else {
            return false;
        };
        let Some(root) = self.worktree_root() else {
            return false;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };
        workspace.update(cx, |workspace, cx| {
            let project = workspace.project().clone();
            let Some(repository) = Self::repository_for_root(&project, &root, cx) else {
                return false;
            };
            let repo_path = relative_path.and_then(|relative_path| {
                repository
                    .read(cx)
                    .abs_path_to_repo_path(&root.join(relative_path))
            });
            ProjectDiff::deploy_branch_diff_with_base_ref(
                workspace,
                project,
                repository,
                SharedString::from(base_ref),
                repo_path,
                window,
                cx,
            );
            true
        })
    }

    fn open_change(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_change = Some(path.clone());
        if !self.open_branch_diff(Some(&path), window, cx) {
            self.open_changed_file(&path, window, cx);
        }
        self.mark_file_viewed(path, cx);
        self.changes_focus_handle.focus(window, cx);
        cx.notify();
    }

    fn review_all_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open_branch_diff(None, window, cx) {
            window.dispatch_action(BranchDiff.boxed_clone(), cx);
        }
    }

    fn archive_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(summary) = self.summary.as_ref() else {
            return;
        };
        let (workspace_id, workspace_name) = (summary.id.clone(), summary.name.clone());
        let Some(panel) = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<WorkspacePanel>(cx))
        else {
            return;
        };
        panel.update(cx, |panel, cx| {
            panel.archive_workspace(workspace_id, workspace_name, window, cx);
        });
    }

    fn is_merged(&self, ws: &WorkspaceSummary) -> bool {
        self.pr_status.as_ref().is_some_and(|status| {
            status.workspace_id == ws.id
                && status.found
                && status.state.as_deref() == Some("MERGED")
        })
    }

    fn change_entries<'a>(&self, ws: &'a WorkspaceSummary) -> Vec<ChangeListEntry<'a>> {
        change_list_entries(
            &ws.changed_files,
            self.change_scope
                .as_ref()
                .map(|scope| &scope.uncommitted_paths),
            &self.collapsed_change_folders,
        )
    }

    fn toggle_change_folder(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.collapsed_change_folders.remove(&key) {
            self.collapsed_change_folders.insert(key);
        }
        cx.notify();
    }

    fn move_change_selection(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(summary) = self.summary.as_ref() else {
            return;
        };
        let file_rows = self
            .change_entries(summary)
            .into_iter()
            .enumerate()
            .filter_map(|(entry_index, entry)| match entry {
                ChangeListEntry::File { file, .. } => Some((entry_index, file.path.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        let last = file_rows.len().saturating_sub(1);
        let current = self
            .selected_change
            .as_deref()
            .and_then(|selected| file_rows.iter().position(|(_, path)| path == selected));
        let next = match (current, forward) {
            (Some(position), true) => (position + 1).min(last),
            (Some(position), false) => position.saturating_sub(1),
            (None, true) => 0,
            (None, false) => last,
        };
        if let Some((entry_index, path)) = file_rows.into_iter().nth(next) {
            self.selected_change = Some(path);
            self.changes_scroll_handle.scroll_to_item(entry_index);
            cx.notify();
        }
    }

    fn select_next_change(
        &mut self,
        _: &menu::SelectNext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_change_selection(true, cx);
    }

    fn select_previous_change(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_change_selection(false, cx);
    }

    fn confirm_change(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.selected_change.clone() {
            self.open_change(path, window, cx);
        }
    }

    fn render_merged_changes(&self, ws: &WorkspaceSummary, cx: &mut Context<Self>) -> AnyElement {
        let base_branch = ws.branch_health.base_branch.trim();
        let base_branch = if base_branch.is_empty() {
            "the base branch"
        } else {
            base_branch
        };
        v_flex()
            .w_full()
            .px_2()
            .py_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(
                        Icon::new(IconName::Check)
                            .size(IconSize::Small)
                            .color(Color::Success),
                    )
                    .child(Label::new(format!("Merged into {base_branch}")).size(LabelSize::Small)),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("archive-merged-workspace", "Archive workspace")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.archive_workspace(window, cx);
                            })),
                    )
                    .child(
                        Button::new("show-merged-changes", "Show changes")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_merged_changes = true;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_change_file(
        &self,
        index: usize,
        file: &ChangedFile,
        in_folder: bool,
        list_focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (directory, file_name) = split_change_path(&file.path);
        let (status_label, status_label_color) = Self::change_status_label(&file.status);
        let deleted = status_label == "D";
        let selected = self.selected_change.as_deref() == Some(file.path.as_str());
        let unviewed = self.viewed_loaded && !self.viewed_files.contains(&file.path);
        let overlap_tooltip = self
            .file_overlaps
            .get(&file.path)
            .map(|names| format!("Also changed in {}", names.join(", ")));
        let path_for_open = file.path.clone();
        let colors = cx.theme().colors();

        h_flex()
            .id(("change-file", index))
            .h(px(22.))
            .w_full()
            .pl(if in_folder { px(20.) } else { px(6.) })
            .pr_2()
            .gap_1p5()
            .items_center()
            .cursor_pointer()
            .border_1()
            .when(selected, |el| el.bg(cx.theme().status().info.alpha(0.08)))
            .when(selected && list_focused, |el| {
                el.border_color(colors.panel_focused_border)
            })
            .hover(|style| style.bg(colors.ghost_element_hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_change(path_for_open.clone(), window, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .size(px(6.))
                    .rounded_full()
                    .when(unviewed, |el| el.bg(colors.text_accent)),
            )
            .child(
                div().flex_none().w(px(10.)).child(
                    Label::new(status_label)
                        .size(LabelSize::XSmall)
                        .color(status_label_color),
                ),
            )
            .child(
                h_flex()
                    .id(("change-file-name", index))
                    .min_w_0()
                    .flex_1()
                    .gap_1p5()
                    .overflow_hidden()
                    .tooltip(Tooltip::text(file.path.clone()))
                    .child(
                        div().flex_none().child(
                            Label::new(file_name.to_string())
                                .size(LabelSize::Small)
                                .color(if deleted {
                                    Color::Muted
                                } else {
                                    Color::Default
                                })
                                .when(deleted, |label| label.strikethrough())
                                .single_line(),
                        ),
                    )
                    .when(!in_folder && !directory.is_empty(), |el| {
                        el.child(
                            div().min_w_0().flex_1().child(
                                Label::new(directory.to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate_start(),
                            ),
                        )
                    }),
            )
            .when_some(overlap_tooltip, |el, tooltip| {
                el.child(
                    div()
                        .id(("change-overlap", index))
                        .flex_none()
                        .tooltip(Tooltip::text(tooltip))
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::XSmall)
                                .color(Color::Warning),
                        ),
                )
            })
            .when(file.additions > 0 || file.deletions > 0, |el| {
                el.child(
                    div().flex_none().child(
                        DiffStat::new(
                            ("change-stat", index),
                            file.additions as usize,
                            file.deletions as usize,
                        )
                        .label_size(LabelSize::XSmall),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_changes(
        &self,
        ws: &WorkspaceSummary,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.is_merged(ws) && !self.show_merged_changes {
            return self.render_merged_changes(ws, cx);
        }

        if ws.changed_files.is_empty() {
            return v_flex()
                .w_full()
                .flex_grow(1.0)
                .child(
                    h_flex().px_2().py_2().child(
                        Label::new("No changes")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
                .into_any_element();
        }

        let (total_additions, total_deletions) =
            ws.changed_files
                .iter()
                .fold((0usize, 0usize), |(additions, deletions), file| {
                    (
                        additions.saturating_add(file.additions as usize),
                        deletions.saturating_add(file.deletions as usize),
                    )
                });
        let file_count = ws.changed_files.len();
        let base_branch = ws.branch_health.base_branch.trim();
        let review_tooltip = if base_branch.is_empty() {
            "Open the full branch diff".to_string()
        } else {
            format!("Open the full branch diff against {base_branch}")
        };

        let header = h_flex()
            .px_2()
            .h(px(26.))
            .gap_2()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Label::new(if file_count == 1 {
                            "1 file".to_string()
                        } else {
                            format!("{file_count} files")
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        DiffStat::new("changes-total", total_additions, total_deletions)
                            .label_size(LabelSize::XSmall),
                    ),
            )
            .child(
                Button::new("review-all-changes", "Review all")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::XSmall)
                    .tooltip(Tooltip::text(review_tooltip))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.review_all_changes(window, cx);
                    })),
            );

        let list_focused = self.changes_focus_handle.is_focused(window);
        let mut list = v_flex()
            .id("changes-list")
            .key_context("MnemonicChangesList")
            .track_focus(&self.changes_focus_handle)
            .on_action(cx.listener(Self::select_next_change))
            .on_action(cx.listener(Self::select_previous_change))
            .on_action(cx.listener(Self::confirm_change))
            .w_full()
            .flex_1()
            .min_h_0()
            .pb_1()
            .overflow_y_scroll()
            .track_scroll(&self.changes_scroll_handle);

        for (index, entry) in self.change_entries(ws).into_iter().enumerate() {
            list = list.child(match entry {
                ChangeListEntry::Group { group, count } => h_flex()
                    .id(("change-group", index))
                    .px_2()
                    .pt_1()
                    .h(px(24.))
                    .gap_1()
                    .items_center()
                    .child(
                        Label::new(group.label())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(count.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder),
                    )
                    .into_any_element(),
                ChangeListEntry::Folder {
                    key,
                    directory,
                    count,
                    collapsed,
                } => h_flex()
                    .id(("change-folder", index))
                    .px_2()
                    .h(px(22.))
                    .gap_1()
                    .items_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_change_folder(key.clone(), cx);
                    }))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        div().min_w_0().flex_1().child(
                            Label::new(directory.to_string())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate_start(),
                        ),
                    )
                    .child(
                        Label::new(count.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder),
                    )
                    .into_any_element(),
                ChangeListEntry::File { file, in_folder } => {
                    self.render_change_file(index, file, in_folder, list_focused, cx)
                }
            });
        }

        v_flex()
            .w_full()
            .flex_1()
            .min_h_0()
            .child(header)
            .child(list)
            .into_any_element()
    }

    fn check_icon(check_status: &str, conclusion: Option<&str>) -> (IconName, Color) {
        if check_status.eq_ignore_ascii_case("completed") {
            match conclusion.unwrap_or("") {
                "SUCCESS" | "NEUTRAL" | "SKIPPED" => (IconName::Check, Color::Success),
                "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" => {
                    (IconName::Close, Color::Error)
                }
                _ => (IconName::Dash, Color::Muted),
            }
        } else {
            (IconName::ArrowCircle, Color::Warning)
        }
    }

    fn render_checks(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut section = v_flex().w_full().flex_grow(1.0).py_1();

        let Some(status) = self.pr_status.as_ref() else {
            let message = if self.pr_busy {
                "Checking GitHub…"
            } else {
                "No GitHub status yet"
            };
            return section.child(
                h_flex().px_2().py_2().child(
                    Label::new(message)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
        };

        if !status.found {
            section = section.child(
                h_flex().px_2().py_1().child(
                    Label::new(status.checks_summary.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
            if let Some(warning) = &status.warning {
                section = section.child(
                    h_flex().px_2().py_1().child(
                        Label::new(warning.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                    ),
                );
            }
            return section;
        }

        // Git/PR state rows, Conductor-style.
        let mut state_rows: Vec<(String, Color)> = Vec::new();
        if status.mergeable.as_deref() == Some("CONFLICTING")
            || status.merge_state_status.as_deref() == Some("DIRTY")
        {
            state_rows.push(("Merge conflicts detected".to_string(), Color::Error));
        }
        match status.review_decision.as_deref() {
            Some("APPROVED") => state_rows.push(("Approved".to_string(), Color::Success)),
            Some("CHANGES_REQUESTED") => {
                state_rows.push(("Changes requested".to_string(), Color::Warning))
            }
            Some("REVIEW_REQUIRED") | None if status.state.as_deref() == Some("OPEN") => {
                state_rows.push(("Waiting for PR review".to_string(), Color::Muted))
            }
            _ => {}
        }
        for (text, color) in state_rows {
            section = section.child(
                h_flex()
                    .px_2()
                    .h(px(24.))
                    .gap_1p5()
                    .items_center()
                    .child(Icon::new(IconName::Circle).size(IconSize::XSmall).color(color))
                    .child(Label::new(text).size(LabelSize::XSmall).color(color)),
            );
        }

        let any_failed = status.checks.iter().any(|check| {
            matches!(
                check.conclusion.as_deref(),
                Some("FAILURE") | Some("TIMED_OUT") | Some("CANCELLED") | Some("ACTION_REQUIRED")
            )
        });
        let has_conflicts = status.mergeable.as_deref() == Some("CONFLICTING")
            || status.merge_state_status.as_deref() == Some("DIRTY");
        if any_failed || has_conflicts {
            section = section.child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .child(
                        Button::new("fix-errors", "Fix errors")
                            .style(ButtonStyle::Tinted(TintColor::Warning))
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.fix_errors_via_agent(cx);
                            })),
                    )
                    .when(any_failed, |el| {
                        el.child(
                            Button::new("rerun-failed", "Re-run failed")
                                .label_size(LabelSize::Small)
                                .disabled(self.pr_busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rerun_failed_checks(cx);
                                })),
                        )
                    }),
            );
        }

        section = section.child(
            h_flex()
                .px_2()
                .h(px(26.))
                .items_center()
                .child(
                    Label::new(format!("Checks · {}", status.checks_summary))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        );

        if status.checks.is_empty() {
            section = section.child(
                h_flex().px_2().py_1().child(
                    Label::new("No checks reported")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
        }

        for (index, check) in status.checks.iter().enumerate() {
            let (icon, color) = Self::check_icon(&check.status, check.conclusion.as_deref());
            let url = check.url.clone();
            section = section.child(
                h_flex()
                    .id(("pr-check", index))
                    .px_2()
                    .h(px(24.))
                    .gap_1p5()
                    .items_center()
                    .w_full()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if let Some(url) = url.as_deref() {
                            cx.open_url(url);
                        }
                    }))
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                    .child(
                        div().flex_grow(1.0).overflow_hidden().child(
                            Label::new(check.name.clone())
                                .size(LabelSize::XSmall)
                                .single_line(),
                        ),
                    ),
            );
        }

        if status.state.as_deref() == Some("OPEN") {
            section = section.child(self.render_review_threads(cx));
        }

        if let Some(warning) = &status.warning {
            section = section.child(
                h_flex().px_2().py_1().child(
                    Label::new(warning.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                ),
            );
        }

        section
    }

    fn render_review_threads(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.review_threads.len();
        let heading = if self.review_threads_loading && count == 0 {
            "Review comments · loading…".to_string()
        } else {
            format!("Review comments · {count} unresolved")
        };
        let mut section = v_flex().w_full().pt_1().child(
            h_flex()
                .px_2()
                .h(px(26.))
                .gap_1()
                .items_center()
                .justify_between()
                .child(
                    Label::new(heading)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .when(count > 0, |el| {
                    el.child(
                        Button::new("send-all-comments", "Send all to agent")
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.send_review_comments_to_agent(None, cx);
                            })),
                    )
                }),
        );

        if let Some(error) = self.review_threads_error.clone() {
            section = section.child(
                h_flex().px_2().py_1().child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                ),
            );
        }

        for (index, comment) in self.review_threads.iter().enumerate() {
            let Some(thread_id) = comment.thread_id.clone() else {
                continue;
            };
            let resolving = self.resolving_thread.as_deref() == Some(thread_id.as_str());
            let url = comment.url.clone();
            let thread_for_send = thread_id.clone();
            let thread_for_resolve = thread_id.clone();
            section = section.child(
                v_flex()
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .w_full()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .w_full()
                            .child(Label::new(comment.author.clone()).size(LabelSize::XSmall))
                            .child(
                                div().flex_grow(1.0).overflow_hidden().child(
                                    Label::new(Self::comment_location(comment))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .single_line(),
                                ),
                            )
                            .when(comment.thread_outdated, |el| {
                                el.child(
                                    Label::new("outdated")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning),
                                )
                            })
                            .when(url.is_some(), |el| {
                                el.child(
                                    IconButton::new(
                                        ("open-comment", index),
                                        IconName::ArrowUpRight,
                                    )
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text("Open on GitHub"))
                                    .on_click(cx.listener(
                                        move |_, _, _, cx| {
                                            if let Some(url) = url.as_deref() {
                                                cx.open_url(url);
                                            }
                                        },
                                    )),
                                )
                            }),
                    )
                    .child(
                        Label::new(Self::comment_preview(&comment.body))
                            .size(LabelSize::XSmall)
                            .single_line(),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(("send-comment", index), "Send to agent")
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::XSmall)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.send_review_comments_to_agent(
                                            Some(&thread_for_send),
                                            cx,
                                        );
                                    })),
                            )
                            .when(comment.thread_resolvable, |el| {
                                el.child(
                                    Button::new(
                                        ("resolve-comment", index),
                                        if resolving { "Resolving…" } else { "Resolve" },
                                    )
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::XSmall)
                                    .disabled(self.resolving_thread.is_some())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.resolve_review_thread(
                                            thread_for_resolve.clone(),
                                            window,
                                            cx,
                                        );
                                    })),
                                )
                            }),
                    ),
            );
        }

        section
    }

    /// Scans the workspace's listening ports off the UI thread (lsof).
    /// `rescan` skips the port service's short-lived cache.
    fn refresh_ports(&mut self, rescan: bool, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.summary.as_ref().map(|summary| summary.id.clone()) else {
            return;
        };
        if self.ports_busy {
            return;
        }
        self.ports_busy = true;
        cx.notify();

        let state = mnemonic_bridge::mnemonic_state(cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let workspace_id = workspace_id.clone();
                    async move {
                        if rescan {
                            workspace_port_service::invalidate_workspace_port_cache(&workspace_id);
                        }
                        workspace_port_service::list_workspace_ports(&state, &workspace_id)
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.ports_busy = false;
                this.apply_ports_result(&workspace_id, result);
                this.schedule_ports_poll(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_ports_result(&mut self, workspace_id: &str, result: Result<Vec<WorkspacePort>, String>) {
        let still_selected = self
            .summary
            .as_ref()
            .is_some_and(|summary| summary.id == workspace_id);
        if !still_selected {
            return;
        }
        match result {
            Ok(ports) => {
                self.ports = ports;
                self.ports_error = None;
            }
            Err(err) => {
                log::warn!("Failed to list workspace ports: {err}");
                self.ports_error = Some(SharedString::from(err));
            }
        }
    }

    /// Keeps the Ports tab fresh only while it is the tab showing in a
    /// visible panel; opening the tab or the panel again restarts it.
    fn schedule_ports_poll(&mut self, cx: &mut Context<Self>) {
        if self.ports_poll_scheduled || self.tab != DetailTab::Ports || self.summary.is_none() {
            return;
        }
        self.ports_poll_scheduled = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PORTS_POLL_INTERVAL).await;
            this.update(cx, |this, cx| {
                this.ports_poll_scheduled = false;
                if this.tab == DetailTab::Ports && this.is_visible_in_dock(cx) {
                    this.refresh_ports(false, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn is_visible_in_dock(&self, cx: &Context<Self>) -> bool {
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };
        let panel_id = cx.entity_id();
        workspace.read(cx).all_docks().iter().any(|dock| {
            dock.read(cx)
                .visible_panel()
                .is_some_and(|panel| panel.panel_id() == panel_id)
        })
    }

    fn kill_port(&mut self, port: u16, pid: u32, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.summary.as_ref().map(|summary| summary.id.clone()) else {
            return;
        };
        self.ports_busy = true;
        cx.notify();

        let state = mnemonic_bridge::mnemonic_state(cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let workspace_id = workspace_id.clone();
                    async move {
                        workspace_port_service::kill_workspace_port_process(
                            &state,
                            &workspace_id,
                            port,
                            pid,
                        )
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.ports_busy = false;
                match result {
                    Ok(ports) => this.apply_ports_result(&workspace_id, Ok(ports)),
                    Err(err) => {
                        log::error!("Failed to kill port {port} (pid {pid}): {err}");
                        this.show_port_toast(format!("Couldn't kill port {port}: {err}"), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn show_port_toast(&self, message: String, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<MnemonicPortToast>(), message),
                    cx,
                );
            });
        }
    }

    fn render_ports(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut section = v_flex().w_full().flex_grow(1.0).py_1();

        if let Some(error) = self.ports_error.clone() {
            section = section.child(
                h_flex()
                    .px_2()
                    .py_1()
                    .child(Label::new(error).size(LabelSize::XSmall).color(Color::Warning)),
            );
        }

        if self.ports.is_empty() {
            let message = if self.ports_busy {
                "Scanning ports…"
            } else {
                "No listening ports from this workspace"
            };
            return section.child(
                h_flex().px_2().py_2().child(
                    Label::new(message)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
        }

        for (index, port) in self.ports.iter().enumerate() {
            let port_number = port.port;
            let pid = port.pid;
            section = section.child(
                h_flex()
                    .id(("workspace-port", index))
                    .px_2()
                    .h(px(26.))
                    .gap_1p5()
                    .items_center()
                    .w_full()
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .child(Label::new(format!(":{port_number}")).size(LabelSize::Small))
                    .child(
                        div().flex_grow(1.0).overflow_hidden().child(
                            Label::new(format!("{} · pid {pid}", port.command))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .single_line(),
                        ),
                    )
                    .child(
                        Button::new(("open-port", index), "Open")
                            .label_size(LabelSize::XSmall)
                            .tooltip(Tooltip::text(format!("Open http://localhost:{port_number}")))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.open_url(&format!("http://localhost:{port_number}"));
                            })),
                    )
                    .child(
                        IconButton::new(("kill-port", index), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .disabled(self.ports_busy)
                            .tooltip(Tooltip::text(format!("Kill pid {pid} (SIGTERM)")))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.kill_port(port_number, pid, cx);
                            })),
                    ),
            );
        }

        section
    }

    /// Lists the workspace's checkpoints (newest first) off the UI thread.
    fn refresh_checkpoints(&mut self, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.summary.as_ref().map(|summary| summary.id.clone()) else {
            return;
        };
        self.checkpoints_loading = true;
        cx.notify();

        let state = mnemonic_bridge::mnemonic_state(cx);
        self.checkpoints_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let workspace_id = workspace_id.clone();
                    async move {
                        checkpoint_service::list_workspace_checkpoints(&state, &workspace_id)
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.checkpoints_loading = false;
                let still_selected = this
                    .summary
                    .as_ref()
                    .is_some_and(|summary| summary.id == workspace_id);
                if still_selected {
                    match result {
                        Ok(checkpoints) => {
                            this.checkpoints = checkpoints;
                            this.checkpoints_error = None;
                        }
                        Err(err) => {
                            log::warn!("Failed to list checkpoints: {err}");
                            this.checkpoints_error = Some(SharedString::from(err));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Called by the workspace view after it saves a checkpoint.
    pub(crate) fn checkpoints_changed(&mut self, workspace_id: &str, cx: &mut Context<Self>) {
        let showing = self.tab == DetailTab::Checkpoints
            && self
                .summary
                .as_ref()
                .is_some_and(|summary| summary.id == workspace_id);
        if showing {
            self.refresh_checkpoints(cx);
        }
    }

    fn agent_busy(&self, workspace_id: &str, cx: &App) -> bool {
        self.workspace.upgrade().is_some_and(|workspace| {
            WorkspaceHost::workspace_agent_busy(workspace.read(cx), workspace_id, cx)
        })
    }

    fn show_agent_busy_prompt(window: &mut Window, cx: &mut App) {
        let _dismissed = window.prompt(
            PromptLevel::Warning,
            "An agent is working in this workspace",
            Some("Wait for its turn to finish, or stop it, before restoring a checkpoint."),
            &["OK"],
            cx,
        );
    }

    fn show_checkpoint_toast(&self, message: String, cx: &mut Context<Self>) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<MnemonicCheckpointToast>(), message),
                    cx,
                );
            });
        }
    }

    fn restore_prompt_detail(plan: &WorkspaceCheckpointRestorePlan) -> String {
        let mut lines = Vec::new();
        let file_count = plan.files_to_restore.len();
        if file_count > 0 {
            let mut names = plan
                .files_to_restore
                .iter()
                .take(RESTORE_PROMPT_FILE_LIMIT)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            if file_count > RESTORE_PROMPT_FILE_LIMIT {
                names.push_str(&format!(
                    " and {} more",
                    file_count - RESTORE_PROMPT_FILE_LIMIT
                ));
            }
            let plural = if file_count == 1 { "" } else { "s" };
            lines.push(format!(
                "{file_count} tracked file{plural} will be changed back to this checkpoint: {names}."
            ));
        }
        if plan.current_dirty {
            lines.push(
                "Your current uncommitted changes are saved as a new checkpoint first, so this \
                 can be undone."
                    .to_string(),
            );
        }
        lines.extend(plan.warnings.iter().cloned());
        lines.join("\n\n")
    }

    /// Restores a checkpoint after showing its restore plan. Refused while an
    /// agent in the workspace is mid-turn, both before and after the prompt.
    fn restore_checkpoint(
        &mut self,
        reference: String,
        label: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace_id) = self.summary.as_ref().map(|summary| summary.id.clone()) else {
            return;
        };
        if self.checkpoint_restoring {
            return;
        }
        if self.agent_busy(&workspace_id, cx) {
            Self::show_agent_busy_prompt(window, cx);
            return;
        }
        self.checkpoint_restoring = true;
        cx.notify();

        let state = mnemonic_bridge::mnemonic_state(cx);
        cx.spawn_in(window, async move |this, cx| {
            let plan = cx
                .background_spawn({
                    let state = state.clone();
                    let workspace_id = workspace_id.clone();
                    let reference = reference.clone();
                    async move {
                        checkpoint_service::get_workspace_checkpoint_restore_plan(
                            &state,
                            &workspace_id,
                            &reference,
                        )
                    }
                })
                .await;

            let answer = this.update_in(cx, |this, window, cx| match plan {
                Err(err) => {
                    this.checkpoint_restoring = false;
                    cx.notify();
                    let _dismissed = window.prompt(
                        PromptLevel::Critical,
                        "Couldn't prepare the restore",
                        Some(&err),
                        &["OK"],
                        cx,
                    );
                    None
                }
                Ok(plan) if !plan.overwritten_untracked_files.is_empty() => {
                    this.checkpoint_restoring = false;
                    cx.notify();
                    let _dismissed = window.prompt(
                        PromptLevel::Critical,
                        "This checkpoint can't be restored yet",
                        Some(&Self::restore_prompt_detail(&plan)),
                        &["OK"],
                        cx,
                    );
                    None
                }
                Ok(plan) => Some(window.prompt(
                    PromptLevel::Warning,
                    &format!("Restore to before \"{label}\"?"),
                    Some(&Self::restore_prompt_detail(&plan)),
                    &["Restore", "Cancel"],
                    cx,
                )),
            });
            let Ok(Some(answer)) = answer else {
                return;
            };
            let confirmed = answer.await == Ok(0);

            let proceed = this.update_in(cx, |this, window, cx| {
                let busy = this.agent_busy(&workspace_id, cx);
                if !confirmed || busy {
                    this.checkpoint_restoring = false;
                    cx.notify();
                }
                if confirmed && busy {
                    Self::show_agent_busy_prompt(window, cx);
                }
                confirmed && !busy
            });
            if !matches!(proceed, Ok(true)) {
                return;
            }

            let result = cx
                .background_spawn(async move {
                    checkpoint_service::restore_workspace_checkpoint(
                        &state,
                        &workspace_id,
                        &reference,
                    )
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.checkpoint_restoring = false;
                match result {
                    Ok(restored) => this.show_checkpoint_toast(restored.message, cx),
                    Err(err) => {
                        log::error!("Failed to restore checkpoint: {err}");
                        let _dismissed = window.prompt(
                            PromptLevel::Critical,
                            "Couldn't restore checkpoint",
                            Some(&err),
                            &["OK"],
                            cx,
                        );
                    }
                }
                this.refresh(cx);
                this.refresh_checkpoints(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens what changed in tracked files since the checkpoint as a
    /// read-only diff in the editor.
    fn view_checkpoint_changes(
        &mut self,
        reference: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace_id) = self.summary.as_ref().map(|summary| summary.id.clone()) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        if project.read(cx).is_remote() {
            self.show_checkpoint_toast(
                "Checkpoint diffs can only be opened in local projects".to_string(),
                cx,
            );
            return;
        }
        let language = project.read(cx).languages().language_for_name("Diff");
        let state = mnemonic_bridge::mnemonic_state(cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    checkpoint_service::get_workspace_changes_since_checkpoint(
                        &state,
                        &workspace_id,
                        &reference,
                    )
                })
                .await;
            let diff = match result {
                Ok(diff) if diff.diff.trim().is_empty() => {
                    this.update(cx, |this, cx| {
                        this.show_checkpoint_toast(
                            "No tracked file has changed since this checkpoint".to_string(),
                            cx,
                        );
                    })
                    .ok();
                    return;
                }
                Ok(diff) => diff.diff,
                Err(err) => {
                    log::error!("Failed to diff checkpoint: {err}");
                    this.update(cx, |this, cx| {
                        this.show_checkpoint_toast(format!("Couldn't load the diff: {err}"), cx);
                    })
                    .ok();
                    return;
                }
            };
            let language = language.await.ok();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let buffer = project.update(cx, |project, cx| {
                        project.create_local_buffer(&diff, language, false, cx)
                    });
                    // Read-only so closing the diff never asks to save it.
                    buffer.update(cx, |buffer, cx| {
                        buffer.set_capability(language::Capability::ReadOnly, cx);
                    });
                    let editor =
                        cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
                    workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
                })
                .ok();
        })
        .detach();
    }

    fn relative_time(created_at: &str) -> String {
        let Ok(created) = created_at.parse::<u64>() else {
            return String::new();
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(created);
        let elapsed = now.saturating_sub(created);
        match elapsed {
            0..60 => "just now".to_string(),
            60..3_600 => format!("{}m ago", elapsed / 60),
            3_600..86_400 => format!("{}h ago", elapsed / 3_600),
            _ => format!("{}d ago", elapsed / 86_400),
        }
    }

    fn render_checkpoints(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut section = v_flex().w_full().flex_grow(1.0).py_1();

        if let Some(error) = self.checkpoints_error.clone() {
            section = section.child(
                h_flex().px_2().py_1().child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                ),
            );
        }

        if self.checkpoints.is_empty() {
            let message = if self.checkpoints_loading {
                "Loading checkpoints…"
            } else {
                "No checkpoints yet. One is saved before each agent turn."
            };
            return section.child(
                h_flex().px_2().py_2().child(
                    Label::new(message)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
        }

        for (index, checkpoint) in self.checkpoints.iter().enumerate() {
            let label = SharedString::from(
                checkpoint_service::checkpoint_label(&checkpoint.subject).to_string(),
            );
            let reference_for_diff = checkpoint.reference.clone();
            let reference_for_restore = checkpoint.reference.clone();
            let label_for_restore = label.clone();
            section = section.child(
                h_flex()
                    .id(("workspace-checkpoint", index))
                    .px_2()
                    .h(px(26.))
                    .gap_1p5()
                    .items_center()
                    .w_full()
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .tooltip(Tooltip::text(format!("{label}\n{}", checkpoint.short_oid)))
                    .child(
                        div().flex_grow(1.0).overflow_hidden().child(
                            Label::new(label.clone())
                                .size(LabelSize::XSmall)
                                .single_line(),
                        ),
                    )
                    .child(
                        Label::new(Self::relative_time(&checkpoint.created_at))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        IconButton::new(("checkpoint-diff", index), IconName::FileDiff)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("View changes since this checkpoint"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.view_checkpoint_changes(
                                    reference_for_diff.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        IconButton::new(("checkpoint-restore", index), IconName::RotateCcw)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .disabled(self.checkpoint_restoring)
                            .tooltip(Tooltip::text("Restore files to this checkpoint"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.restore_checkpoint(
                                    reference_for_restore.clone(),
                                    label_for_restore.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    ),
            );
        }

        section
    }

    fn render_file_tree(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_grow(1.0)
            .w_full()
            .min_h(px(0.))
            .overflow_hidden()
            .map(|el| match self.file_tree.clone() {
                Some(tree) => el.child(tree),
                None => el.child(
                    h_flex().px_2().py_2().child(
                        Label::new("Loading file tree…")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                ),
            })
    }

    fn render_empty(&self) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                Label::new("No workspace selected")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new("Select a worktree on the left")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }
}

impl Focusable for WorkspaceDetailPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorkspaceDetailPanel {}

impl Panel for WorkspaceDetailPanel {
    fn persistent_name() -> &'static str {
        "MnemonicWorkspaceDetailPanel"
    }

    fn panel_key() -> &'static str {
        WORKSPACE_DETAIL_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
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
        px(300.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::FileDiff)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Workspace Detail")
    }

    fn icon_label(&self, _window: &Window, _cx: &App) -> Option<String> {
        let count = self
            .summary
            .as_ref()
            .map(|ws| ws.changed_files.len())
            .unwrap_or(0);
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
        7
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active && self.tab == DetailTab::Ports {
            self.refresh_ports(true, cx);
        }
    }
}

impl Render for WorkspaceDetailPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.maybe_refresh_change_annotations(cx);

        let content = v_flex()
            .id("workspace-detail-panel")
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background);

        match self.summary.clone() {
            Some(ws) => {
                let body = match self.tab {
                    DetailTab::AllFiles => self.render_file_tree(cx).into_any_element(),
                    DetailTab::Changes => self.render_changes(&ws, window, cx),
                    DetailTab::Checks => self.render_checks(cx).into_any_element(),
                    DetailTab::Ports => self.render_ports(cx).into_any_element(),
                    DetailTab::Checkpoints => self.render_checkpoints(cx).into_any_element(),
                };
                content
                    .child(self.render_header(&ws, cx))
                    .child(self.render_tabs(&ws, cx))
                    .child(body)
            }
            None => content.child(self.render_empty()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.to_string(),
            additions: 1,
            deletions: 0,
            status: "modified".to_string(),
        }
    }

    fn describe(entries: &[ChangeListEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                ChangeListEntry::Group { group, count } => format!("# {} {count}", group.label()),
                ChangeListEntry::Folder {
                    directory,
                    count,
                    collapsed,
                    ..
                } => format!(
                    "> {directory} {count}{}",
                    if *collapsed { " collapsed" } else { "" }
                ),
                ChangeListEntry::File { file, in_folder } => {
                    format!("{}{}", if *in_folder { "  " } else { "" }, file.path)
                }
            })
            .collect()
    }

    #[test]
    fn changes_split_into_groups_and_fold_into_folders_past_threshold() {
        let files = vec![changed("src/a.rs"), changed("README.md")];
        let uncommitted = HashSet::from(["README.md".to_string()]);
        assert_eq!(
            describe(&change_list_entries(
                &files,
                Some(&uncommitted),
                &HashSet::new()
            )),
            ["# Uncommitted 1", "README.md", "# On branch 1", "src/a.rs"],
        );
        assert_eq!(
            describe(&change_list_entries(
                &files,
                Some(&HashSet::new()),
                &HashSet::new()
            )),
            ["# On branch 2", "src/a.rs", "README.md"],
        );

        let mut files = (0..CHANGE_FOLDER_THRESHOLD)
            .map(|index| changed(&format!("src/file_{index:02}.rs")))
            .collect::<Vec<_>>();
        files.push(changed("docs/guide.md"));
        files.push(changed("Cargo.toml"));
        let collapsed = HashSet::from(["On branch:src".to_string()]);
        assert_eq!(
            describe(&change_list_entries(
                &files,
                Some(&HashSet::new()),
                &collapsed
            )),
            [
                "# On branch 14",
                "Cargo.toml",
                "> docs 1",
                "  docs/guide.md",
                "> src 12 collapsed",
            ],
        );
    }
}
