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

    log::info!("mnemonic-core initialized");
}

