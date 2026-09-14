//! Tauri commands the panel calls over IPC.

use shepherd_core::cc::install;
use shepherd_core::config::ShepherdConfig;
use shepherd_core::registry::{Registry, Snapshot};
use shepherd_core::store::Store;
use shepherd_core::Control;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Manager, State};

pub struct Ipc {
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
    pub config: ShepherdConfig,
    pub hook_bin: PathBuf,
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
pub fn hooks_status(state: State<Ipc>) -> bool {
    install::installed(&state.config.claude_settings_path)
}

#[tauri::command]
pub fn set_hooks(state: State<Ipc>, enabled: bool) -> Result<bool, String> {
    if enabled {
        if !state.hook_bin.exists() {
            return Err(format!(
                "shepherd-hook not found at {} (build it with: cargo build -p shepherd-hook)",
                state.hook_bin.display()
            ));
        }
        install::install(
            &state.config.claude_settings_path,
            &state.hook_bin,
            &state.config.socket_path,
        )
        .map(|_| true)
    } else {
        install::uninstall(&state.config.claude_settings_path).map(|_| false)
    }
}

#[tauri::command]
pub fn hide_panel(app: AppHandle) {
    if let Some(w) = app.get_webview_window("panel") {
        let _ = w.hide();
    }
}
