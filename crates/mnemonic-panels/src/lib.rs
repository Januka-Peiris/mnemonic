mod command_approval_panel;
mod coordinator_panel;
mod team;
mod team_settings;
mod team_view;
mod workspace_detail_panel;
mod workspace_panel;
mod workspace_view;
mod workspaces_settings;

pub use command_approval_panel::CommandApprovalPanel;
pub use command_approval_panel::ToggleFocus as ToggleCommandApprovalFocus;
pub use coordinator_panel::CoordinatorPanel;
pub use coordinator_panel::ToggleFocus as ToggleCoordinatorFocus;
pub use workspace_detail_panel::ToggleFocus as ToggleWorkspaceDetailFocus;
pub use workspace_detail_panel::WorkspaceDetailPanel;
pub use workspace_panel::ToggleFocus as ToggleWorkspacePanelFocus;
pub use workspace_panel::WorkspacePanel;
pub use workspace_panel::{SelectNextWorkspace, SelectPreviousWorkspace};
pub use workspace_view::{WorkspaceHost, WorkspaceView, is_session_tab};
pub use team::TeamSettings;
pub use workspaces_settings::WorkspacesSettings;

pub fn init(cx: &mut gpui::App) {
    workspaces_settings::init(cx);
    team::init(cx);
}

pub use mnemonic_bridge::ActiveWorkspace;
