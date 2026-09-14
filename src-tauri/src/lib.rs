//! Shepherd app shell: panel window, tray, adapters, and IPC wiring.

mod icon;
mod ipc;
mod tray;

use serde::Serialize;
use shepherd_core::config::ShepherdConfig;
use shepherd_core::registry::{EventSink, UiEvent, UiSession};
use shepherd_core::store::Store;
use shepherd_core::{cc, mock, AdapterContext, AgentAdapter, Pending, Status};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager, WindowEvent};
use tauri_plugin_notification::NotificationExt;
use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};

/// Bridges registry events to the webview, the tray badge, the store, and
/// (M4) macOS notifications.
struct AppSink {
    app: tauri::AppHandle,
    store: Arc<Store>,
    /// Sessions already notified for their current wait, so one card fires
    /// exactly one notification. Cleared when the session resumes or ends.
    notified: Mutex<HashSet<String>>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ActivityPayload {
    session_id: String,
    line: shepherd_core::LogLine,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RemovedPayload {
    session_id: String,
}

impl EventSink for AppSink {
    fn emit(&self, event: UiEvent) {
        // Persistence failures must not break the live panel; stderr keeps
        // them visible for debugging.
        match &event {
            UiEvent::Session(s) => {
                if let Err(e) = self.store.upsert_session(s) {
                    eprintln!("shepherd: persist session failed: {e}");
                }
                self.maybe_notify(s);
            }
            UiEvent::Removed { session_id } => {
                if let Err(e) = self.store.remove_session(session_id) {
                    eprintln!("shepherd: remove session failed: {e}");
                }
                self.notified.lock().unwrap().remove(session_id);
            }
            UiEvent::Run(r) => {
                if let Err(e) = self.store.insert_run(r) {
                    eprintln!("shepherd: persist run failed: {e}");
                }
            }
            _ => {}
        }
        match event {
            UiEvent::Session(s) => {
                let _ = self.app.emit("session", s);
            }
            UiEvent::Activity { session_id, line } => {
                let _ = self
                    .app
                    .emit("activity", ActivityPayload { session_id, line });
            }
            UiEvent::Removed { session_id } => {
                let _ = self.app.emit("removed", RemovedPayload { session_id });
            }
            UiEvent::Run(r) => {
                let _ = self.app.emit("run", r);
            }
            UiEvent::Badge { count } => tray::refresh_badge(&self.app, count),
        }
    }
}

impl AppSink {
    /// One OS notification per wait, gated by the persisted mute flag. The
    /// notification plugin has no click events on desktop, so the amber tray
    /// badge stays the way back in.
    fn maybe_notify(&self, s: &UiSession) {
        let mut notified = self.notified.lock().unwrap();
        match s.session.status {
            Status::Waiting => {
                let Some(pending) = &s.pending else {
                    return;
                };
                if !notified.insert(s.session.id.clone()) {
                    return; // already notified for this wait
                }
                if self.store.muted() {
                    return;
                }
                let (title, body) = match pending {
                    Pending::Permission(p) => (
                        "Agent needs approval",
                        if p.command.is_empty() {
                            p.tool.as_str()
                        } else {
                            p.command.as_str()
                        },
                    ),
                    Pending::Input(q) => ("Agent is waiting for you", q.question.as_str()),
                };
                if let Err(e) = self
                    .app
                    .notification()
                    .builder()
                    .title(title)
                    .body(body)
                    .show()
                {
                    eprintln!("shepherd: notification failed: {e}");
                }
            }
            _ => {
                notified.remove(&s.session.id);
            }
        }
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            // menu-bar app: no dock icon
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let panel = app.get_webview_window("panel").expect("panel window");
            apply_vibrancy(&panel, NSVisualEffectMaterial::Sidebar, None, None)
                .expect("apply vibrancy");

            let config = ShepherdConfig::default();
            // shepherd-hook lives next to the app binary (bundle or target dir)
            let hook_bin = std::env::var("SHEPHERD_HOOK_BIN")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| {
                    std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.join("shepherd-hook")))
                        .unwrap_or_else(|| std::path::PathBuf::from("shepherd-hook"))
                });
            let store = Arc::new(Store::open(&config.db_path).expect("open shepherd db"));
            let registry = Arc::new(shepherd_core::registry::Registry::new(Arc::new(AppSink {
                app: app.handle().clone(),
                store: store.clone(),
                notified: Mutex::new(HashSet::new()),
            })));
            registry.set_muted(store.muted());
            if let Err(e) = store.recent_runs(200).map(|runs| registry.seed_runs(runs)) {
                eprintln!("shepherd: load runs failed: {e}");
            }

            // real Claude Code sessions always; mock demo agents on demand
            // (SHEPHERD_MOCK=1 npm run dev) so mock runs never pollute the db
            let mut adapters: Vec<Box<dyn AgentAdapter>> =
                vec![Box::new(cc::CcAdapter::new(config.clone()))];
            if std::env::var("SHEPHERD_MOCK").ok().as_deref() == Some("1") {
                adapters.extend(mock::adapters());
            }
            for adapter in adapters {
                let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel();
                registry.register_adapter(adapter.id(), ctl_tx);
                let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
                adapter.spawn(AdapterContext {
                    events: ev_tx,
                    controls: ctl_rx,
                    spawn: Arc::new(|f| {
                        tauri::async_runtime::spawn(f);
                    }),
                });
                let reg = registry.clone();
                tauri::async_runtime::spawn(async move {
                    while let Some(env) = ev_rx.recv().await {
                        reg.handle_event(env);
                    }
                });
            }

            tray::build(app.handle())?;
            app.manage(ipc::Ipc {
                registry: registry.clone(),
                store,
                config,
                hook_bin,
            });
            Ok(())
        })
        .on_window_event(|window, event| match event {
            WindowEvent::Focused(false) if window.label() == "panel" => {
                tray::blur_hide(window.app_handle());
            }
            // panel hides, never closes: Shepherd lives in the tray
            WindowEvent::CloseRequested { api, .. } if window.label() == "panel" => {
                api.prevent_close();
                let _ = window.hide();
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            ipc::get_state,
            ipc::send_control,
            ipc::set_muted,
            ipc::hooks_status,
            ipc::set_hooks,
            ipc::hide_panel
        ])
        .run(tauri::generate_context!())
        .expect("error while running shepherd");
}
