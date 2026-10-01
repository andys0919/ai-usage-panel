//! Desktop shell: keeps the usage numbers fresh in the background (every 5 minutes by default)
//! and hands them to the UI. All data gathering lives in `usage-core`.

mod settings;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use usage_core::cache::{self, Cache};
use usage_core::collector::{Collector, CollectorConfig};
use usage_core::model::{AccountView, Provider};
use usage_core::util::now_ms;

use settings::{Settings, ALLOWED_INTERVALS_SECS, MANUAL_COOLDOWN_MS};

const SNAPSHOT_EVENT: &str = "usage-snapshot";

#[derive(Clone, Serialize)]
struct Snapshot {
    accounts: Vec<AccountView>,
    cycle_running: bool,
    cycle_started_ms: Option<i64>,
    /// When the next automatic cycle starts.
    next_cycle_ms: i64,
    /// Earliest moment the "refresh now" button works again.
    manual_allowed_at_ms: i64,
    interval_secs: u64,
    auto_refresh_tokens: bool,
    allowed_intervals_secs: Vec<u64>,
}

struct Inner {
    accounts: Vec<AccountView>,
    cycle_running: bool,
    cycle_started_ms: Option<i64>,
    manual_requested: bool,
    settings: Settings,
}

struct AppState {
    collector: Arc<Collector>,
    base_cfg: CollectorConfig,
    data_dir: PathBuf,
    inner: Mutex<Inner>,
    /// Wakes the poller: manual refresh requested or settings changed.
    wake: tokio::sync::Notify,
}

fn provider_rank(p: Provider) -> u8 {
    match p {
        Provider::Claude => 0,
        Provider::Codex => 1,
        Provider::Gemini => 2,
    }
}

fn sort_accounts(accounts: &mut [AccountView]) {
    accounts.sort_by_key(|a| {
        (
            provider_rank(a.provider),
            a.label.to_lowercase(),
            a.key.clone(),
        )
    });
}

impl AppState {
    fn snapshot(&self) -> Snapshot {
        let inner = self.inner.lock().unwrap();
        let interval_ms = inner.settings.interval_secs as i64 * 1000;
        Snapshot {
            accounts: inner.accounts.clone(),
            cycle_running: inner.cycle_running,
            cycle_started_ms: inner.cycle_started_ms,
            next_cycle_ms: inner
                .cycle_started_ms
                .map_or_else(now_ms, |t| t + interval_ms),
            manual_allowed_at_ms: inner.cycle_started_ms.map_or(0, |t| t + MANUAL_COOLDOWN_MS),
            interval_secs: inner.settings.interval_secs,
            auto_refresh_tokens: inner.settings.auto_refresh_tokens,
            allowed_intervals_secs: ALLOWED_INTERVALS_SECS.to_vec(),
        }
    }

    fn persist_cache(&self) {
        let cache = {
            let inner = self.inner.lock().unwrap();
            Cache {
                cycle_started_ms: inner.cycle_started_ms,
                accounts: inner.accounts.clone(),
            }
        };
        if let Err(e) = cache::save(&self.data_dir.join("cache.json"), &cache) {
            eprintln!("could not save cache: {}", e.kind());
        }
    }
}

fn emit(app: &AppHandle, state: &AppState) {
    let _ = app.emit(SNAPSHOT_EVENT, state.snapshot());
}

async fn run_cycle(app: &AppHandle, state: &Arc<AppState>) {
    let cfg = {
        let mut inner = state.inner.lock().unwrap();
        inner.cycle_running = true;
        inner.manual_requested = false;
        inner.cycle_started_ms = Some(now_ms());
        let mut cfg = state.base_cfg.clone();
        cfg.auto_refresh_tokens = inner.settings.auto_refresh_tokens;
        cfg
    };

    // Keep the list of cards in step with the accounts that exist on disk right now.
    let discovered = state.collector.skeletons(&cfg);
    {
        let mut inner = state.inner.lock().unwrap();
        inner
            .accounts
            .retain(|a| discovered.iter().any(|d| d.key == a.key));
        for d in discovered {
            if !inner.accounts.iter().any(|a| a.key == d.key) {
                inner.accounts.push(d);
            }
        }
        sort_accounts(&mut inner.accounts);
    }
    state.persist_cache(); // remember the cycle start so a restart does not query again at once
    emit(app, state);

    let (app2, state2) = (app.clone(), state.clone());
    state
        .collector
        .collect(&cfg, move |view| {
            {
                let mut inner = state2.inner.lock().unwrap();
                match inner.accounts.iter_mut().find(|a| a.key == view.key) {
                    Some(slot) => *slot = view,
                    None => inner.accounts.push(view),
                }
                sort_accounts(&mut inner.accounts);
            }
            emit(&app2, &state2);
        })
        .await;

    state.inner.lock().unwrap().cycle_running = false;
    state.persist_cache();
    emit(app, state);
}

fn spawn_poller(app: AppHandle, state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        loop {
            let (wait_ms, manual) = {
                let inner = state.inner.lock().unwrap();
                let interval_ms = inner.settings.interval_secs as i64 * 1000;
                let now = now_ms();
                let due = inner.cycle_started_ms.map_or(now, |t| t + interval_ms);
                ((due - now).max(0), inner.manual_requested)
            };
            if wait_ms > 0 && !manual {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(wait_ms as u64)) => {}
                    () = state.wake.notified() => {
                        // settings changed or a manual refresh was requested: re-evaluate
                        emit(&app, &state);
                        continue;
                    }
                }
            }
            run_cycle(&app, &state).await;
        }
    });
}

#[tauri::command]
fn get_state(state: State<'_, Arc<AppState>>) -> Snapshot {
    state.snapshot()
}

#[tauri::command]
fn refresh_now(state: State<'_, Arc<AppState>>) -> Result<Snapshot, String> {
    {
        let mut inner = state.inner.lock().unwrap();
        if inner.cycle_running {
            return Err("正在更新中,請稍等".into());
        }
        if let Some(t) = inner.cycle_started_ms {
            let left = t + MANUAL_COOLDOWN_MS - now_ms();
            if left > 0 {
                return Err(format!("官方會限流,請 {} 秒後再按", left / 1000 + 1));
            }
        }
        inner.manual_requested = true;
    }
    state.wake.notify_one();
    Ok(state.snapshot())
}

#[tauri::command]
fn set_settings(
    state: State<'_, Arc<AppState>>,
    interval_secs: u64,
    auto_refresh_tokens: bool,
) -> Result<Snapshot, String> {
    if !ALLOWED_INTERVALS_SECS.contains(&interval_secs) {
        return Err("不支援這個更新間隔(最短 5 分鐘)".into());
    }
    let settings = {
        let mut inner = state.inner.lock().unwrap();
        let turned_on = !inner.settings.auto_refresh_tokens && auto_refresh_tokens;
        inner.settings = Settings {
            interval_secs,
            auto_refresh_tokens,
        };
        // Switching token refresh ON is the cure for "token expired" cards: do not make the
        // user wait for the next cycle (this is an explicit action, so the cool-down is skipped).
        if turned_on && !inner.cycle_running {
            inner.manual_requested = true;
        }
        inner.settings.clone()
    };
    settings
        .save(&state.data_dir.join("settings.json"))
        .map_err(|e| format!("設定存不下來({})", e.kind()))?;
    state.wake.notify_one();
    Ok(state.snapshot())
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
            let base_cfg = CollectorConfig::from_env(true);
            let data_dir = base_cfg.roots.appdata.join("ai-usage-panel");
            let settings = Settings::load(&data_dir.join("settings.json"));
            let cached = cache::load(&data_dir.join("cache.json")).unwrap_or_default();

            let collector = Collector::new();
            collector.seed_last_good(cached.accounts.clone());

            // Show last run's numbers at once; accounts never seen before start as loading cards.
            let mut accounts = collector.skeletons(&base_cfg);
            for slot in &mut accounts {
                if let Some(old) = cached.accounts.iter().find(|c| c.key == slot.key) {
                    *slot = old.clone();
                }
            }
            sort_accounts(&mut accounts);

            let state = Arc::new(AppState {
                collector,
                base_cfg,
                data_dir,
                inner: Mutex::new(Inner {
                    accounts,
                    cycle_running: false,
                    cycle_started_ms: cached.cycle_started_ms,
                    manual_requested: false,
                    settings,
                }),
                wake: tokio::sync::Notify::new(),
            });
            app.manage(state.clone());
            spawn_poller(app.handle().clone(), state);
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
