use std::sync::{mpsc, Arc};

use futures::StreamExt;
use gpui::{actions, App, AppContext as _, Entity, EventEmitter, Global};
use mnemonic_core::state::{AppEventEmitter, AppState};

actions!(
    mnemonic,
    [
        /// Ask claude to create a pull request for the active workspace.
        CreatePullRequest,
        /// Create a pull request and arm the CI auto-fix loop.
        CreatePullRequestWithAutoFix
    ]
);

/// The workspace currently selected in the left panel. Set by the workspace
/// panel; observed by panels and the title bar.
pub struct ActiveWorkspace(pub Option<String>);

impl Global for ActiveWorkspace {}

/// Last terminal output time per workspace's agent, written by workspace
/// views and read by the left panel for its attention indicators. A plain
/// static (not a gpui Global) so high-frequency writes don't trigger
/// observers.
static AGENT_ACTIVITY: std::sync::Mutex<
    Option<std::collections::HashMap<String, std::time::Instant>>,
> = std::sync::Mutex::new(None);

pub fn record_agent_activity(workspace_id: &str) {
    let mut guard = AGENT_ACTIVITY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .get_or_insert_with(Default::default)
        .insert(workspace_id.to_string(), std::time::Instant::now());
}

pub fn last_agent_activity(workspace_id: &str) -> Option<std::time::Instant> {
    let guard = AGENT_ACTIVITY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .as_ref()
        .and_then(|map| map.get(workspace_id).copied())
}

pub fn clear_agent_activity(workspace_id: &str) {
    let mut guard = AGENT_ACTIVITY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(map) = guard.as_mut() {
        map.remove(workspace_id);
    }
}

/// Something an agent did in a workspace while nobody was looking at it,
/// shown on the workspace's sidebar row until the user opens it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentAttention {
    /// A turn finished, or the agent is waiting for permission.
    NeedsAttention,
    /// The agent stopped with an error or refused.
    Error,
}

static AGENT_ATTENTION: std::sync::Mutex<
    Option<std::collections::HashMap<String, AgentAttention>>,
> = std::sync::Mutex::new(None);

pub fn set_agent_attention(workspace_id: &str, attention: AgentAttention) {
    let mut guard = AGENT_ATTENTION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .get_or_insert_with(Default::default)
        .insert(workspace_id.to_string(), attention);
}

pub fn agent_attention(workspace_id: &str) -> Option<AgentAttention> {
    let guard = AGENT_ATTENTION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .as_ref()
        .and_then(|map| map.get(workspace_id).copied())
}

pub fn clear_agent_attention(workspace_id: &str) {
    let mut guard = AGENT_ATTENTION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(map) = guard.as_mut() {
        map.remove(workspace_id);
    }
}

/// Last observed PR state per workspace ("open", "ready", "conflicts",
/// "failed", "merged"), written by whichever component polls GitHub and read
/// by the left panel's git icons.
static PR_STATES: std::sync::Mutex<Option<std::collections::HashMap<String, String>>> =
    std::sync::Mutex::new(None);

pub fn record_pr_state(workspace_id: &str, state: &str) {
    let mut guard = PR_STATES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .get_or_insert_with(Default::default)
        .insert(workspace_id.to_string(), state.to_string());
}

pub fn pr_state(workspace_id: &str) -> Option<String> {
    let guard = PR_STATES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.as_ref().and_then(|map| map.get(workspace_id).cloned())
}

pub fn clear_pr_state(workspace_id: &str) {
    let mut guard = PR_STATES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(map) = guard.as_mut() {
        map.remove(workspace_id);
    }
}

/// Classifies a GitHub PR status into the small state vocabulary used by
/// the left panel's git icons, and records it.
pub fn record_pr_status(
    workspace_id: &str,
    status: &mnemonic_core::models::WorkspacePrStatus,
) {
    let state = if !status.found {
        None
    } else if status.state.as_deref() == Some("MERGED") {
        Some("merged")
    } else if status.mergeable.as_deref() == Some("CONFLICTING")
        || status.merge_state_status.as_deref() == Some("DIRTY")
    {
        Some("conflicts")
    } else if status.checks.iter().any(|check| {
        matches!(
            check.conclusion.as_deref(),
            Some("FAILURE") | Some("TIMED_OUT") | Some("CANCELLED") | Some("ACTION_REQUIRED")
        )
    }) {
        Some("failed")
    } else if status.merge_state_status.as_deref() == Some("CLEAN") {
        Some("ready")
    } else {
        Some("open")
    };

    match state {
        Some(state) => record_pr_state(workspace_id, state),
        None => clear_pr_state(workspace_id),
    }
}

pub struct MnemonicEvent {
    pub name: String,
    pub payload: serde_json::Value,
}

struct ChannelEvent {
    name: String,
    payload: serde_json::Value,
}

struct ZedEventEmitter {
    sender: mpsc::SyncSender<ChannelEvent>,
}

impl AppEventEmitter for ZedEventEmitter {
    fn emit(&self, event: &str, payload: &serde_json::Value) {
        let _ = self.sender.try_send(ChannelEvent {
            name: event.to_string(),
            payload: payload.clone(),
        });
    }
}

pub struct MnemonicEventHub;

impl EventEmitter<MnemonicEvent> for MnemonicEventHub {}

struct GlobalMnemonicState {
    state: AppState,
    event_hub: Entity<MnemonicEventHub>,
}

impl Global for GlobalMnemonicState {}

pub fn mnemonic_state(cx: &App) -> AppState {
    cx.global::<GlobalMnemonicState>().state.clone()
}

pub fn mnemonic_state_cloned() -> Option<AppState> {
    MNEMONIC_STATE.lock().ok().and_then(|g| g.clone())
}

pub fn mnemonic_event_hub(cx: &App) -> Entity<MnemonicEventHub> {
    cx.global::<GlobalMnemonicState>().event_hub.clone()
}

static MNEMONIC_STATE: std::sync::Mutex<Option<AppState>> = std::sync::Mutex::new(None);

pub fn init(cx: &mut App) {
    let data_dir = paths::data_dir().clone();
    let cache_dir = paths::data_dir().join("cache");

    let (sender, receiver) = mpsc::sync_channel::<ChannelEvent>(256);
    let emitter = Arc::new(ZedEventEmitter { sender });

    let state = match AppState::initialize(data_dir, cache_dir, emitter) {
        Ok(s) => s,
        Err(e) => {
            log::error!("Failed to initialize mnemonic-core: {e}");
            return;
        }
    };

    let event_hub = cx.new(|_| MnemonicEventHub);

    if let Ok(mut guard) = MNEMONIC_STATE.lock() {
        *guard = Some(state.clone());
    }

    cx.set_global(GlobalMnemonicState {
        state,
        event_hub: event_hub.clone(),
    });

    cx.spawn(async move |cx| {
        let (async_tx, mut async_rx) = futures::channel::mpsc::unbounded::<ChannelEvent>();

        std::thread::Builder::new()
            .name("mnemonic-event-reader".into())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    if async_tx.unbounded_send(event).is_err() {
                        break;
                    }
                }
            })
            .ok();

        while let Some(event) = async_rx.next().await {
            let hub = event_hub.clone();
            cx.update(|cx| {
                hub.update(cx, |_, cx| {
                    cx.emit(MnemonicEvent {
                        name: event.name,
                        payload: event.payload,
                    });
                });
            });
        }
    })
    .detach();

    log::info!("mnemonic-core initialized");
}

