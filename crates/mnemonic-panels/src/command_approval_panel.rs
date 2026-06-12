use gpui::{
    actions, px, Action, App, AppContext as _, Context, Entity, EventEmitter, Focusable,
    FocusHandle, IntoElement, Pixels, Render, WeakEntity, Window,
};
use mnemonic_core::events;
use mnemonic_core::services::terminal_service;
use ui::{h_flex, v_flex, Button, ButtonStyle, Label, prelude::*};
use workspace::dock::{DockPosition, PanelEvent};
use workspace::{Panel, Workspace};

actions!(mnemonic_command_panel, [ToggleFocus]);

const COMMAND_PANEL_KEY: &str = "MnemonicCommandPanel";

#[derive(Clone)]
struct PendingApproval {
    session_id: String,
    _workspace_id: String,
    command: String,
}

#[derive(Clone)]
struct PendingDecision {
    session_id: String,
    _workspace_id: String,
    question: String,
    options: Vec<(String, String)>,
}

pub struct CommandApprovalPanel {
    focus_handle: FocusHandle,
    _workspace: WeakEntity<Workspace>,
    pending_approvals: Vec<PendingApproval>,
    pending_decisions: Vec<PendingDecision>,
}

impl CommandApprovalPanel {
    pub fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let panel = cx.new(|cx| {
            let this = Self {
                focus_handle: cx.focus_handle(),
                _workspace: workspace.weak_handle(),
                pending_approvals: Vec::new(),
                pending_decisions: Vec::new(),
            };
            this
        });

        let event_hub = mnemonic_bridge::mnemonic_event_hub(cx);
        cx.subscribe(&event_hub, {
            let panel = panel.downgrade();
            move |_workspace, _hub, event: &mnemonic_bridge::MnemonicEvent, cx| {
                if let Some(panel) = panel.upgrade() {
                    panel.update(cx, |this, cx| {
                        this.handle_event(event);
                        cx.notify();
                    });
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

    fn handle_event(&mut self, event: &mnemonic_bridge::MnemonicEvent) {
        match event.name.as_str() {
            events::COMMAND_APPROVAL_REQUIRED => {
                if let Ok(ev) =
                    serde_json::from_value::<mnemonic_core::models::CommandApprovalEvent>(
                        event.payload.clone(),
                    )
                {
                    self.pending_approvals.push(PendingApproval {
                        session_id: ev.session_id,
                        _workspace_id: ev.workspace_id,
                        command: ev.command,
                    });
                }
            }
            events::AGENT_DECISION_REQUIRED => {
                if let Ok(ev) =
                    serde_json::from_value::<mnemonic_core::models::AgentDecisionEvent>(
                        event.payload.clone(),
                    )
                {
                    self.pending_decisions.push(PendingDecision {
                        session_id: ev.session_id,
                        _workspace_id: ev.workspace_id,
                        question: ev.question,
                        options: ev
                            .options
                            .into_iter()
                            .map(|o| (o.key, o.label))
                            .collect(),
                    });
                }
            }
            events::AGENT_DECISION_RESOLVED => {
                if let Some(session_id) = event.payload.get("sessionId").and_then(|v| v.as_str()) {
                    self.pending_decisions
                        .retain(|d| d.session_id != session_id);
                }
            }
            _ => {}
        }
    }

    fn approve_command(&mut self, index: usize, approved: bool, cx: &App) {
        if let Some(approval) = self.pending_approvals.get(index) {
            let state = mnemonic_bridge::mnemonic_state(cx);
            terminal_service::approve_workspace_terminal_command(
                &state,
                &approval.session_id,
                approved,
            )
            .ok();
        }
        if index < self.pending_approvals.len() {
            self.pending_approvals.remove(index);
        }
    }

    fn answer_decision(&mut self, decision_idx: usize, key: &str, label: &str, cx: &App) {
        if let Some(decision) = self.pending_decisions.get(decision_idx) {
            let state = mnemonic_bridge::mnemonic_state(cx);
            terminal_service::answer_workspace_terminal_decision(
                &state,
                &decision.session_id,
                key,
                label,
            )
            .ok();
        }
        if decision_idx < self.pending_decisions.len() {
            self.pending_decisions.remove(decision_idx);
        }
    }

    fn render_approval(
        &self,
        index: usize,
        approval: &PendingApproval,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let idx = index;
        v_flex()
            .px_2()
            .py(px(6.))
            .gap_1()
            .w_full()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .w(px(6.))
                            .h(px(6.))
                            .rounded_full()
                            .bg(cx.theme().status().warning),
                    )
                    .child(
                        Label::new("Command approval")
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        Label::new(approval.command.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Default),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new(format!("allow-{idx}"), "Allow")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.approve_command(idx, true, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(format!("deny-{idx}"), "Deny")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.approve_command(idx, false, cx);
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_decision(
        &self,
        index: usize,
        decision: &PendingDecision,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let options: Vec<_> = decision
            .options
            .iter()
            .enumerate()
            .map(|(_, (key, label))| {
                let k = key.clone();
                let l = label.clone();
                let idx = index;
                Button::new(format!("opt-{index}-{key}"), label.clone())
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.answer_decision(idx, &k, &l, cx);
                        cx.notify();
                    }))
            })
            .collect();

        v_flex()
            .px_2()
            .py(px(6.))
            .gap_1()
            .w_full()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .w(px(6.))
                            .h(px(6.))
                            .rounded_full()
                            .bg(cx.theme().status().info),
                    )
                    .child(
                        Label::new("Agent decision")
                            .size(LabelSize::Small)
                            .color(Color::Accent),
                    ),
            )
            .child(
                Label::new(decision.question.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Default),
            )
            .child(h_flex().gap_1().children(options))
    }
}

impl Focusable for CommandApprovalPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for CommandApprovalPanel {}

impl Panel for CommandApprovalPanel {
    fn persistent_name() -> &'static str {
        "MnemonicCommandPanel"
    }

    fn panel_key() -> &'static str {
        COMMAND_PANEL_KEY
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
        px(300.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::BellDot)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Command Approvals")
    }

    fn icon_label(&self, _window: &Window, _cx: &App) -> Option<String> {
        let pending = self.pending_approvals.len() + self.pending_decisions.len();
        if pending > 0 {
            Some(pending.to_string())
        } else {
            None
        }
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        5
    }
}

impl Render for CommandApprovalPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_items = !self.pending_approvals.is_empty() || !self.pending_decisions.is_empty();

        let mut content = v_flex()
            .id("command-approval-panel")
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
                        Label::new("Approvals")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            );

        if has_items {
            let approvals = self.pending_approvals.clone();
            let decisions = self.pending_decisions.clone();

            let approval_rows: Vec<_> = approvals
                .iter()
                .enumerate()
                .map(|(i, a)| self.render_approval(i, a, cx))
                .collect();

            let decision_rows: Vec<_> = decisions
                .iter()
                .enumerate()
                .map(|(i, d)| self.render_decision(i, d, cx))
                .collect();

            content = content.child(
                v_flex()
                    .py_1()
                    .overflow_y_hidden()
                    .flex_grow(1.0)
                    .children(approval_rows)
                    .children(decision_rows),
            );
        } else {
            content = content.child(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .child(
                        Label::new("No pending approvals")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            );
        }

        content
    }
}
