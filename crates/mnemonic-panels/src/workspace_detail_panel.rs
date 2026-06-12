use std::path::PathBuf;

use gpui::{
    actions, px, Action, App, AppContext as _, Context, Entity, EventEmitter, Focusable,
    FocusHandle, IntoElement, Pixels, Render, SharedString, WeakEntity, Window,
};
use mnemonic_core::models::{WorkspacePrStatus, WorkspaceSummary};
use mnemonic_core::services::{pr_draft_service, workspace_service};
use ui::{
    h_flex, v_flex, Button, ButtonStyle, Icon, IconButton, IconName, Label, TintColor, Tooltip,
    prelude::*,
};
use std::time::{Duration, Instant};

use workspace::dock::{DockPosition, PanelEvent};
use workspace::notifications::NotificationId;
use workspace::{OpenOptions, Panel, Toast, Workspace};

use crate::ActiveWorkspace;
use crate::workspace_view::WorkspaceHost;

actions!(mnemonic_workspace_detail_panel, [ToggleFocus]);

const WORKSPACE_DETAIL_PANEL_KEY: &str = "MnemonicWorkspaceDetailPanel";

#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailTab {
    AllFiles,
    Changes,
    Checks,
}

/// How often the panel re-queries GitHub while a workspace is selected.
const PR_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Faster cadence right after dispatching an instruction (PR creation, CI
/// fix), so the panel catches the result without a manual refresh.
const PR_POLL_FAST_INTERVAL: Duration = Duration::from_secs(15);
/// How long the fast cadence lasts after a dispatch.
const PR_POLL_BOOST: Duration = Duration::from_secs(300);

/// Marker for PR state-change toasts.
struct MnemonicPrToast;

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
            };
            panel.refresh(cx);
            panel
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
                        if let Some(message) =
                            Self::transition_message(this.pr_status.as_ref(), &status)
                        {
                            this.show_pr_toast(message, cx);
                        }
                        this.pr_status = Some(status);
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
    fn create_pr(&mut self, cx: &mut Context<Self>) {
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
        let sent = workspace.update(cx, |workspace, cx| {
            WorkspaceHost::dispatch_to_workspace_agent(workspace, &workspace_id, prompt, cx)
        });
        self.dispatch_note = Some(if sent {
            self.poll_fast_until = Some(Instant::now() + PR_POLL_BOOST);
            self.schedule_next_poll(cx);
            SharedString::from(format!("Sent {label} to claude ✓"))
        } else {
            SharedString::from(format!(
                "Couldn't send {label}: open the workspace first (click its worktree)"
            ))
        });
        cx.notify();
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

    fn render_pr_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.pr_busy;
        match self.pr_status.as_ref().filter(|status| status.found) {
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
            }
            None => Button::new("create-pr", if busy { "Working…" } else { "Create PR" })
                .style(ButtonStyle::Filled)
                .label_size(LabelSize::Small)
                .disabled(busy || self.summary.is_none())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.create_pr(cx);
                })),
        }
    }

    /// Create PR and immediately arm the CI auto-fix loop, so the whole
    /// create → watch checks → fix → push cycle runs hands-off.
    fn create_pr_with_auto_fix(&mut self, cx: &mut Context<Self>) {
        self.create_pr(cx);
        let Some(workspace_id) = self.summary.as_ref().map(|summary| summary.id.clone()) else {
            return;
        };
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                WorkspaceHost::enable_auto_fix_for_workspace(workspace, &workspace_id, cx);
            });
        }
    }

    fn render_header(&self, ws: &WorkspaceSummary, cx: &mut Context<Self>) -> impl IntoElement {
        let status_color = self.status_color(&ws.status, cx);
        let ahead = ws.branch_health.ahead_by;
        let behind = ws.branch_health.behind_by;

        v_flex()
            .px_2()
            .py_1p5()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
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
                                !self
                                    .pr_status
                                    .as_ref()
                                    .is_some_and(|status| status.found),
                                |el| {
                                    el.child(
                                        Button::new("create-pr-autofix", "+ Auto-fix")
                                            .style(ButtonStyle::Tinted(TintColor::Accent))
                                            .label_size(LabelSize::Small)
                                            .disabled(self.pr_busy || self.summary.is_none())
                                            .tooltip(Tooltip::text(
                                                "Create PR, then watch CI and auto-fix until green",
                                            ))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.create_pr_with_auto_fix(cx);
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
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tab = tab;
                    if tab == DetailTab::Checks && this.pr_status.is_none() && !this.pr_busy {
                        this.refresh_pr_status(cx);
                    }
                    cx.notify();
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
            .child(div().flex_grow(1.0))
            .child(
                IconButton::new("refresh-pr-status", IconName::ArrowCircle)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Refresh GitHub status"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.refresh_pr_status(cx);
                    })),
            )
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

    fn render_changes(&self, ws: &WorkspaceSummary, cx: &mut Context<Self>) -> impl IntoElement {
        let mut section = v_flex().w_full().flex_grow(1.0);

        if ws.changed_files.is_empty() {
            return section.child(
                h_flex().px_2().py_2().child(
                    Label::new("No changes")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            );
        }

        for file in &ws.changed_files {
            let additions = file.additions;
            let deletions = file.deletions;
            let (status_label, status_label_color) = Self::change_status_label(&file.status);
            let path_for_open = file.path.clone();
            section = section.child(
                h_flex()
                    .id(SharedString::from(format!("changed-{}", file.path)))
                    .px_2()
                    .h(px(24.))
                    .gap_1p5()
                    .items_center()
                    .w_full()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_changed_file(&path_for_open, window, cx);
                    }))
                    .child(
                        Label::new(status_label)
                            .size(LabelSize::XSmall)
                            .color(status_label_color),
                    )
                    .child(
                        div().flex_grow(1.0).overflow_hidden().child(
                            Label::new(file.path.clone())
                                .size(LabelSize::XSmall)
                                .single_line(),
                        ),
                    )
                    .when(additions > 0, |el| {
                        el.child(
                            Label::new(format!("+{additions}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Created),
                        )
                    })
                    .when(deletions > 0, |el| {
                        el.child(
                            Label::new(format!("-{deletions}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Deleted),
                        )
                    }),
            );
        }

        section
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
}

impl Render for WorkspaceDetailPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh(cx);

        let content = v_flex()
            .id("workspace-detail-panel")
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background);

        match self.summary.clone() {
            Some(ws) => {
                let body = match self.tab {
                    DetailTab::AllFiles => self.render_file_tree(cx).into_any_element(),
                    DetailTab::Changes => self.render_changes(&ws, cx).into_any_element(),
                    DetailTab::Checks => self.render_checks(cx).into_any_element(),
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
