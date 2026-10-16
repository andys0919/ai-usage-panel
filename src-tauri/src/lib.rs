//! Desktop shell: shows the panel in a window. Scheduling lives in `usage_core::panel`,
//! all data gathering in the rest of `usage-core`.

use std::sync::Arc;

use tauri::{Emitter, Manager, State};
use usage_core::collector::CollectorConfig;
use usage_core::panel::{Panel, Snapshot};

const SNAPSHOT_EVENT: &str = "usage-snapshot";

#[tauri::command]
fn get_state(panel: State<'_, Arc<Panel>>) -> Snapshot {
    panel.snapshot()
}

#[tauri::command]
fn refresh_now(panel: State<'_, Arc<Panel>>) -> Result<Snapshot, String> {
    panel.refresh_now()
}

#[tauri::command]
fn set_settings(
    panel: State<'_, Arc<Panel>>,
    interval_secs: u64,
    auto_refresh_tokens: bool,
) -> Result<Snapshot, String> {
    panel.set_settings(interval_secs, auto_refresh_tokens)
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // A second copy would double the requests to rate-limited endpoints: focus the first.
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .setup(|app| {
            let cfg = CollectorConfig::from_env(true);
            let data_dir = cfg.roots.appdata.join("ai-usage-panel");
            let panel = Panel::new(cfg, data_dir);
            app.manage(panel.clone());

            let mut snapshots = panel.subscribe();
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while snapshots.changed().await.is_ok() {
                    let snap = snapshots.borrow_and_update().clone();
                    let _ = handle.emit(SNAPSHOT_EVENT, snap);
                }
            });
            tauri::async_runtime::spawn(panel.run_forever());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            refresh_now,
            set_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running the AI usage panel");
}
