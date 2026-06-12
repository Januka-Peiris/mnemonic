use std::sync::{mpsc, Arc};

use futures::StreamExt;
use gpui::{App, AppContext as _, Entity, EventEmitter, Global};
use mnemonic_core::state::{AppEventEmitter, AppState};

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

    seed_sample_workspaces(&mnemonic_state(cx));

    log::info!("mnemonic-core initialized");
}

fn seed_sample_workspaces(state: &AppState) {
    use mnemonic_core::models::{
        AgentSessionSummary, BranchHealth, WorkspaceDetail, WorkspaceSummary,
    };
    use mnemonic_core::repositories::workspace_repository;
    use mnemonic_core::services::workspace_service;

    if let Ok(existing) = workspace_service::list_workspaces(state) {
        if !existing.is_empty() {
            return;
        }
    }

    let samples = vec![
        ("API rate limiter", "mnemonic", "feat/rate-limiter", "Active", "Claude Code", "running"),
        ("Fix auth redirect", "mnemonic", "fix/auth-redirect", "Active", "Claude Code", "idle"),
        ("DB migration v42", "mnemonic", "feat/migration-v42", "Merged", "", ""),
        ("AWS SDK auth fix", "axe-agent-server", "fix/aws-auth", "Active", "Claude Code", "running"),
        ("Add webhook retries", "axe-agent-server", "feat/webhook-retries", "Active", "", ""),
        ("Update onboarding flow", "axe-logistics", "feat/onboarding-v2", "Active", "Claude Code", "idle"),
    ];

    for (name, repo, branch, status, agent, agent_status) in samples {
        let id = format!("ws-seed-{}", name.replace(' ', "-").to_lowercase());
        let detail = WorkspaceDetail {
            summary: WorkspaceSummary {
                id: id.clone(),
                name: name.to_string(),
                repo: repo.to_string(),
                branch: branch.to_string(),
                agent: agent.to_string(),
                status: status.to_string(),
                current_step: String::new(),
                completed_steps: Vec::new(),
                changed_files: Vec::new(),
                last_updated: String::new(),
                pr_status: None,
                pr_number: None,
                description: String::new(),
                current_task: String::new(),
                branch_health: BranchHealth {
                    ahead_by: 0,
                    behind_by: 0,
                    merge_risk: "low".to_string(),
                    last_rebase: String::new(),
                    base_branch: "main".to_string(),
                },
                agent_session: AgentSessionSummary {
                    id: String::new(),
                    agent: agent.to_string(),
                    status: agent_status.to_string(),
                    model: "claude-opus-4-6".to_string(),
                    token_count: 0,
                    estimated_cost: "0.00".to_string(),
                    last_message: String::new(),
                    started_at: String::new(),
                },
                repository_id: None,
                repository_path: None,
                selected_branch: None,
                selected_worktree_id: None,
                selected_worktree_path: None,
                workspace_root_path: None,
                worktree_managed_by_forge: false,
                workspace_source: "seed".to_string(),
                parent_workspace_id: None,
                source_workspace_id: None,
                derived_from_branch: None,
                linked_worktrees: Vec::new(),
                cost_limit_usd: None,
                run_tests_on_create: false,
                create_pr_on_complete: true,
            },
            worktree_path: String::new(),
            base_branch: "main".to_string(),
            recent_events: Vec::new(),
        };
        workspace_repository::insert(&state.db, &detail).ok();
    }

    log::info!("seeded 3 sample workspaces");
}

