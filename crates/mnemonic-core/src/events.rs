//! Stable frontend event names emitted by Mnemonic backend services.
//!
//! Keeping these names centralized protects both the existing Tauri frontend
//! and the GPUI frontend from accidental string drift while sharing
//! `mnemonic-core`.

pub const TERMINAL_OUTPUT: &str = "mn://terminal-output";
pub const COMMAND_APPROVAL_REQUIRED: &str = "mn://command-approval-required";
pub const AGENT_MODE_CHANGED: &str = "mn://agent-mode-changed";
pub const AGENT_DECISION_REQUIRED: &str = "mn://agent-decision-required";
pub const AGENT_DECISION_RESOLVED: &str = "mn://agent-decision-resolved";
pub const COORDINATOR_NOTIFY: &str = "mn://coordinator-notify";
pub const ORCHESTRATOR_NOTIFY: &str = "mn://orchestrator-notify";
pub const WORKSPACE_REBASE_CONFLICT: &str = "mn://workspace-rebase-conflict";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_event_names_remain_stable() {
        assert_eq!(TERMINAL_OUTPUT, "mn://terminal-output");
        assert_eq!(COMMAND_APPROVAL_REQUIRED, "mn://command-approval-required");
        assert_eq!(AGENT_MODE_CHANGED, "mn://agent-mode-changed");
        assert_eq!(AGENT_DECISION_REQUIRED, "mn://agent-decision-required");
        assert_eq!(AGENT_DECISION_RESOLVED, "mn://agent-decision-resolved");
        assert_eq!(COORDINATOR_NOTIFY, "mn://coordinator-notify");
        assert_eq!(ORCHESTRATOR_NOTIFY, "mn://orchestrator-notify");
        assert_eq!(WORKSPACE_REBASE_CONFLICT, "mn://workspace-rebase-conflict");
    }
}
