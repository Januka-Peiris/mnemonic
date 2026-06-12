mod command_approval_panel;
mod coordinator_panel;
mod workspace_detail_panel;
mod workspace_panel;

pub use command_approval_panel::CommandApprovalPanel;
pub use coordinator_panel::CoordinatorPanel;
pub use workspace_detail_panel::WorkspaceDetailPanel;
pub use workspace_panel::WorkspacePanel;

/// The workspace currently selected in the left panel. Set by
/// [`WorkspacePanel`] and observed by panels that show per-workspace state.
pub struct ActiveWorkspace(pub Option<String>);

impl gpui::Global for ActiveWorkspace {}
