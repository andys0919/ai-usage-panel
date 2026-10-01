//! OAuth token refresh with a safe write-back to the credential files that Orca and the
//! official CLIs also use.
//!
//! Refresh tokens rotate: once the server issues a new one the old one is dead. So after a
//! successful refresh the new tokens MUST reach disk (or the recovery dir) or the account is
//! logged out. The rules, all covered by `tests/oauth_refresh.rs`:
//!
//! * a token that is still valid is never refreshed and the file is never touched;
//! * only one refresh per credential file runs at a time (in-process lock);
//! * the file is re-read after the network call; if somebody else changed it meanwhile we
//!   never overwrite their work (their valid token wins, otherwise ours is merged into theirs);
//! * writes go through a temp file + rename, retried, and fall back to a recovery copy.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};

use crate::fsutil::write_atomic;

pub const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Public OAuth client id of Claude Code (same value Orca uses for its own refresh).
pub const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Public OAuth client id of the Codex CLI (present in the installed codex.exe).
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_USER_AGENT: &str = "codex-cli";

const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_ATTEMPTS: u32 = 3;
const WRITE_RETRY_DELAY: Duration = Duration::from_millis(150);
/// Used when a refresh response forgets `expires_in`, so we never refresh in a tight loop.
const FALLBACK_LIFETIME_SECS: i64 = 1800;

// ---------------------------------------------------------------------------------------
// credential storage
// ---------------------------------------------------------------------------------------

/// Where the credentials live. The real implementation is a file; tests wrap it to inject
/// failures.
pub trait CredStore: Send + Sync {
    fn read(&self) -> io::Result<Vec<u8>>;
    fn write(&self, bytes: &[u8]) -> io::Result<()>;
    /// File-name-safe identifier, used to name the recovery copy.
    fn name(&self) -> String;
}

pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl CredStore for FileStore {
    fn read(&self) -> io::Result<Vec<u8>> {
        std::fs::read(&self.path)
    }

    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        write_atomic(&self.path, bytes)
    }

    fn name(&self) -> String {
        let parts: Vec<String> = self
            .path
            .components()
            .rev()
            .take(3)
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let joined = parts.into_iter().rev().collect::<Vec<_>>().join("-");
        joined
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// provider flavours
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TokenInfo {
    /// Empty string means "no usable access token" (treated as expired).
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unknown expiry (None) is treated as still valid.
    pub expires_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct RefreshResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_in_secs: Option<i64>,
    pub scope: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RefreshCallError {
    /// The server said no (bad / expired / already-used refresh token).
    #[error("{0}")]
    Rejected(String),
    /// Network trouble, rate limit or a server error: try again later.
    #[error("{0}")]
    Transient(String),
}

pub trait TokenFlavor: Send + Sync {
    /// Pull the token fields out of the provider's credential JSON.
    fn parse(&self, doc: &Value) -> Option<TokenInfo>;
    /// Exchange the refresh token for new tokens.
    fn refresh(
        &self,
        http: &reqwest::Client,
        refresh_token: &str,
    ) -> impl Future<Output = Result<RefreshResponse, RefreshCallError>> + Send;
    /// Merge the new tokens into the credential JSON, keeping every unrelated field.
    fn apply(&self, doc: &mut Value, resp: &RefreshResponse, now_ms: i64);
}

fn non_empty(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn as_millis(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

fn short(s: &str) -> String {
    s.chars().take(120).collect()
}

/// Turn an HTTP answer from a token endpoint into a `RefreshResponse` or an error.
fn classify_refresh_answer(
    status: reqwest::StatusCode,
    body: &str,
) -> Result<RefreshResponse, RefreshCallError> {
    let json: Option<Value> = serde_json::from_str(body).ok();
    if status.is_success() {
        let j =
            json.ok_or_else(|| RefreshCallError::Transient("token 伺服器回的不是 JSON".into()))?;
        let access_token = non_empty(j.get("access_token"))
            .ok_or_else(|| RefreshCallError::Transient("token 伺服器沒有回 access_token".into()))?;
        return Ok(RefreshResponse {
            access_token,
            refresh_token: non_empty(j.get("refresh_token")),
            id_token: non_empty(j.get("id_token")),
            expires_in_secs: as_millis(j.get("expires_in")),
            scope: non_empty(j.get("scope")),
        });
    }
    // Only the error code is ever surfaced, never the body: nothing secret can leak.
    let why = json
        .as_ref()
        .and_then(|j| {
            j.get("error")
                .and_then(Value::as_str)
                .or_else(|| j.pointer("/error/type").and_then(Value::as_str))
        })
        .map(short)
        .unwrap_or_default();
    match status.as_u16() {
        400 | 401 | 403 => Err(RefreshCallError::Rejected(
            format!("HTTP {} {why}", status.as_u16()).trim().to_string(),
        )),
        429 => Err(RefreshCallError::Transient(
            "token 伺服器暫時限流(HTTP 429)".to_string(),
        )),
        code => Err(RefreshCallError::Transient(
            format!("HTTP {code} {why}").trim().to_string(),
        )),
    }
}

fn net_error(e: &reqwest::Error) -> RefreshCallError {
    let kind = if e.is_timeout() {
        "逾時"
    } else if e.is_connect() {
        "連不上"
    } else {
        "網路錯誤"
    };
    RefreshCallError::Transient(format!("{kind}(refresh)"))
}

// ---- Claude ---------------------------------------------------------------------------

pub struct ClaudeFlavor {
    token_url: String,
}

impl ClaudeFlavor {
    pub fn new(token_url: String) -> Self {
        Self { token_url }
    }
}

impl Default for ClaudeFlavor {
    fn default() -> Self {
        Self::new(CLAUDE_TOKEN_URL.to_string())
    }
}

impl TokenFlavor for ClaudeFlavor {
    fn parse(&self, doc: &Value) -> Option<TokenInfo> {
        let o = doc.get("claudeAiOauth")?.as_object()?;
        Some(TokenInfo {
            access_token: non_empty(o.get("accessToken")).unwrap_or_default(),
            refresh_token: non_empty(o.get("refreshToken")),
            expires_at_ms: as_millis(o.get("expiresAt")),
        })
    }

    async fn refresh(
        &self,
        http: &reqwest::Client,
        refresh_token: &str,
    ) -> Result<RefreshResponse, RefreshCallError> {
        let resp = http
            .post(&self.token_url)
            .header("User-Agent", crate::http::USER_AGENT)
            .timeout(REFRESH_TIMEOUT)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", CLAUDE_CLIENT_ID),
            ])
            .send()
            .await
            .map_err(|e| net_error(&e))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        classify_refresh_answer(status, &body)
    }

    fn apply(&self, doc: &mut Value, resp: &RefreshResponse, now_ms: i64) {
        let Some(o) = doc.get_mut("claudeAiOauth").and_then(Value::as_object_mut) else {
            return;
        };
        o.insert("accessToken".into(), json!(resp.access_token));
        let secs = resp.expires_in_secs.unwrap_or(FALLBACK_LIFETIME_SECS);
        o.insert("expiresAt".into(), json!(now_ms + secs * 1000));
        if let Some(r) = &resp.refresh_token {
            o.insert("refreshToken".into(), json!(r));
        }
        if let Some(scope) = &resp.scope {
            let scopes: Vec<&str> = scope.split_whitespace().collect();
            if !scopes.is_empty() {
                o.insert("scopes".into(), json!(scopes));
            }
        }
    }
}

// ---- Codex ----------------------------------------------------------------------------

pub struct CodexFlavor {
    token_url: String,
}

impl CodexFlavor {
    pub fn new(token_url: String) -> Self {
        Self { token_url }
    }
}

impl Default for CodexFlavor {
    fn default() -> Self {
        Self::new(CODEX_TOKEN_URL.to_string())
    }
}

/// Decode the claims of a JWT without verifying it (we only read our own tokens).
pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn jwt_exp_ms(token: &str) -> Option<i64> {
    jwt_claims(token)?.get("exp")?.as_i64().map(|s| s * 1000)
}

impl TokenFlavor for CodexFlavor {
    fn parse(&self, doc: &Value) -> Option<TokenInfo> {
        let t = doc.get("tokens")?.as_object()?;
        let access = non_empty(t.get("access_token")).unwrap_or_default();
        Some(TokenInfo {
            expires_at_ms: jwt_exp_ms(&access),
            access_token: access,
            refresh_token: non_empty(t.get("refresh_token")),
        })
    }

    async fn refresh(
        &self,
        http: &reqwest::Client,
        refresh_token: &str,
    ) -> Result<RefreshResponse, RefreshCallError> {
        let resp = http
            .post(&self.token_url)
            .header("User-Agent", CODEX_USER_AGENT)
            .timeout(REFRESH_TIMEOUT)
            .json(&json!({
                "client_id": CODEX_CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "scope": "openid profile email",
            }))
            .send()
            .await
            .map_err(|e| net_error(&e))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        classify_refresh_answer(status, &body)
    }

    fn apply(&self, doc: &mut Value, resp: &RefreshResponse, now_ms: i64) {
        if let Some(t) = doc.get_mut("tokens").and_then(Value::as_object_mut) {
            t.insert("access_token".into(), json!(resp.access_token));
            if let Some(r) = &resp.refresh_token {
                t.insert("refresh_token".into(), json!(r));
            }
            if let Some(i) = &resp.id_token {
                t.insert("id_token".into(), json!(i));
            }
        }
        if let Some(now) = chrono::DateTime::from_timestamp_millis(now_ms) {
            doc["last_refresh"] = json!(now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
        }
    }
}

// ---------------------------------------------------------------------------------------
// the boundary
// ---------------------------------------------------------------------------------------

pub struct RefreshCtx<'a> {
    pub http: &'a reqwest::Client,
    /// One lock per credential file: serialises refreshes inside this process.
    pub lock: &'a tokio::sync::Mutex<()>,
    pub now_ms: i64,
    /// Refresh when the token expires within this many milliseconds.
    pub skew_ms: i64,
    pub recovery_dir: &'a Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    IfExpired,
    /// Refresh even if the token looks valid (used after the API answered 401).
    Force,
}

#[derive(Debug, Clone)]
pub struct Fresh {
    pub access_token: String,
    pub refreshed: bool,
    /// Something the user should know (e.g. the write-back failed).
    pub warning: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum EnsureError {
    /// Needs an interactive login; retrying will not help.
    #[error("{0}")]
    LoginRequired(String),
    /// Try again later.
    #[error("{0}")]
    Transient(String),
    /// The token is expired and refreshing is switched off (only returned by `peek`).
    #[error("access token 已過期")]
    Expired,
}

fn load<F: TokenFlavor, S: CredStore>(
    flavor: &F,
    store: &S,
) -> Result<(Vec<u8>, Value, TokenInfo), EnsureError> {
    let bytes = store
        .read()
        .map_err(|e| EnsureError::LoginRequired(format!("讀不到憑證檔({})", e.kind())))?;
    let text = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    let doc: Value = serde_json::from_slice(text)
        .map_err(|_| EnsureError::LoginRequired("憑證檔格式不對".into()))?;
    let info = flavor
        .parse(&doc)
        .ok_or_else(|| EnsureError::LoginRequired("憑證檔裡沒有 token".into()))?;
    Ok((bytes, doc, info))
}

fn needs_refresh(info: &TokenInfo, ctx: &RefreshCtx<'_>, need: Need) -> bool {
    match need {
        Need::Force => true,
        Need::IfExpired => {
            info.access_token.is_empty()
                || info
                    .expires_at_ms
                    .is_some_and(|e| e - ctx.now_ms <= ctx.skew_ms)
        }
    }
}

fn journal_path<S: CredStore>(store: &S, recovery_dir: &Path) -> PathBuf {
    recovery_dir.join(format!("{}.json", store.name()))
}

/// Write the merged credentials. The new tokens are journaled in the recovery dir FIRST, so a
/// crash between "the server issued them" and "the file has them" can never lose them; the
/// journal is removed once the write succeeded. Returns a warning when something went wrong.
async fn persist<S: CredStore>(store: &S, bytes: &[u8], recovery_dir: &Path) -> Option<String> {
    let journal = journal_path(store, recovery_dir);
    let journaled =
        std::fs::create_dir_all(recovery_dir).and_then(|()| write_atomic(&journal, bytes));

    let mut last_err = None;
    for attempt in 0..WRITE_ATTEMPTS {
        match store.write(bytes) {
            Ok(()) => {
                if journaled.is_ok() {
                    let _ = std::fs::remove_file(&journal);
                }
                return None;
            }
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < WRITE_ATTEMPTS {
                    tokio::time::sleep(WRITE_RETRY_DELAY).await;
                }
            }
        }
    }
    let err = last_err.map(|e| e.kind().to_string()).unwrap_or_default();
    Some(match journaled {
        Ok(()) => format!(
            "token 已更新,但寫回憑證檔失敗({err});新 token 已另存到 {}",
            journal.display()
        ),
        Err(e) => format!(
            "token 已更新,但寫回憑證檔失敗({err}),連備份也失敗({})",
            e.kind()
        ),
    })
}

/// A journal means an earlier run got new tokens from the server but never finished writing
/// them (crash, locked file). Put them back when they are newer than what the file holds;
/// throw the journal away when the file is already newer.
fn replay_journal<F: TokenFlavor, S: CredStore>(
    flavor: &F,
    store: &S,
    recovery_dir: &Path,
) -> Option<String> {
    let journal = journal_path(store, recovery_dir);
    let bytes = std::fs::read(&journal).ok()?;
    let text = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    let theirs = serde_json::from_slice::<Value>(text)
        .ok()
        .and_then(|doc| flavor.parse(&doc))?;
    let current = load(flavor, store).ok().map(|(_, _, info)| info);

    let restore = match (&current, theirs.expires_at_ms) {
        (None, _) => true,
        (Some(cur), Some(j)) => {
            if cur.expires_at_ms.is_none_or(|c| j > c) {
                true
            } else {
                let _ = std::fs::remove_file(&journal); // the file is newer: the journal is stale
                return None;
            }
        }
        (Some(_), None) => return None,
    };
    if restore && store.write(&bytes).is_ok() {
        let _ = std::fs::remove_file(&journal);
        return Some(
            "上次換好的新 token 還沒寫進憑證檔(程式中斷或檔案被鎖住),已從備份還原".to_string(),
        );
    }
    None // could not write: keep the journal and try again next time
}

/// Read the token without ever refreshing or writing (used when auto-refresh is switched off).
pub fn peek<F: TokenFlavor, S: CredStore>(
    flavor: &F,
    store: &S,
    ctx: &RefreshCtx<'_>,
) -> Result<Fresh, EnsureError> {
    let (_, _, info) = load(flavor, store)?;
    if needs_refresh(&info, ctx, Need::IfExpired) {
        return Err(EnsureError::Expired);
    }
    Ok(Fresh {
        access_token: info.access_token,
        refreshed: false,
        warning: None,
    })
}

/// Return an access token that is valid right now, refreshing (and writing back) if needed.
pub async fn ensure_fresh<F: TokenFlavor, S: CredStore>(
    flavor: &F,
    store: &S,
    ctx: &RefreshCtx<'_>,
    need: Need,
) -> Result<Fresh, EnsureError> {
    let _guard = ctx.lock.lock().await;
    let restored = replay_journal(flavor, store, ctx.recovery_dir);
    let mut fresh = refresh_locked(flavor, store, ctx, need).await?;
    if fresh.warning.is_none() {
        fresh.warning = restored;
    }
    Ok(fresh)
}

async fn refresh_locked<F: TokenFlavor, S: CredStore>(
    flavor: &F,
    store: &S,
    ctx: &RefreshCtx<'_>,
    need: Need,
) -> Result<Fresh, EnsureError> {
    let (bytes0, doc0, info0) = load(flavor, store)?;
    if !needs_refresh(&info0, ctx, need) {
        return Ok(Fresh {
            access_token: info0.access_token,
            refreshed: false,
            warning: None,
        });
    }
    let Some(refresh_token) = info0.refresh_token.clone() else {
        return Err(EnsureError::LoginRequired("沒有 refresh token".into()));
    };

    match flavor.refresh(ctx.http, &refresh_token).await {
        Err(RefreshCallError::Transient(m)) => Err(EnsureError::Transient(m)),
        Err(RefreshCallError::Rejected(m)) => {
            // The CLI (or Orca) may have rotated the token first; if so its file already
            // holds a newer valid token and our old refresh token is simply dead.
            if let Ok((_, _, theirs)) = load(flavor, store) {
                if theirs.access_token != info0.access_token
                    && !needs_refresh(&theirs, ctx, Need::IfExpired)
                {
                    return Ok(Fresh {
                        access_token: theirs.access_token,
                        refreshed: false,
                        warning: None,
                    });
                }
            }
            Err(EnsureError::LoginRequired(format!(
                "refresh token 被拒絕({m})"
            )))
        }
        Ok(resp) => {
            // Did somebody else change the file while we were talking to the server?
            let (bytes1, mut doc1, info1) = match load(flavor, store) {
                Ok(latest) => latest,
                Err(_) => (bytes0.clone(), doc0, info0.clone()),
            };
            if bytes1 != bytes0
                && info1.access_token != info0.access_token
                && !needs_refresh(&info1, ctx, Need::IfExpired)
            {
                return Ok(Fresh {
                    access_token: info1.access_token,
                    refreshed: false,
                    warning: None,
                });
            }
            flavor.apply(&mut doc1, &resp, ctx.now_ms);
            // The server already rotated the tokens: nothing after this point may fail early.
            let out =
                serde_json::to_vec_pretty(&doc1).unwrap_or_else(|_| doc1.to_string().into_bytes());
            let warning = persist(store, &out, ctx.recovery_dir).await;
            Ok(Fresh {
                access_token: resp.access_token,
                refreshed: true,
                warning,
            })
        }
    }
}
