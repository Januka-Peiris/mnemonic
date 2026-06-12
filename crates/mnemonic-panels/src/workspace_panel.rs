use gpui::{
    actions, px, Action, App, AppContext as _, Context, Entity, EventEmitter, Focusable,
    FocusHandle, IntoElement, Pixels, Render, WeakEntity, Window,
};
use mnemonic_core::models::WorkspaceSummary;
use mnemonic_core::services::workspace_service;
use ui::{h_flex, v_flex, IconButton, IconName, Label, prelude::*, utils::platform_title_bar_height};
use workspace::dock::{DockPosition, PanelEvent};
use workspace::{Panel, Workspace};

actions!(mnemonic_workspace_panel, [ToggleFocus]);

const WORKSPACE_PANEL_KEY: &str = "MnemonicWorkspacePanel";

pub struct WorkspacePanel {
    focus_handle: FocusHandle,
    _workspace: WeakEntity<Workspace>,
    workspaces: Vec<WorkspaceSummary>,
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
                _workspace: workspace.weak_handle(),
                workspaces: Vec::new(),
            };
            panel.refresh_workspaces(cx);
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

    fn refresh_workspaces(&mut self, cx: &App) {
        let state = mnemonic_bridge::mnemonic_state(cx);
        match workspace_service::list_workspaces(&state) {
            Ok(ws) => self.workspaces = ws,
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

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_2()
            .py_1()
            .justify_between()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new("Workspaces")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                IconButton::new("refresh-workspaces", IconName::ArrowCircle)
                    .icon_size(IconSize::Small)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.refresh_workspaces(cx);
                        cx.notify();
                    })),
            )
    }

    fn render_workspace_row(
        &self,
        ws: &WorkspaceSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_color = self.status_color(&ws.status, cx);

        let agent_text = if ws.agent_session.status == "running" {
            format!("{} running", ws.agent_session.agent)
        } else if !ws.agent_session.agent.is_empty() {
            ws.agent_session.agent.clone()
        } else {
            String::new()
        };

        let files_changed = ws.changed_files.len();

        v_flex()
            .id(ws.id.clone())
            .px_2()
            .py(px(6.))
            .gap_0p5()
            .w_full()
            .rounded_md()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(
                        div()
                            .w(px(6.))
                            .h(px(6.))
                            .rounded_full()
                            .bg(status_color),
                    )
                    .child(
                        Label::new(ws.name.clone())
                            .size(LabelSize::Small)
                            .color(Color::Default),
                    ),
            )
            .child(
                h_flex()
                    .pl(px(16.))
                    .gap_2()
                    .child(
                        Label::new(ws.branch.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .when(files_changed > 0, |el| {
                        el.child(
                            Label::new(format!("+{files_changed}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when(!agent_text.is_empty(), |el| {
                el.child(
                    h_flex().pl(px(16.)).child(
                        Label::new(agent_text)
                            .size(LabelSize::XSmall)
                            .color(Color::Accent),
                    ),
                )
            })
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                Label::new("No workspaces yet")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new("Create one to get started")
                    .size(LabelSize::XSmall)
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
        Some("Workspaces")
    }

    fn icon_label(&self, _window: &Window, _cx: &App) -> Option<String> {
        let active = self
            .workspaces
            .iter()
            .filter(|w| w.status == "Active")
            .count();
        if active > 0 {
            Some(active.to_string())
        } else {
            None
        }
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        2
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }
}

impl Render for WorkspacePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_workspaces(cx);

        let has_workspaces = !self.workspaces.is_empty();
        let titlebar_height = platform_title_bar_height(window);

        v_flex()
            .id("workspace-panel")
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .pt(titlebar_height)
            .child(self.render_header(cx))
            .when(has_workspaces, |el| {
                let workspaces = self.workspaces.clone();
                let rows: Vec<_> = workspaces
                    .iter()
                    .map(|ws| self.render_workspace_row(ws, cx))
                    .collect();
                el.child(
                    v_flex()
                        .py_1()
                        .overflow_y_hidden()
                        .flex_grow(1.0)
                        .children(rows),
                )
            })
            .when(!has_workspaces, |el| {
                el.child(self.render_empty(cx))
            })
    }
}
