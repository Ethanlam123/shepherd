//! Shepherd app shell: panel window, tray, adapters, and IPC wiring.

mod icon;
mod ipc;
mod tray;

use serde::Serialize;
use shepherd_core::registry::{EventSink, UiEvent};
use shepherd_core::{mock, AdapterContext};
use std::sync::Arc;
use tauri::{Emitter, Manager, WindowEvent};
use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};

/// Bridges registry events to the webview and the tray badge.
struct AppSink {
    app: tauri::AppHandle,
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

            let registry = Arc::new(shepherd_core::registry::Registry::new(Arc::new(AppSink {
                app: app.handle().clone(),
            })));
            for adapter in mock::adapters() {
                let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel();
                registry.register_adapter(adapter.id(), ctl_tx);
                let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
                adapter.spawn(AdapterContext {
                    events: ev_tx,
                    controls: ctl_rx,
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
            ipc::hide_panel
        ])
        .run(tauri::generate_context!())
        .expect("error while running shepherd");
}
