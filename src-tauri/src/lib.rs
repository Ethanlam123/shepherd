//! Shepherd app shell: panel window, tray, adapters, and IPC wiring.

mod icon;
mod ipc;
mod tray;

use serde::Serialize;
use shepherd_core::config::ShepherdConfig;
use shepherd_core::registry::{EventSink, UiEvent};
use shepherd_core::store::Store;
use shepherd_core::{cc, mock, AdapterContext, AgentAdapter};
use std::sync::Arc;
use tauri::{Emitter, Manager, WindowEvent};
use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};

/// Bridges registry events to the webview, the tray badge, and the store.
struct AppSink {
    app: tauri::AppHandle,
    store: Arc<Store>,
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
            }
            UiEvent::Removed { session_id } => {
                if let Err(e) = self.store.remove_session(session_id) {
                    eprintln!("shepherd: remove session failed: {e}");
                }
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

pub fn run() {
    tauri::Builder::default()
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
