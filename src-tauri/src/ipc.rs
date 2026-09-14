//! Tauri commands the panel calls over IPC.

use shepherd_core::registry::{Registry, Snapshot};
use shepherd_core::store::Store;
use shepherd_core::Control;
use std::sync::Arc;
use tauri::{AppHandle, Manager, State};

pub struct Ipc {
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
}

#[tauri::command]
pub fn get_state(state: State<Ipc>) -> Snapshot {
    state.registry.snapshot()
}

#[tauri::command]
pub fn send_control(state: State<Ipc>, session_id: String, control: Control) -> Result<(), String> {
    state
        .registry
        .handle_control(&session_id, control)
        .then_some(())
        .ok_or_else(|| format!("unknown session {session_id}"))
}

#[tauri::command]
pub fn set_muted(state: State<Ipc>, muted: bool) {
    state.registry.set_muted(muted);
    if let Err(e) = state.store.set_muted(muted) {
        eprintln!("shepherd: persist mute failed: {e}");
    }
}

#[tauri::command]
pub fn hide_panel(app: AppHandle) {
    if let Some(w) = app.get_webview_window("panel") {
        let _ = w.hide();
    }
}
