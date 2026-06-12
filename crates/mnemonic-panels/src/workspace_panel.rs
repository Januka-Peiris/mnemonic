use gpui::{
    actions, px, Action, App, AppContext as _, Context, Entity, EventEmitter, Focusable,
    FocusHandle, IntoElement, Pixels, Render, WeakEntity, Window,
};
use mnemonic_core::models::WorkspaceSummary;
use mnemonic_core::services::workspace_service;
use ui::{h_flex, v_flex, Label, prelude::*};
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

    fn render_workspace_row(
        &self,
        ws: &WorkspaceSummary,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let agent_label = if ws.agent_session.status == "running" {
            format!("{} (running)", ws.agent_session.agent)
        } else if !ws.agent_session.agent.is_empty() {
            ws.agent_session.agent.clone()
        } else {
            String::new()
        };

        let files_changed = ws.changed_files.len();

        v_flex()
            .px_2()
            .py_1()
            .gap_0p5()
            .w_full()
            .rounded_md()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Label::new(ws.name.clone())
                            .size(LabelSize::Default)
                            .color(Color::Default),
                    )
                    .child(
                        Label::new(ws.status.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(ws.branch.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .when(files_changed > 0, |el| {
                        el.child(
                            Label::new(format!("{files_changed} files"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when(!agent_label.is_empty(), |el| {
                el.child(
                    Label::new(agent_label)
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
            })
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
        px(280.)
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

        v_flex()
            .id("workspace-panel")
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new("Workspaces")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .when(has_workspaces, |el| {
                let rows: Vec<_> = self
                    .workspaces
                    .clone()
                    .into_iter()
                    .map(|ws| self.render_workspace_row(&ws, window, cx))
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
                el.child(
                    v_flex()
                        .size_full()
                        .items_center()
                        .justify_center()
                        .child(
                            Label::new("No workspaces")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
            })
    }
}
