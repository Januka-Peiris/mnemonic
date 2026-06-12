use std::sync::atomic::Ordering;

use crate::repositories::orchestrator_repository;
use crate::services::{coordinator_service, orchestrator_service, repo_intelligence_service};
use crate::state::AppState;

#[derive(Clone, Copy, Debug)]
pub struct BackgroundServiceOptions {
    /// Start the v2 terminal WebSocket bridge. GPUI v3 visible terminals should
    /// keep this disabled because they render directly from Zed's terminal PTY.
    pub terminal_ws: bool,
    pub orchestrator: bool,
    pub repo_intelligence: bool,
    pub prune_old_data: bool,
    pub reconcile_coordinators: bool,
}

impl BackgroundServiceOptions {
    /// Runtime defaults for GPUI v3.
    ///
    /// Visible terminals in GPUI use Zed's PTY/rendering path directly, so the
    /// legacy v2 terminal WebSocket bridge must stay disabled. Background
    /// coordination/repo-intelligence services remain enabled.
    pub fn gpui_visible_terminal() -> Self {
        Self {
            terminal_ws: false,
            ..Self::default()
        }
    }

    /// Runtime defaults for the existing Tauri v2 application.
    ///
    /// Tauri v2 still renders terminal output through the terminal WebSocket
    /// bridge, so keep that bridge enabled there.
    pub fn tauri_v2() -> Self {
        Self {
            terminal_ws: true,
            ..Self::default()
        }
    }
}

impl Default for BackgroundServiceOptions {
    fn default() -> Self {
        Self {
            terminal_ws: false,
            orchestrator: true,
            repo_intelligence: true,
            prune_old_data: true,
            reconcile_coordinators: true,
        }
    }
}

pub fn start_background_services(
    state: &AppState,
    options: BackgroundServiceOptions,
) -> Result<(), String> {
    if options.prune_old_data {
        let bg_db = state.db.clone();
        std::thread::spawn(move || {
            if let Err(err) = bg_db.prune_old_data() {
                log::warn!(target: "mnemonic_lib", "Background prune failed: {err}");
            }
        });
    }

    restore_orchestrator_settings(state);

    if options.reconcile_coordinators {
        if let Err(error) = coordinator_service::reconcile_all_active_runs_on_startup(state) {
            log::warn!(target: "mnemonic_lib", "Failed to reconcile active coordinator runs on startup: {error}");
        }
    }

    if options.terminal_ws {
        start_terminal_ws_bridge(state)?;
    }

    if options.orchestrator {
        orchestrator_service::start_orchestrator_loop(state.clone());
    }
    if options.repo_intelligence {
        repo_intelligence_service::start_repo_intelligence_loop(state.clone());
    }

    Ok(())
}

fn restore_orchestrator_settings(state: &AppState) {
    if let Ok(Some(val)) = orchestrator_repository::load_setting(&state.db, "orchestrator_enabled")
    {
        state
            .orchestrator_enabled
            .store(val == "true", Ordering::Relaxed);
    }
    if let Ok(Some(model)) = orchestrator_repository::load_setting(&state.db, "orchestrator_model")
    {
        if let Ok(mut guard) = state.orchestrator_model.lock() {
            *guard = model;
        }
    }
}

fn start_terminal_ws_bridge(state: &AppState) -> Result<(), String> {
    match crate::services::terminal_ws_server::start_ws_server(state.clone()) {
        Ok((port, token)) => {
            *state
                .ws_port
                .lock()
                .map_err(|_| "WS port lock poisoned".to_string())? = port;
            *state
                .ws_token
                .lock()
                .map_err(|_| "WS token lock poisoned".to_string())? = token;
        }
        Err(error) => {
            log::warn!(target: "mnemonic_lib", "Failed to start terminal WS server: {error}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpui_visible_terminal_runtime_disables_legacy_terminal_ws() {
        let options = BackgroundServiceOptions::gpui_visible_terminal();
        assert!(!options.terminal_ws);
        assert!(options.orchestrator);
        assert!(options.repo_intelligence);
        assert!(options.prune_old_data);
        assert!(options.reconcile_coordinators);
    }

    #[test]
    fn tauri_v2_runtime_keeps_terminal_ws_enabled() {
        let options = BackgroundServiceOptions::tauri_v2();
        assert!(options.terminal_ws);
        assert!(options.orchestrator);
        assert!(options.repo_intelligence);
        assert!(options.prune_old_data);
        assert!(options.reconcile_coordinators);
    }
}
