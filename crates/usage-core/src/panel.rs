//! The panel's state, shared by the desktop shell and the web server: keeps the usage numbers
//! fresh in the background (every 5 minutes by default) and publishes every change.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify};

use crate::cache::{self, Cache};
use crate::collector::{Collector, CollectorConfig};
use crate::fsutil::write_atomic;
use crate::model::{AccountView, Provider};
use crate::util::now_ms;

// ---- Settings -----------------------------------------------------------------------------

/// The official endpoints rate-limit, so the interval can never be shorter than 5 minutes.
pub const ALLOWED_INTERVALS_SECS: [u64; 4] = [300, 600, 900, 1800];
pub const DEFAULT_INTERVAL_SECS: u64 = 300;
/// Minimum time between two manual refreshes.
pub const MANUAL_COOLDOWN_MS: i64 = 60_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub interval_secs: u64,
    /// Refresh expired OAuth tokens (and write them back to the credential file).
    pub auto_refresh_tokens: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            interval_secs: DEFAULT_INTERVAL_SECS,
            auto_refresh_tokens: true,
        }
    }
}

impl Settings {
    fn sanitized(mut self) -> Self {
        if !ALLOWED_INTERVALS_SECS.contains(&self.interval_secs) {
            self.interval_secs = DEFAULT_INTERVAL_SECS;
        }
        self
    }

    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
            .unwrap_or_default()
            .sanitized()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_atomic(path, &bytes)
    }
}

// ---- Panel --------------------------------------------------------------------------------

#[derive(Clone, Serialize)]
pub struct Snapshot {
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

impl Inner {
    fn snapshot(&self) -> Snapshot {
        let interval_ms = self.settings.interval_secs as i64 * 1000;
        Snapshot {
            accounts: self.accounts.clone(),
            cycle_running: self.cycle_running,
            cycle_started_ms: self.cycle_started_ms,
            next_cycle_ms: self
                .cycle_started_ms
                .map_or_else(now_ms, |t| t + interval_ms),
            manual_allowed_at_ms: self.cycle_started_ms.map_or(0, |t| t + MANUAL_COOLDOWN_MS),
            interval_secs: self.settings.interval_secs,
            auto_refresh_tokens: self.settings.auto_refresh_tokens,
            allowed_intervals_secs: ALLOWED_INTERVALS_SECS.to_vec(),
        }
    }
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

pub struct Panel {
    collector: Arc<Collector>,
    base_cfg: CollectorConfig,
    /// Holds settings.json and cache.json.
    data_dir: PathBuf,
    inner: Mutex<Inner>,
    /// Wakes the poller: manual refresh requested or settings changed.
    wake: Notify,
    snapshots: watch::Sender<Snapshot>,
}

impl Panel {
    pub fn new(base_cfg: CollectorConfig, data_dir: PathBuf) -> Arc<Self> {
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

        let inner = Inner {
            accounts,
            cycle_running: false,
            cycle_started_ms: cached.cycle_started_ms,
            manual_requested: false,
            settings,
        };
        let (snapshots, _) = watch::channel(inner.snapshot());
        Arc::new(Self {
            collector,
            base_cfg,
            data_dir,
            inner: Mutex::new(inner),
            wake: Notify::new(),
            snapshots,
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.lock().unwrap().snapshot()
    }

    /// Every published snapshot (the latest one wins when several arrive at once).
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.snapshots.subscribe()
    }

    fn publish(&self) {
        self.snapshots.send_replace(self.snapshot());
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

    async fn run_cycle(self: &Arc<Self>) {
        let cfg = {
            let mut inner = self.inner.lock().unwrap();
            inner.cycle_running = true;
            inner.manual_requested = false;
            inner.cycle_started_ms = Some(now_ms());
            let mut cfg = self.base_cfg.clone();
            cfg.auto_refresh_tokens = inner.settings.auto_refresh_tokens;
            cfg
        };

        // Keep the list of cards in step with the accounts that exist on disk right now.
        let discovered = self.collector.skeletons(&cfg);
        {
            let mut inner = self.inner.lock().unwrap();
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
        self.persist_cache(); // remember the cycle start so a restart does not query again at once
        self.publish();

        let me = self.clone();
        self.collector
            .collect(&cfg, move |view| {
                {
                    let mut inner = me.inner.lock().unwrap();
                    match inner.accounts.iter_mut().find(|a| a.key == view.key) {
                        Some(slot) => *slot = view,
                        None => inner.accounts.push(view),
                    }
                    sort_accounts(&mut inner.accounts);
                }
                me.publish();
            })
            .await;

        self.inner.lock().unwrap().cycle_running = false;
        self.persist_cache();
        self.publish();
    }

    /// The scheduler. Spawn it once on the caller's runtime.
    pub async fn run_forever(self: Arc<Self>) {
        loop {
            let (wait_ms, manual) = {
                let inner = self.inner.lock().unwrap();
                let interval_ms = inner.settings.interval_secs as i64 * 1000;
                let now = now_ms();
                let due = inner.cycle_started_ms.map_or(now, |t| t + interval_ms);
                ((due - now).max(0), inner.manual_requested)
            };
            if wait_ms > 0 && !manual {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(wait_ms as u64)) => {}
                    () = self.wake.notified() => {
                        // settings changed or a manual refresh was requested: re-evaluate
                        self.publish();
                        continue;
                    }
                }
            }
            self.run_cycle().await;
        }
    }

    pub fn refresh_now(&self) -> Result<Snapshot, String> {
        {
            let mut inner = self.inner.lock().unwrap();
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
        self.wake.notify_one();
        Ok(self.snapshot())
    }

    pub fn set_settings(
        &self,
        interval_secs: u64,
        auto_refresh_tokens: bool,
    ) -> Result<Snapshot, String> {
        if !ALLOWED_INTERVALS_SECS.contains(&interval_secs) {
            return Err("不支援這個更新間隔(最短 5 分鐘)".into());
        }
        let settings = {
            let mut inner = self.inner.lock().unwrap();
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
            .save(&self.data_dir.join("settings.json"))
            .map_err(|e| format!("設定存不下來({})", e.kind()))?;
        self.wake.notify_one();
        Ok(self.snapshot())
    }
}
