use gpui::{
    Action, App, AppContext as _, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    IntoElement, Pixels, Render, Subscription, WeakEntity, Window, actions, px,
};
use mnemonic_bridge::ActiveWorkspace;
use ui::{Label, prelude::*, v_flex};
use workspace::dock::{DockPosition, PanelEvent};
use workspace::{Panel, Workspace};

use crate::workspace_detail_panel::WorkspaceDetailPanel;
use crate::workspace_view::{WorkspaceHost, WorkspaceView};

actions!(mnemonic_tools_panel, [ToggleFocus]);

/// Setup, Run and Terminal for the selected workspace, stacked under the
/// right panel.
pub struct WorkspaceToolsPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    /// The workspace view being shown and the subscription that redraws
    /// this panel when it changes.
    observed: Option<(EntityId, Subscription)>,
    _active_workspace: Subscription,
}

impl WorkspaceToolsPanel {
    pub fn new(workspace: &Workspace, cx: &mut Context<Workspace>) -> Entity<Self> {
        let workspace = workspace.weak_handle();
        cx.new(|cx| Self {
            focus_handle: cx.focus_handle(),
            workspace,
            observed: None,
            _active_workspace: cx.observe_global::<ActiveWorkspace>(|_, cx| cx.notify()),
        })
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _window, cx| Self::new(workspace, cx))
    }

    fn selected_view(&self, cx: &App) -> Option<Entity<WorkspaceView>> {
        let workspace = self.workspace.upgrade()?;
        let workspace_id = ActiveWorkspace::get(workspace.entity_id(), cx)?;
        WorkspaceHost::view_in(workspace.read(cx), &workspace_id, cx)
    }
}

/// Puts the panel under the right panel. With `open` it also opens the
/// right dock; without, it only stacks when the right panel is showing.
pub fn show_tools_panel(
    workspace: &mut Workspace,
    open: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.right_dock().update(cx, |dock, cx| {
        let Some(index) = dock.panel_index_for_type::<WorkspaceToolsPanel>() else {
            return;
        };
        let stacked = dock
            .stacked_panel()
            .is_some_and(|panel| panel.to_any().downcast::<WorkspaceToolsPanel>().is_ok());
        let detail_showing = dock
            .active_panel()
            .is_some_and(|panel| panel.to_any().downcast::<WorkspaceDetailPanel>().is_ok());
        if !stacked && open && !detail_showing {
            // It only stacks under the right panel, so that comes back first.
            if let Some(detail_index) = dock.panel_index_for_type::<WorkspaceDetailPanel>() {
                dock.activate_panel(detail_index, window, cx);
            }
        }
        let detail_showing = detail_showing || open;
        if !stacked && detail_showing && (open || dock.is_open()) {
            dock.activate_panel(index, window, cx);
        }
        if open && !dock.is_open() {
            dock.set_open(true, window, cx);
        }
    });
}

/// The dock button: hides the panel when it's stacked under the right
/// panel, and shows it there otherwise.
pub fn toggle_tools_panel(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let stacked = workspace
        .right_dock()
        .read(cx)
        .stacked_panel()
        .is_some_and(|panel| panel.to_any().downcast::<WorkspaceToolsPanel>().is_ok());
    if stacked {
        workspace.right_dock().update(cx, |dock, cx| dock.close_stacked_panel(window, cx));
    } else {
        show_tools_panel(workspace, true, window, cx);
    }
}

impl Focusable for WorkspaceToolsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorkspaceToolsPanel {}

impl Panel for WorkspaceToolsPanel {
    fn persistent_name() -> &'static str {
        "MnemonicToolsPanel"
    }

    fn panel_key() -> &'static str {
        "MnemonicToolsPanel"
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
        px(360.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Terminal)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Setup, Run and Terminal")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        6
    }

    fn stacks_below_other_panels(&self, _cx: &App) -> bool {
        true
    }
}

impl Render for WorkspaceToolsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.selected_view(cx);
        let view_id = view.as_ref().map(|view| view.entity_id());
        if self.observed.as_ref().map(|(id, _)| *id) != view_id {
            self.observed = view
                .as_ref()
                .map(|view| (view.entity_id(), cx.observe(view, |_, _, cx| cx.notify())));
        }
        let body = match view {
            Some(view) => view.update(cx, |view, cx| view.render_tools(cx)),
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    Label::new("Select a workspace")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().panel_background)
            .child(body)
    }
}
