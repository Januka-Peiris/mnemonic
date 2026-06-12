use gpui::{
    actions, px, Action, App, AppContext as _, Context, Entity, EventEmitter, Focusable,
    FocusHandle, IntoElement, Pixels, Render, WeakEntity, Window,
};
use mnemonic_core::events;
use mnemonic_core::models::WorkspaceCoordinatorStatus;
use mnemonic_core::services::{coordinator_service, workspace_service};
use ui::{h_flex, v_flex, IconButton, IconName, Label, prelude::*};
use workspace::dock::{DockPosition, PanelEvent};
use workspace::{Panel, Workspace};

actions!(mnemonic_coordinator_panel, [ToggleFocus]);

const COORDINATOR_PANEL_KEY: &str = "MnemonicCoordinatorPanel";

pub struct CoordinatorPanel {
    focus_handle: FocusHandle,
    _workspace: WeakEntity<Workspace>,
    statuses: Vec<WorkspaceCoordinatorStatus>,
    last_notifications: Vec<(String, String)>,
}

impl CoordinatorPanel {
    pub fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let panel = cx.new(|cx| {
            let mut this = Self {
                focus_handle: cx.focus_handle(),
                _workspace: workspace.weak_handle(),
                statuses: Vec::new(),
                last_notifications: Vec::new(),
            };
            this.refresh(cx);
            this
        });

        let event_hub = mnemonic_bridge::mnemonic_event_hub(cx);
        cx.subscribe(&event_hub, {
            let panel = panel.downgrade();
            move |_workspace, _hub, event: &mnemonic_bridge::MnemonicEvent, cx| {
                if event.name == events::COORDINATOR_NOTIFY
                    || event.name == events::ORCHESTRATOR_NOTIFY
                {
                    if let Some(panel) = panel.upgrade() {
                        panel.update(cx, |this, cx| {
                            if let (Some(ws_id), Some(msg)) = (
                                event.payload.get("workspaceId").and_then(|v| v.as_str()),
                                event.payload.get("message").and_then(|v| v.as_str()),
                            ) {
                                this.last_notifications
                                    .push((ws_id.to_string(), msg.to_string()));
                                if this.last_notifications.len() > 20 {
                                    this.last_notifications.remove(0);
                                }
                            }
                            this.refresh(cx);
                            cx.notify();
                        });
                    }
                }
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
        let state = mnemonic_bridge::mnemonic_state(cx);
        let workspaces = workspace_service::list_workspaces(&state).unwrap_or_default();

        self.statuses.clear();
        for ws in &workspaces {
            if ws.status != "Active" {
                continue;
            }
            if let Ok(status) =
                coordinator_service::get_workspace_coordinator_status(&state, &ws.id)
            {
                self.statuses.push(status);
            }
        }
    }

    fn render_coordinator_status(
        &self,
        status: &WorkspaceCoordinatorStatus,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let worker_count = status.workers.len();

        let mode_color = match status.mode.as_str() {
            "running" | "active" => cx.theme().status().info,
            "stopped" | "idle" => cx.theme().colors().text_muted,
            "paused" => cx.theme().status().warning,
            _ => cx.theme().colors().text_muted,
        };

        let workers_label = if worker_count > 0 {
            let running = status
                .workers
                .iter()
                .filter(|w| w.status == "running")
                .count();
            format!("{running}/{worker_count} workers")
        } else {
            "No workers".to_string()
        };

        v_flex()
            .px_2()
            .py(px(6.))
            .gap_0p5()
            .w_full()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(
                        div()
                            .w(px(6.))
                            .h(px(6.))
                            .rounded_full()
                            .bg(mode_color),
                    )
                    .child(
                        Label::new(status.workspace_id.clone())
                            .size(LabelSize::Small)
                            .color(Color::Default),
                    ),
            )
            .child(
                h_flex().pl(px(16.)).gap_2().child(
                    Label::new(workers_label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
            .when(!status.recent_actions.is_empty(), |el| {
                let last = status.recent_actions.last().unwrap();
                let action_text = last
                    .action_kind
                    .clone();
                el.child(
                    h_flex().pl(px(16.)).child(
                        Label::new(action_text)
                            .size(LabelSize::XSmall)
                            .color(Color::Accent),
                    ),
                )
            })
    }

    fn render_notification(
        &self,
        ws_id: &str,
        msg: &str,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .px_2()
            .py_0p5()
            .gap_1()
            .child(
                Label::new(ws_id.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(msg.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Default),
            )
    }
}

impl Focusable for CoordinatorPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for CoordinatorPanel {}

impl Panel for CoordinatorPanel {
    fn persistent_name() -> &'static str {
        "MnemonicCoordinatorPanel"
    }

    fn panel_key() -> &'static str {
        COORDINATOR_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(
            position,
            DockPosition::Left | DockPosition::Right | DockPosition::Bottom
        )
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(200.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::Sparkle)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Coordinator")
    }

    fn icon_label(&self, _window: &Window, _cx: &App) -> Option<String> {
        let active = self
            .statuses
            .iter()
            .filter(|s| s.active_run.is_some())
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
        6
    }
}

impl Render for CoordinatorPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh(cx);

        let has_content =
            !self.statuses.is_empty() || !self.last_notifications.is_empty();

        let mut content = v_flex()
            .id("coordinator-panel")
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .justify_between()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new("Coordinator")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        IconButton::new("refresh-coordinator", IconName::ArrowCircle)
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.refresh(cx);
                                cx.notify();
                            })),
                    ),
            );

        if has_content {
            let statuses = self.statuses.clone();
            let status_rows: Vec<_> = statuses
                .iter()
                .map(|s| self.render_coordinator_status(s, cx))
                .collect();

            let notifications = self.last_notifications.clone();
            let notification_rows: Vec<_> = notifications
                .iter()
                .rev()
                .take(10)
                .map(|(ws_id, msg)| self.render_notification(ws_id, msg, cx))
                .collect();

            content = content.child(
                v_flex()
                    .overflow_y_hidden()
                    .flex_grow(1.0)
                    .children(status_rows)
                    .when(!notification_rows.is_empty(), |el| {
                        el.child(
                            v_flex()
                                .px_2()
                                .py_1()
                                .child(
                                    Label::new("Recent activity")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .children(notification_rows),
                        )
                    }),
            );
        } else {
            content = content.child(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .child(
                        Label::new("No active coordinators")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            );
        }

        content
    }
}
