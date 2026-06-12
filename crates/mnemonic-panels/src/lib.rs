mod command_approval_panel;
mod coordinator_panel;
mod workspace_detail_panel;
mod workspace_panel;
mod workspace_view;

pub use command_approval_panel::CommandApprovalPanel;
pub use command_approval_panel::ToggleFocus as ToggleCommandApprovalFocus;
pub use coordinator_panel::CoordinatorPanel;
pub use coordinator_panel::ToggleFocus as ToggleCoordinatorFocus;
pub use workspace_detail_panel::ToggleFocus as ToggleWorkspaceDetailFocus;
pub use workspace_detail_panel::WorkspaceDetailPanel;
pub use workspace_panel::ToggleFocus as ToggleWorkspacePanelFocus;
pub use workspace_panel::WorkspacePanel;
pub use workspace_view::{WorkspaceHost, WorkspaceView};

/// The workspace currently selected in the left panel. Set by
/// [`WorkspacePanel`] and observed by panels that show per-workspace state.
pub struct ActiveWorkspace(pub Option<String>);

impl gpui::Global for ActiveWorkspace {}
