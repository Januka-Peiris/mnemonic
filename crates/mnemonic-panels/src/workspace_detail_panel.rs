use gpui::{
    actions, px, Action, App, AppContext as _, Context, Entity, EventEmitter, Focusable,
    FocusHandle, IntoElement, Pixels, Render, SharedString, WeakEntity, Window,
};
use mnemonic_core::models::WorkspaceSummary;
use mnemonic_core::services::workspace_service;
use ui::{h_flex, v_flex, Icon, IconName, Label, prelude::*};
use workspace::dock::{DockPosition, PanelEvent};
use workspace::{Panel, Workspace};

use crate::ActiveWorkspace;

actions!(mnemonic_workspace_detail_panel, [ToggleFocus]);

const WORKSPACE_DETAIL_PANEL_KEY: &str = "MnemonicWorkspaceDetailPanel";

pub struct WorkspaceDetailPanel {
    focus_handle: FocusHandle,
    workspace: Option<WorkspaceSummary>,
}

impl WorkspaceDetailPanel {
    pub fn new(
        _workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            cx.observe_global::<ActiveWorkspace>(|this: &mut Self, cx| {
                this.refresh(cx);
                cx.notify();
            })
            .detach();

            let mut panel = Self {
                focus_handle: cx.focus_handle(),
                workspace: None,
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
        let selected_id = cx
            .try_global::<ActiveWorkspace>()
            .and_then(|active| active.0.clone());
        let Some(selected_id) = selected_id else {
            self.workspace = None;
            return;
        };

        let state = mnemonic_bridge::mnemonic_state(cx);
        match workspace_service::list_workspaces(&state) {
            Ok(workspaces) => {
                self.workspace = workspaces.into_iter().find(|ws| ws.id == selected_id);
            }
            Err(e) => log::error!("Failed to list workspaces: {e}"),
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

    fn render_header(&self, ws: &WorkspaceSummary, cx: &mut Context<Self>) -> impl IntoElement {
        let status_color = self.status_color(&ws.status, cx);
        let pr_text = ws
            .pr_number
            .map(|number| match ws.pr_status.as_deref() {
                Some(status) if !status.is_empty() => format!("#{number} · {status}"),
                _ => format!("#{number}"),
            });
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
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Label::new(ws.status.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .when_some(pr_text, |el, pr_text| {
                        el.child(
                            Label::new(pr_text)
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                    }),
            )
    }

    fn render_changes(&self, ws: &WorkspaceSummary, cx: &mut Context<Self>) -> impl IntoElement {
        let count = ws.changed_files.len();

        let mut section = v_flex().w_full().child(
            h_flex()
                .px_2()
                .h(px(26.))
                .items_center()
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .child(
                    Label::new(format!("Changes {count}"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        );

        if count == 0 {
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
            section = section.child(
                h_flex()
                    .id(SharedString::from(format!("changed-{}", file.path)))
                    .px_2()
                    .h(px(24.))
                    .gap_1p5()
                    .items_center()
                    .w_full()
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
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
            .workspace
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

        match self.workspace.clone() {
            Some(ws) => content
                .child(self.render_header(&ws, cx))
                .child(self.render_changes(&ws, cx)),
            None => content.child(self.render_empty()),
        }
    }
}
