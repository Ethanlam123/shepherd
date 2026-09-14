//! Tauri commands the panel calls over IPC.

use serde::Serialize;
use shepherd_core::cc::install;
use shepherd_core::config::ShepherdConfig;
use shepherd_core::registry::{Registry, Snapshot};
use shepherd_core::store::Store;
use shepherd_core::Control;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_updater::UpdaterExt;

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

/* ---------- launch at login (SMAppService) ---------- */

#[tauri::command]
pub fn login_item_enabled() -> Result<bool, String> {
    crate::login::is_enabled()
}

#[tauri::command]
pub fn set_login_item(enabled: bool) -> Result<(), String> {
    crate::login::set(enabled)
}

/* ---------- updates (minisign updater) ---------- */

/// Pending update as the panel shows it. `notes` is the release body.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub notes: Option<String>,
}

/// Manual check only - no startup auto-poll while there is no release
/// server; errors land in the panel notice line.
#[tauri::command]
pub async fn check_updates(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    let update = app
        .updater()
        .map_err(|e| format!("updater unavailable: {e}"))?
        .check()
        .await
        .map_err(|e| format!("update check failed: {e}"))?;
    Ok(update.map(|u| UpdateInfo {
        version: u.version.clone(),
        notes: u.body.clone(),
    }))
}

#[tauri::command]
pub async fn install_updates(app: AppHandle) -> Result<(), String> {
    let update = app
        .updater()
        .map_err(|e| format!("updater unavailable: {e}"))?
        .check()
        .await
        .map_err(|e| format!("update check failed: {e}"))?
        .ok_or("no update available")?;
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| format!("update install failed: {e}"))
}

/// The updater swaps the app bundle; a restart is required to run it.
#[tauri::command]
pub fn relaunch(app: AppHandle) {
    app.restart();
}
