mod command_approval_panel;
mod team;
mod team_connection;
mod team_copy;
mod team_live_chat;
mod team_settings;
mod team_workspace_view;
mod workspace_detail_panel;
mod workspace_panel;
mod workspace_tools_panel;
mod workspace_view;
mod workspaces_settings;

pub use command_approval_panel::CommandApprovalPanel;
pub use command_approval_panel::ToggleFocus as ToggleCommandApprovalFocus;
pub use workspace_detail_panel::ToggleFocus as ToggleWorkspaceDetailFocus;
pub use workspace_detail_panel::WorkspaceDetailPanel;
pub use workspace_panel::ToggleFocus as ToggleWorkspacePanelFocus;
pub use workspace_panel::WorkspacePanel;
pub use workspace_tools_panel::ToggleFocus as ToggleWorkspaceToolsFocus;
pub use workspace_tools_panel::{WorkspaceToolsPanel, show_tools_panel, toggle_tools_panel};
pub use workspace_panel::{SelectNextWorkspace, SelectPreviousWorkspace};
pub use workspace_view::{SessionPaneToolbar, WorkspaceHost, WorkspaceView, is_session_tab};
pub use team::TeamSettings;
pub use workspaces_settings::WorkspacesSettings;

pub fn init(cx: &mut gpui::App) {
    workspaces_settings::init(cx);
    team::init(cx);
}

pub use mnemonic_bridge::ActiveWorkspace;
