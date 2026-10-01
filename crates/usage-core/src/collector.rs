//! Gathers all accounts concurrently and turns every outcome into an `AccountView`.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::accounts::{self, ClaudeAccount, CodexAccount, GeminiAccount, Roots};
use crate::http::FetchError;
use crate::model::{AccountView, Extra, Provider, StateKind, WindowView};
use crate::oauth::{
    self, ensure_fresh, peek, ClaudeFlavor, CodexFlavor, EnsureError, FileStore, Need, RefreshCtx,
    TokenFlavor,
};
use crate::util::now_ms;
use crate::{claude, codex, gemini};

/// Refresh a token when it expires within five minutes (same margin Orca uses).
const SKEW_MS: i64 = 300_000;
const MAX_BACKOFF_SECS: u64 = 900;
const MIN_BACKOFF_SECS: u64 = 60;
/// A 401 right after a "valid" token forces one refresh; never more often than this.
const FORCE_REFRESH_COOLDOWN_MS: i64 = 30 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub claude_usage: String,
    pub claude_token: String,
    pub codex_usage: String,
    pub codex_token: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            claude_usage: claude::USAGE_URL.into(),
            claude_token: oauth::CLAUDE_TOKEN_URL.into(),
            codex_usage: codex::USAGE_URL.into(),
            codex_token: oauth::CODEX_TOKEN_URL.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CollectorConfig {
    pub roots: Roots,
    pub endpoints: Endpoints,
    /// When false the program never refreshes or writes any credential file.
    pub auto_refresh_tokens: bool,
    /// Where freshly rotated tokens are saved if writing them back fails.
    pub recovery_dir: PathBuf,
    /// Only handle accounts whose key or label contains this text (case-insensitive).
    pub only: Option<String>,
}

impl CollectorConfig {
    pub fn from_env(auto_refresh_tokens: bool) -> Self {
        let roots = Roots::from_env();
        let recovery_dir = roots.appdata.join("ai-usage-panel").join("recovery");
        Self {
            roots,
            endpoints: Endpoints::default(),
            auto_refresh_tokens,
            recovery_dir,
            only: None,
        }
    }

    fn wants(&self, key: &str, label: &str) -> bool {
        self.only.as_ref().is_none_or(|f| {
            let f = f.to_lowercase();
            key.to_lowercase().contains(&f) || label.to_lowercase().contains(&f)
        })
    }
}

struct Fetched {
    windows: Vec<WindowView>,
    plan: Option<String>,
    extras: Vec<Extra>,
}

/// What to tell the user when only an interactive login can fix an account.
struct LoginHint {
    text: String,
    command: String,
}

/// Everything the shared OAuth flow needs to know about one account's credentials.
struct Flow<'a> {
    cfg: &'a CollectorConfig,
    store: FileStore,
    policy: RefreshPolicy,
    hint: LoginHint,
}

/// May this program rotate the account's refresh token?
#[derive(Clone, Copy, PartialEq, Eq)]
enum RefreshPolicy {
    Allowed,
    /// The user switched automatic refreshing off in the settings.
    DisabledBySetting,
    /// The same login also lives in other files (Orca's runtime home, the CLI's own home …).
    /// Rotating the token in one copy would kill the refresh token of every other copy and
    /// log the user out of the running CLI, so the official CLI must do that itself.
    SharedLogin,
}

impl RefreshPolicy {
    fn for_account(cfg: &CollectorConfig, shared: bool) -> Self {
        if shared {
            Self::SharedLogin
        } else if cfg.auto_refresh_tokens {
            Self::Allowed
        } else {
            Self::DisabledBySetting
        }
    }
}

fn expiry_of<T: TokenFlavor>(flavor: &T, path: &std::path::Path) -> Option<i64> {
    accounts::read_json(path)
        .and_then(|doc| flavor.parse(&doc))
        .and_then(|info| info.expires_at_ms)
}

/// Among all files that hold the same login, read the one whose token lives longest.
fn freshest_store<T: TokenFlavor>(
    flavor: &T,
    primary: &std::path::Path,
    copies: &[PathBuf],
) -> FileStore {
    let mut best = primary.to_path_buf();
    let mut best_expiry = expiry_of(flavor, primary);
    for copy in copies {
        let expiry = expiry_of(flavor, copy);
        if expiry > best_expiry {
            best = copy.clone();
            best_expiry = expiry;
        }
    }
    FileStore::new(best)
}

pub struct Collector {
    http: reqwest::Client,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    last_good: Mutex<HashMap<String, AccountView>>,
    /// account key -> (do not call the network before this time, why)
    retry_not_before: Mutex<HashMap<String, (i64, String)>>,
    /// account key -> consecutive refresh failures (drives the growing back-off)
    refresh_failures: Mutex<HashMap<String, u32>>,
    forced_at: Mutex<HashMap<String, i64>>,
}

impl Collector {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("HTTP client"),
            locks: Mutex::default(),
            last_good: Mutex::default(),
            retry_not_before: Mutex::default(),
            refresh_failures: Mutex::default(),
            forced_at: Mutex::default(),
        })
    }

    /// Remember views from a previous run so a failure can fall back to them (marked stale).
    pub fn seed_last_good(&self, views: impl IntoIterator<Item = AccountView>) {
        let mut last = self.last_good.lock().unwrap();
        for v in views {
            if v.state == StateKind::Ok {
                last.insert(v.key.clone(), v);
            }
        }
    }

    /// Cards to show immediately, before the first network answer arrives.
    pub fn skeletons(&self, cfg: &CollectorConfig) -> Vec<AccountView> {
        let mut out: Vec<AccountView> = Vec::new();
        out.extend(
            accounts::discover_claude(&cfg.roots)
                .iter()
                .map(|a| self.claude_base(a)),
        );
        out.extend(
            accounts::discover_codex(&cfg.roots)
                .iter()
                .map(|a| self.codex_base(a)),
        );
        out.extend(
            accounts::discover_gemini(&cfg.roots)
                .iter()
                .map(|a| self.gemini_base(a)),
        );
        for v in &mut out {
            v.state = StateKind::Loading;
        }
        out
    }

    /// Fetch every account; `on_account` is called as soon as each one finishes.
    pub async fn collect(
        self: &Arc<Self>,
        cfg: &CollectorConfig,
        mut on_account: impl FnMut(AccountView) + Send,
    ) {
        let cfg = Arc::new(cfg.clone());
        let mut set = tokio::task::JoinSet::new();

        for a in accounts::discover_claude(&cfg.roots)
            .into_iter()
            .filter(|a| cfg.wants(&a.key, &a.email))
        {
            let (me, cfg) = (self.clone(), cfg.clone());
            set.spawn(async move { me.claude_account(&cfg, a).await });
        }
        for a in accounts::discover_codex(&cfg.roots)
            .into_iter()
            .filter(|a| cfg.wants(&a.key, &a.email))
        {
            let (me, cfg) = (self.clone(), cfg.clone());
            set.spawn(async move { me.codex_account(&cfg, a).await });
        }
        if let Some(a) =
            accounts::discover_gemini(&cfg.roots).filter(|a| cfg.wants(&a.key, &a.email))
        {
            let (me, cfg) = (self.clone(), cfg.clone());
            set.spawn(async move { me.gemini_account(&cfg, a).await });
        }

        while let Some(done) = set.join_next().await {
            if let Ok(view) = done {
                if view.state == StateKind::Ok {
                    self.last_good
                        .lock()
                        .unwrap()
                        .insert(view.key.clone(), view.clone());
                }
                on_account(view);
            }
        }
    }

    // ---- per provider ---------------------------------------------------------------

    fn claude_base(&self, a: &ClaudeAccount) -> AccountView {
        let mut v = AccountView::new(a.key.clone(), Provider::Claude, a.email.clone());
        v.detail = a.org.clone();
        v.plan = accounts::read_json(&a.creds_path).and_then(|doc| {
            let o = doc.get("claudeAiOauth")?;
            claude::plan_label(
                o.get("subscriptionType").and_then(Value::as_str),
                o.get("rateLimitTier").and_then(Value::as_str),
            )
        });
        v
    }

    fn codex_base(&self, a: &CodexAccount) -> AccountView {
        let mut v = AccountView::new(a.key.clone(), Provider::Codex, a.email.clone());
        v.detail = a.detail.clone();
        v.plan = a.plan_hint.clone();
        v
    }

    fn gemini_base(&self, a: &GeminiAccount) -> AccountView {
        let mut v = AccountView::new(a.key.clone(), Provider::Gemini, a.email.clone());
        v.detail = Some("Antigravity CLI".into());
        v
    }

    async fn claude_account(&self, cfg: &CollectorConfig, acct: ClaudeAccount) -> AccountView {
        let view = self.claude_base(&acct);
        let flavor = ClaudeFlavor::new(cfg.endpoints.claude_token.clone());
        let store = freshest_store(&flavor, &acct.creds_path, &acct.shared_copies);
        let policy = RefreshPolicy::for_account(cfg, !acct.shared_copies.is_empty());
        let (http, url) = (&self.http, cfg.endpoints.claude_usage.as_str());
        let hint = LoginHint {
            text: format!("請用 {} 重新登入", acct.email),
            command: "orca account add".into(),
        };
        let flow = Flow {
            cfg,
            store,
            policy,
            hint,
        };
        self.oauth_flow(flow, view, &flavor, |token| async move {
            claude::fetch_usage(http, url, &token)
                .await
                .map(|windows| Fetched {
                    windows,
                    plan: None,
                    extras: Vec::new(),
                })
        })
        .await
    }

    async fn codex_account(&self, cfg: &CollectorConfig, acct: CodexAccount) -> AccountView {
        let view = self.codex_base(&acct);
        let flavor = CodexFlavor::new(cfg.endpoints.codex_token.clone());
        let store = freshest_store(&flavor, &acct.auth_path, &acct.shared_copies);
        let policy = RefreshPolicy::for_account(cfg, !acct.shared_copies.is_empty());
        let (http, url) = (&self.http, cfg.endpoints.codex_usage.as_str());
        let auth_path = store.path().to_path_buf();
        let command = if acct.key.starts_with("codex:system") || acct.key == "codex:orca-runtime" {
            "codex login"
        } else {
            "orca account add --agent codex"
        };
        let hint = LoginHint {
            text: format!("請用 {} 重新登入", acct.email),
            command: command.into(),
        };
        let flow = Flow {
            cfg,
            store,
            policy,
            hint,
        };
        self.oauth_flow(flow, view, &flavor, |token| {
            let auth_path = auth_path.clone();
            async move {
                let account_id = accounts::read_json(&auth_path).and_then(|d| {
                    d.pointer("/tokens/account_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
                codex::fetch_usage(http, url, &token, account_id.as_deref())
                    .await
                    .map(|u| Fetched {
                        windows: u.windows,
                        plan: u.plan,
                        extras: u.extras,
                    })
            }
        })
        .await
    }

    async fn gemini_account(&self, cfg: &CollectorConfig, acct: GeminiAccount) -> AccountView {
        let mut view = self.gemini_base(&acct);
        match gemini::fetch_quota(&acct.agy_path, &cfg.roots.home).await {
            Ok(windows) => {
                view.windows = windows;
                view.state = StateKind::Ok;
                view.fetched_at_ms = Some(now_ms());
                view
            }
            Err(msg) => {
                let lower = msg.to_lowercase();
                if lower.contains("login") || lower.contains("sign in") || lower.contains("auth") {
                    view.state = StateKind::LoginRequired;
                    view.message = Some(format!("{msg}。請重新登入 Antigravity"));
                    view.fix_command = Some("agy".into());
                    view
                } else {
                    self.stale_or_fail(view, msg, true)
                }
            }
        }
    }

    // ---- shared flow for OAuth-file based providers ------------------------------------

    async fn oauth_flow<T, F, Fut>(
        &self,
        flow: Flow<'_>,
        mut view: AccountView,
        flavor: &T,
        fetch: F,
    ) -> AccountView
    where
        T: TokenFlavor,
        F: Fn(String) -> Fut + Send + Sync,
        Fut: Future<Output = Result<Fetched, FetchError>> + Send,
    {
        let Flow {
            cfg,
            store,
            policy,
            hint: login_hint,
        } = flow;
        let (store, login_hint) = (&store, &login_hint);
        let now = now_ms();
        let waiting = self
            .retry_not_before
            .lock()
            .unwrap()
            .get(&view.key)
            .cloned();
        if let Some((until, why)) = waiting {
            if until > now {
                let reason = format!("{why}(約 {} 分鐘後自動再試)", (until - now) / 60_000 + 1);
                return self.stale_or_fail(view, reason, true);
            }
        }

        let lock = self.lock_for(&view.key);
        let ctx = RefreshCtx {
            http: &self.http,
            lock: &lock,
            now_ms: now,
            skew_ms: SKEW_MS,
            recovery_dir: &cfg.recovery_dir,
        };

        let first = if policy == RefreshPolicy::Allowed {
            ensure_fresh(flavor, store, &ctx, Need::IfExpired).await
        } else {
            peek(flavor, store, &ctx)
        };
        let fresh = match first {
            Ok(f) => {
                self.refresh_failures.lock().unwrap().remove(&view.key);
                f
            }
            Err(e) => return self.view_for_ensure_error(view, e, login_hint, policy, now),
        };

        let mut refreshed = fresh.refreshed;
        let mut warning = fresh.warning;
        let mut result = fetch(fresh.access_token).await;

        // The server rejected a token that looked valid: refresh once (rarely) and retry.
        if matches!(result, Err(FetchError::Unauthorized))
            && policy == RefreshPolicy::Allowed
            && !refreshed
        {
            let may_force = {
                let mut forced = self.forced_at.lock().unwrap();
                let ok = forced
                    .get(&view.key)
                    .is_none_or(|t| now - *t > FORCE_REFRESH_COOLDOWN_MS);
                if ok {
                    forced.insert(view.key.clone(), now);
                }
                ok
            };
            if may_force {
                match ensure_fresh(flavor, store, &ctx, Need::Force).await {
                    Ok(f) => {
                        refreshed = f.refreshed;
                        warning = f.warning.or(warning);
                        result = fetch(f.access_token).await;
                    }
                    Err(e) => return self.view_for_ensure_error(view, e, login_hint, policy, now),
                }
            }
        }

        match result {
            Ok(f) => {
                view.windows = f.windows;
                view.extras = f.extras;
                if f.plan.is_some() {
                    view.plan = f.plan;
                }
                view.state = StateKind::Ok;
                view.message = warning;
                view.fetched_at_ms = Some(now);
                view.token_refreshed = refreshed;
                view
            }
            Err(FetchError::Unauthorized) => {
                view.state = StateKind::LoginRequired;
                view.message = Some(format!("伺服器不接受這個登入。{}", login_hint.text));
                view.fix_command = Some(login_hint.command.clone());
                view
            }
            Err(FetchError::RateLimited { retry_after_secs }) => {
                let secs = retry_after_secs
                    .unwrap_or(MIN_BACKOFF_SECS)
                    .clamp(MIN_BACKOFF_SECS, MAX_BACKOFF_SECS);
                let why = "用量 API 暫時限流".to_string();
                self.retry_not_before
                    .lock()
                    .unwrap()
                    .insert(view.key.clone(), (now + secs as i64 * 1000, why.clone()));
                self.stale_or_fail(
                    view,
                    format!("{why}(約 {} 分鐘後自動再試)", secs.div_ceil(60)),
                    true,
                )
            }
            Err(e) => {
                let will_retry = matches!(e, FetchError::Network(_))
                    || matches!(e, FetchError::Http(c) if c >= 500);
                self.stale_or_fail(view, e.to_string(), will_retry)
            }
        }
    }

    fn view_for_ensure_error(
        &self,
        mut view: AccountView,
        e: EnsureError,
        login_hint: &LoginHint,
        policy: RefreshPolicy,
        now: i64,
    ) -> AccountView {
        match e {
            EnsureError::LoginRequired(m) => {
                view.state = StateKind::LoginRequired;
                view.message = Some(format!("{m}。{}", login_hint.text));
                view.fix_command = Some(login_hint.command.clone());
                view
            }
            EnsureError::Expired => {
                view.state = StateKind::TokenExpired;
                view.message = Some(match policy {
                    RefreshPolicy::SharedLogin => "access token 已過期。這個登入同時存在好幾個地方(帳號切換或官方 CLI),\
                        為了不弄壞它,面板不會去換它的 token。用一下這個帳號(開 claude 或 codex),它就會自己更新。"
                        .to_string(),
                    _ => "access token 已過期,而且「自動更新 token」目前是關閉的。到右上角「設定」開啟,就會自動處理。"
                        .to_string(),
                });
                view
            }
            EnsureError::Transient(m) => {
                // Do not keep knocking on a struggling token endpoint: 5, 10, then 15 minutes.
                let streak = {
                    let mut failures = self.refresh_failures.lock().unwrap();
                    let n = failures.entry(view.key.clone()).or_insert(0);
                    *n += 1;
                    *n
                };
                let wait_secs = 300 * u64::from(streak.min(3));
                self.retry_not_before
                    .lock()
                    .unwrap()
                    .insert(view.key.clone(), (now + wait_secs as i64 * 1000, m.clone()));
                self.stale_or_fail(
                    view,
                    format!("{m}(約 {} 分鐘後自動再試)", wait_secs / 60),
                    true,
                )
            }
        }
    }

    /// Keep showing the last good numbers (marked stale) when there are any. Otherwise show
    /// `Waiting` if the program will retry by itself, or `Error` for something unexpected.
    fn stale_or_fail(
        &self,
        mut view: AccountView,
        reason: String,
        will_retry: bool,
    ) -> AccountView {
        if let Some(mut prev) = self.last_good.lock().unwrap().get(&view.key).cloned() {
            prev.state = StateKind::Stale;
            prev.message = Some(reason);
            prev.token_refreshed = false;
            return prev;
        }
        view.state = if will_retry {
            StateKind::Waiting
        } else {
            StateKind::Error
        };
        view.message = Some(reason);
        view
    }

    fn lock_for(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default()
            .clone()
    }
}
