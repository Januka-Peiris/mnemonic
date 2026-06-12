use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{mpsc, Arc, Mutex};

use portable_pty::{ChildKiller, MasterPty};

use std::time::{SystemTime, UNIX_EPOCH};

use crate::db::Database;
use crate::models::OrchestratorAction;
use crate::repositories::{
    activity_repository, agent_run_repository, settings_repository, terminal_repository,
};

pub type TerminalRegistry = Arc<Mutex<HashMap<String, Arc<ActiveTerminal>>>>;
/// Maps session_id → raw PTY input bytes that are pending user approval.
pub type PendingCommandRegistry = Arc<Mutex<HashMap<String, String>>>;
pub type CoordinatorStepRegistry = Arc<Mutex<HashSet<String>>>;
pub type SchedulerJobRegistry = Arc<Mutex<HashSet<String>>>;
pub type RepoIntelligenceRegistry = Arc<Mutex<HashSet<String>>>;
/// Maps session_id to a list of WS writer channels for live terminal output.
pub type WsConnectionRegistry = Arc<Mutex<HashMap<String, Vec<mpsc::SyncSender<Vec<u8>>>>>>;

pub trait AppEventEmitter: Send + Sync + 'static {
    fn emit(&self, event: &str, payload: &serde_json::Value);
}

#[derive(Clone, Default)]
pub struct NoopEventEmitter;

impl AppEventEmitter for NoopEventEmitter {
    fn emit(&self, _event: &str, _payload: &serde_json::Value) {}
}

pub struct ActiveTerminal {
    pub session_id: String,
    pub terminal_kind: String,
    pub writer: Mutex<Box<dyn Write + Send>>,
    pub killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pub master: Mutex<Box<dyn MasterPty + Send>>,
    /// Unix timestamp in seconds of the last output received from the PTY.
    /// 0 means no output yet. Updated atomically by the reader thread.
    pub last_output_at_secs: Arc<AtomicU64>,
    /// Same signal in milliseconds, for sub-second readiness checks before
    /// dispatching prompts into a freshly spawned TUI.
    pub last_output_at_millis: Arc<AtomicU64>,
    /// Monotonically increasing sequence counter shared between the reader thread
    /// and any system log writes so all chunks for this session have globally
    /// ordered, non-colliding sequence numbers.
    pub seq_counter: Arc<AtomicU64>,
}

#[derive(Clone)]
pub struct AppState {
    pub event_emitter: Arc<dyn AppEventEmitter>,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub db: Database,
    pub terminals: TerminalRegistry,
    pub pending_commands: PendingCommandRegistry,
    /// Whether the Opus orchestrator loop is running.
    pub orchestrator_enabled: Arc<AtomicBool>,
    /// Model used for orchestrator decisions (e.g. "claude-opus-4-6").
    pub orchestrator_model: Arc<Mutex<String>>,
    /// Timestamp (unix secs) of the last orchestrator pass.
    pub orchestrator_last_run: Arc<Mutex<Option<String>>>,
    /// Actions taken in the most recent orchestrator pass.
    pub orchestrator_last_actions: Arc<Mutex<Vec<OrchestratorAction>>>,
    /// Workspace ids with an in-flight coordinator step.
    pub coordinator_step_inflight: CoordinatorStepRegistry,
    /// Single-flight key set for scheduler jobs (`workspace_id:job_kind`).
    pub scheduler_job_inflight: SchedulerJobRegistry,
    /// Single-flight key set for per-repo intelligence refresh jobs (`repo_id`).
    pub repo_intelligence_inflight: RepoIntelligenceRegistry,
    /// Port the local WebSocket server is listening on for terminal I/O.
    pub ws_port: Arc<Mutex<u16>>,
    /// Auth token for WebSocket connections.
    pub ws_token: Arc<Mutex<String>>,
    /// Active WebSocket writer channels per terminal session.
    pub ws_connections: WsConnectionRegistry,
}

impl AppState {
    pub fn initialize(
        data_dir: PathBuf,
        cache_dir: PathBuf,
        event_emitter: Arc<dyn AppEventEmitter>,
    ) -> Result<Self, String> {
        let db = Database::initialize(&data_dir)?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_string());
        let abandoned_run_groups = agent_run_repository::list_running_run_groups(&db)?;
        agent_run_repository::mark_stale_running_abandoned(&db, &now)?;
        for group in abandoned_run_groups {
            let details = format!(
                "{} running agent run(s) were marked abandoned after app restart; logs were preserved.",
                group.count
            );
            let _ = activity_repository::record(
                &db,
                &group.workspace_id,
                &group.repo,
                Some(&group.branch),
                "Startup agent-run reconciliation",
                "warning",
                Some(&details),
            );
        }
        let stale_terminal_groups = terminal_repository::list_running_session_groups(&db)?;
        terminal_repository::mark_stale_running_sessions(&db, &now)?;
        terminal_repository::mark_all_queued_prompts_stale(&db)?;
        for group in stale_terminal_groups {
            let details = format!(
                "{} running terminal session(s) were marked stale after app restart; history was preserved.",
                group.count
            );
            let _ = activity_repository::record(
                &db,
                &group.workspace_id,
                &group.repo,
                Some(&group.branch),
                "Startup session reconciliation",
                "warning",
                Some(&details),
            );
        }
        let state = Self {
            event_emitter,
            data_dir,
            cache_dir,
            db,
            terminals: Arc::new(Mutex::new(HashMap::new())),
            pending_commands: Arc::new(Mutex::new(HashMap::new())),
            orchestrator_enabled: Arc::new(AtomicBool::new(false)),
            orchestrator_model: Arc::new(Mutex::new("claude-opus-4-6".to_string())),
            orchestrator_last_run: Arc::new(Mutex::new(None)),
            orchestrator_last_actions: Arc::new(Mutex::new(Vec::new())),
            coordinator_step_inflight: Arc::new(Mutex::new(HashSet::new())),
            scheduler_job_inflight: Arc::new(Mutex::new(HashSet::new())),
            repo_intelligence_inflight: Arc::new(Mutex::new(HashSet::new())),
            ws_port: Arc::new(Mutex::new(0)),
            ws_token: Arc::new(Mutex::new(String::new())),
            ws_connections: Arc::new(Mutex::new(HashMap::new())),
        };
        let _ =
            settings_repository::ensure_default_value(&state.db, "notifications_min_level", "info");
        let _ = settings_repository::ensure_default_value(
            &state.db,
            "notifications_dedupe_seconds",
            "30",
        );
        let _ = settings_repository::ensure_default_value(
            &state.db,
            "repo_intelligence_enabled",
            "false",
        );
        Ok(state)
    }
}
