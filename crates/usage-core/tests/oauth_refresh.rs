//! Boundary under test: `oauth::ensure_fresh`.
//!
//! It refreshes an OAuth token and writes the result back into the credential file that
//! Orca and the official CLIs also read. Refresh tokens ROTATE (the old one dies when the
//! new one is issued), so a bug here can log the user out of a real account. Every failure
//! mode at this boundary therefore has a test:
//!
//!  1. token still valid                         -> no network call, file byte-identical
//!  2. token expired                             -> refreshed, written back, other fields survive
//!  3. token expiring inside the skew window     -> refreshed
//!  4. response carries no new refresh token     -> old refresh token kept
//!  5. server rejects (invalid_grant)            -> LoginRequired, file untouched
//!  6. rejected, but the CLI already refreshed   -> adopt the CLI's newer token
//!  7. 5xx / connection refused                  -> Transient, file untouched
//!  8. file replaced by someone else mid-refresh -> their valid token adopted, never overwritten
//!  9. same, but their token is still stale      -> OUR tokens merged into THEIR file (never lost)
//! 10. five concurrent callers                   -> exactly one refresh request
//! 11. missing / malformed / token-less file     -> LoginRequired, no panic, no write, no request
//! 12. write fails                               -> tokens saved to recovery dir + warning, still usable
//! 13. write is atomic                           -> no temp files left, valid JSON
//! 14. Need::Force                               -> refreshes even though the token looks valid
//! 15. Codex shape                               -> JWT exp drives expiry, JSON body, other fields survive
//! 16. crash safety                              -> new tokens journaled BEFORE the write, journal removed after it

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use tempfile::TempDir;
use usage_core::oauth::{
    ensure_fresh, ClaudeFlavor, CodexFlavor, CredStore, EnsureError, FileStore, Need, RefreshCtx,
};
use wiremock::matchers::{body_json, body_string_contains, header, header_regex, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const NOW: i64 = 1_790_000_000_000;
const SKEW: i64 = 300_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 86_400_000;

fn claude_doc(access: &str, refresh: &str, expires_at: i64) -> Value {
    json!({
        "claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": expires_at,
            "refreshTokenExpiresAt": NOW + 20 * DAY,
            "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max",
            "rateLimitTier": "default_claude_max_20x",
            "futureField": { "keep": true }
        },
        "otherTopLevel": 1
    })
}

fn jwt(exp_secs: i64) -> String {
    let h = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let p = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp_secs}}}"#));
    format!("{h}.{p}.sig")
}

fn codex_doc(access_jwt: &str, refresh: &str) -> Value {
    json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "old-id",
            "access_token": access_jwt,
            "refresh_token": refresh,
            "account_id": "acct-1"
        },
        "last_refresh": "2026-09-21T02:52:20.575514200Z"
    })
}

fn write_json(p: &Path, v: &Value) {
    std::fs::write(p, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

fn read_json(p: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

struct Fixture {
    _dir: TempDir,
    auth_dir: PathBuf,
    creds: PathBuf,
    recovery: PathBuf,
    server: MockServer,
    http: reqwest::Client,
    lock: tokio::sync::Mutex<()>,
}

impl Fixture {
    async fn new(file_name: &str, doc: &Value) -> Self {
        let dir = TempDir::new().unwrap();
        let auth_dir = dir.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        let creds = auth_dir.join(file_name);
        write_json(&creds, doc);
        Self {
            recovery: dir.path().join("recovery"),
            _dir: dir,
            auth_dir,
            creds,
            server: MockServer::start().await,
            http: reqwest::Client::new(),
            lock: tokio::sync::Mutex::new(()),
        }
    }

    async fn claude(doc: &Value) -> Self {
        Self::new(".credentials.json", doc).await
    }

    async fn codex(doc: &Value) -> Self {
        Self::new("auth.json", doc).await
    }

    fn claude_flavor(&self) -> ClaudeFlavor {
        ClaudeFlavor::new(format!("{}/v1/oauth/token", self.server.uri()))
    }

    fn codex_flavor(&self) -> CodexFlavor {
        CodexFlavor::new(format!("{}/oauth/token", self.server.uri()))
    }

    fn ctx(&self) -> RefreshCtx<'_> {
        RefreshCtx {
            http: &self.http,
            lock: &self.lock,
            now_ms: NOW,
            skew_ms: SKEW,
            recovery_dir: &self.recovery,
        }
    }

    async fn requests(&self) -> usize {
        self.server.received_requests().await.unwrap().len()
    }

    async fn mount_claude_ok(&self, access: &str, refresh: Option<&str>) {
        let mut body = json!({
            "access_token": access,
            "expires_in": 28800,
            "scope": "user:inference user:profile user:file_upload"
        });
        if let Some(r) = refresh {
            body["refresh_token"] = json!(r);
        }
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }
}

/// Runs a side effect (e.g. "somebody else rewrites the credential file") while the refresh
/// request is in flight, then answers with `resp`.
struct SideEffect<F: Fn() + Send + Sync + 'static> {
    effect: F,
    resp: ResponseTemplate,
}

impl<F: Fn() + Send + Sync + 'static> Respond for SideEffect<F> {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        (self.effect)();
        self.resp.clone()
    }
}

// 1 ---------------------------------------------------------------------------------------
#[tokio::test]
async fn valid_token_needs_no_network_and_leaves_file_untouched() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW + HOUR)).await;
    let before = std::fs::read(&fx.creds).unwrap();

    let got = ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    assert_eq!(got.access_token, "acc-old");
    assert!(!got.refreshed);
    assert!(got.warning.is_none());
    assert_eq!(fx.requests().await, 0);
    assert_eq!(std::fs::read(&fx.creds).unwrap(), before);
}

// 2 ---------------------------------------------------------------------------------------
#[tokio::test]
async fn expired_token_is_refreshed_written_back_and_unrelated_fields_survive() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW - HOUR)).await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=ref-old"))
        .and(body_string_contains(
            "client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "acc-new",
            "refresh_token": "ref-new",
            "expires_in": 28800,
            "scope": "user:inference user:profile user:file_upload"
        })))
        .expect(1)
        .mount(&fx.server)
        .await;

    let got = ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    assert_eq!(got.access_token, "acc-new");
    assert!(got.refreshed);
    assert!(got.warning.is_none());
    let doc = read_json(&fx.creds);
    let o = &doc["claudeAiOauth"];
    assert_eq!(o["accessToken"], "acc-new");
    assert_eq!(o["refreshToken"], "ref-new");
    assert_eq!(o["expiresAt"], NOW + 28_800_000);
    assert_eq!(
        o["scopes"],
        json!(["user:inference", "user:profile", "user:file_upload"])
    );
    assert_eq!(o["subscriptionType"], "max");
    assert_eq!(o["rateLimitTier"], "default_claude_max_20x");
    assert_eq!(o["refreshTokenExpiresAt"], NOW + 20 * DAY);
    assert_eq!(o["futureField"], json!({ "keep": true }));
    assert_eq!(doc["otherTopLevel"], 1);
}

// 3 ---------------------------------------------------------------------------------------
#[tokio::test]
async fn token_expiring_inside_the_skew_window_is_refreshed() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW + 120_000)).await;
    fx.mount_claude_ok("acc-new", Some("ref-new")).await;

    let got = ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    assert_eq!(got.access_token, "acc-new");
    assert!(got.refreshed);
    assert_eq!(fx.requests().await, 1);
}

// 4 ---------------------------------------------------------------------------------------
#[tokio::test]
async fn response_without_new_refresh_token_keeps_the_old_one() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW - HOUR)).await;
    fx.mount_claude_ok("acc-new", None).await;

    ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    let doc = read_json(&fx.creds);
    assert_eq!(doc["claudeAiOauth"]["accessToken"], "acc-new");
    assert_eq!(doc["claudeAiOauth"]["refreshToken"], "ref-old");
}

// 5 ---------------------------------------------------------------------------------------
#[tokio::test]
async fn rejected_refresh_means_login_required_and_file_is_untouched() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW - HOUR)).await;
    let before = std::fs::read(&fx.creds).unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "error": "invalid_grant" })))
        .expect(1)
        .mount(&fx.server)
        .await;

    let err = ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, EnsureError::LoginRequired(_)), "got {err:?}");
    assert_eq!(std::fs::read(&fx.creds).unwrap(), before);
}

// 6 ---------------------------------------------------------------------------------------
#[tokio::test]
async fn rejected_refresh_adopts_the_token_the_cli_refreshed_in_the_meantime() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW - HOUR)).await;
    let creds = fx.creds.clone();
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(SideEffect {
            effect: move || {
                write_json(
                    &creds,
                    &claude_doc("acc-theirs", "ref-theirs", NOW + 8 * HOUR),
                )
            },
            resp: ResponseTemplate::new(400).set_body_json(json!({ "error": "invalid_grant" })),
        })
        .mount(&fx.server)
        .await;

    let got = ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    assert_eq!(got.access_token, "acc-theirs");
    assert!(!got.refreshed);
    assert_eq!(
        read_json(&fx.creds)["claudeAiOauth"]["accessToken"],
        "acc-theirs"
    );
}

// 7 ---------------------------------------------------------------------------------------
#[tokio::test]
async fn server_error_or_unreachable_server_is_transient_and_file_is_untouched() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW - HOUR)).await;
    let before = std::fs::read(&fx.creds).unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&fx.server)
        .await;

    let err = ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, EnsureError::Transient(_)), "got {err:?}");

    let dead = ClaudeFlavor::new("http://127.0.0.1:1/v1/oauth/token".to_string());
    let err = ensure_fresh(
        &dead,
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, EnsureError::Transient(_)), "got {err:?}");

    assert_eq!(std::fs::read(&fx.creds).unwrap(), before);
}

// 8 ---------------------------------------------------------------------------------------
#[tokio::test]
async fn file_replaced_mid_refresh_with_a_valid_token_is_adopted_not_overwritten() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW - HOUR)).await;
    let creds = fx.creds.clone();
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(SideEffect {
            effect: move || {
                write_json(
                    &creds,
                    &claude_doc("acc-theirs", "ref-theirs", NOW + 8 * HOUR),
                )
            },
            resp: ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "acc-ours", "refresh_token": "ref-ours", "expires_in": 28800
            })),
        })
        .mount(&fx.server)
        .await;

    let got = ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    assert_eq!(got.access_token, "acc-theirs");
    assert!(!got.refreshed);
    let doc = read_json(&fx.creds);
    assert_eq!(doc["claudeAiOauth"]["accessToken"], "acc-theirs");
    assert_eq!(doc["claudeAiOauth"]["refreshToken"], "ref-theirs");
}

// 9 ---------------------------------------------------------------------------------------
#[tokio::test]
async fn file_replaced_mid_refresh_with_a_stale_token_gets_our_tokens_merged_in() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW - HOUR)).await;
    let creds = fx.creds.clone();
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(SideEffect {
            effect: move || {
                let mut theirs = claude_doc("acc-theirs-stale", "ref-theirs", NOW - 2 * HOUR);
                theirs["otherTopLevel"] = json!(2);
                write_json(&creds, &theirs);
            },
            resp: ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "acc-ours", "refresh_token": "ref-ours", "expires_in": 28800
            })),
        })
        .mount(&fx.server)
        .await;

    let got = ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    assert_eq!(got.access_token, "acc-ours");
    assert!(got.refreshed);
    let doc = read_json(&fx.creds);
    assert_eq!(doc["claudeAiOauth"]["accessToken"], "acc-ours");
    assert_eq!(doc["claudeAiOauth"]["refreshToken"], "ref-ours");
    assert_eq!(
        doc["otherTopLevel"], 2,
        "their unrelated change must survive"
    );
}

// 10 --------------------------------------------------------------------------------------
#[tokio::test]
async fn five_concurrent_callers_cause_exactly_one_refresh_request() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW - HOUR)).await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(300))
                .set_body_json(json!({
                    "access_token": "acc-new", "refresh_token": "ref-new", "expires_in": 28800
                })),
        )
        .expect(1)
        .mount(&fx.server)
        .await;

    let flavor = fx.claude_flavor();
    let store = FileStore::new(&fx.creds);
    let ctx = fx.ctx();
    let results = futures::future::join_all(
        (0..5).map(|_| ensure_fresh(&flavor, &store, &ctx, Need::IfExpired)),
    )
    .await;

    for r in results {
        assert_eq!(r.unwrap().access_token, "acc-new");
    }
    assert_eq!(fx.requests().await, 1);
}

// 11 --------------------------------------------------------------------------------------
#[tokio::test]
async fn missing_malformed_or_tokenless_files_are_login_required_without_side_effects() {
    let fx = Fixture::claude(&claude_doc("a", "r", NOW - HOUR)).await;
    let flavor = fx.claude_flavor();
    let store = FileStore::new(&fx.creds);

    // (a) malformed JSON
    std::fs::write(&fx.creds, b"{ not json").unwrap();
    let err = ensure_fresh(&flavor, &store, &fx.ctx(), Need::IfExpired)
        .await
        .unwrap_err();
    assert!(
        matches!(err, EnsureError::LoginRequired(_)),
        "malformed: {err:?}"
    );
    assert_eq!(std::fs::read(&fx.creds).unwrap(), b"{ not json");

    // (b) valid JSON, no claudeAiOauth block
    write_json(&fx.creds, &json!({ "something": "else" }));
    let err = ensure_fresh(&flavor, &store, &fx.ctx(), Need::IfExpired)
        .await
        .unwrap_err();
    assert!(
        matches!(err, EnsureError::LoginRequired(_)),
        "no block: {err:?}"
    );

    // (c) expired and no refresh token
    let mut doc = claude_doc("acc-old", "x", NOW - HOUR);
    doc["claudeAiOauth"]
        .as_object_mut()
        .unwrap()
        .remove("refreshToken");
    write_json(&fx.creds, &doc);
    let err = ensure_fresh(&flavor, &store, &fx.ctx(), Need::IfExpired)
        .await
        .unwrap_err();
    assert!(
        matches!(err, EnsureError::LoginRequired(_)),
        "no refresh token: {err:?}"
    );

    // (d) file missing
    std::fs::remove_file(&fx.creds).unwrap();
    let err = ensure_fresh(&flavor, &store, &fx.ctx(), Need::IfExpired)
        .await
        .unwrap_err();
    assert!(
        matches!(err, EnsureError::LoginRequired(_)),
        "missing: {err:?}"
    );

    assert_eq!(fx.requests().await, 0);
}

// 12 --------------------------------------------------------------------------------------
struct WriteAlwaysFails(FileStore);

impl CredStore for WriteAlwaysFails {
    fn read(&self) -> io::Result<Vec<u8>> {
        self.0.read()
    }
    fn write(&self, _: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "locked by another process",
        ))
    }
    fn name(&self) -> String {
        self.0.name()
    }
}

#[tokio::test]
async fn failed_write_saves_new_tokens_to_the_recovery_dir_and_still_returns_them() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW - HOUR)).await;
    let before = std::fs::read(&fx.creds).unwrap();
    fx.mount_claude_ok("acc-new", Some("ref-new")).await;

    let got = ensure_fresh(
        &fx.claude_flavor(),
        &WriteAlwaysFails(FileStore::new(&fx.creds)),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    assert_eq!(got.access_token, "acc-new");
    assert!(got.refreshed);
    assert!(
        got.warning.is_some(),
        "the user must be told the write failed"
    );
    assert_eq!(
        std::fs::read(&fx.creds).unwrap(),
        before,
        "original must be untouched"
    );
    let saved: Vec<_> = std::fs::read_dir(&fx.recovery).unwrap().collect();
    assert_eq!(saved.len(), 1);
    let doc = read_json(&saved[0].as_ref().unwrap().path());
    assert_eq!(doc["claudeAiOauth"]["refreshToken"], "ref-new");
    assert_eq!(doc["claudeAiOauth"]["accessToken"], "acc-new");
}

// 13 --------------------------------------------------------------------------------------
#[tokio::test]
async fn write_is_atomic_no_temp_files_remain_and_json_is_valid() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW - HOUR)).await;
    fx.mount_claude_ok("acc-new", Some("ref-new")).await;

    ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    let names: Vec<String> = std::fs::read_dir(&fx.auth_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec![".credentials.json".to_string()],
        "leftovers: {names:?}"
    );
    assert_eq!(
        read_json(&fx.creds)["claudeAiOauth"]["accessToken"],
        "acc-new"
    );
}

// 14 --------------------------------------------------------------------------------------
#[tokio::test]
async fn force_refreshes_even_when_the_token_looks_valid() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW + 6 * HOUR)).await;
    fx.mount_claude_ok("acc-new", Some("ref-new")).await;

    let got = ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::Force,
    )
    .await
    .unwrap();

    assert_eq!(got.access_token, "acc-new");
    assert!(got.refreshed);
    assert_eq!(fx.requests().await, 1);
}

// 15 --------------------------------------------------------------------------------------
#[tokio::test]
async fn codex_valid_jwt_needs_no_network() {
    let fresh_jwt = jwt(NOW / 1000 + 6 * 24 * 3600);
    let fx = Fixture::codex(&codex_doc(&fresh_jwt, "ref-old")).await;

    let got = ensure_fresh(
        &fx.codex_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    assert_eq!(got.access_token, fresh_jwt);
    assert!(!got.refreshed);
    assert_eq!(fx.requests().await, 0);
}

#[tokio::test]
async fn codex_expired_jwt_is_refreshed_with_a_json_body_and_written_back() {
    let expired_jwt = jwt(NOW / 1000 - 3600);
    let new_jwt = jwt(NOW / 1000 + 10 * 24 * 3600);
    let fx = Fixture::codex(&codex_doc(&expired_jwt, "ref-old")).await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(json!({
            "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
            "grant_type": "refresh_token",
            "refresh_token": "ref-old",
            "scope": "openid profile email"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id_token": "new-id", "access_token": new_jwt, "refresh_token": "ref-new"
        })))
        .expect(1)
        .mount(&fx.server)
        .await;

    let got = ensure_fresh(
        &fx.codex_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    assert_eq!(got.access_token, new_jwt);
    assert!(got.refreshed);
    let doc = read_json(&fx.creds);
    assert_eq!(doc["tokens"]["access_token"], new_jwt);
    assert_eq!(doc["tokens"]["refresh_token"], "ref-new");
    assert_eq!(doc["tokens"]["id_token"], "new-id");
    assert_eq!(doc["tokens"]["account_id"], "acct-1");
    assert_eq!(doc["auth_mode"], "chatgpt");
    assert!(doc["OPENAI_API_KEY"].is_null());
    let last = doc["last_refresh"].as_str().unwrap();
    assert_ne!(last, "2026-09-21T02:52:20.575514200Z");
    assert!(
        last.ends_with('Z') && last.contains('T'),
        "RFC3339 expected, got {last}"
    );
}

// 16 --------------------------------------------------------------------------------------
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Records whether the journal already existed at the moment the credential file is written.
struct WatchesJournal {
    inner: FileStore,
    journal: PathBuf,
    existed_during_write: Arc<AtomicBool>,
}

impl CredStore for WatchesJournal {
    fn read(&self) -> io::Result<Vec<u8>> {
        self.inner.read()
    }
    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        self.existed_during_write
            .store(self.journal.exists(), Ordering::SeqCst);
        self.inner.write(bytes)
    }
    fn name(&self) -> String {
        self.inner.name()
    }
}

#[tokio::test]
async fn new_tokens_are_journaled_before_the_write_and_the_journal_is_removed_after_it() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW - HOUR)).await;
    fx.mount_claude_ok("acc-new", Some("ref-new")).await;
    let inner = FileStore::new(&fx.creds);
    let journal = fx.recovery.join(format!("{}.json", inner.name()));
    let seen = Arc::new(AtomicBool::new(false));
    let store = WatchesJournal {
        inner,
        journal: journal.clone(),
        existed_during_write: seen.clone(),
    };

    let got = ensure_fresh(&fx.claude_flavor(), &store, &fx.ctx(), Need::IfExpired)
        .await
        .unwrap();

    assert!(got.refreshed);
    assert!(got.warning.is_none());
    assert!(
        seen.load(Ordering::SeqCst),
        "the rotated tokens must already be safe on disk BEFORE the credential file is touched"
    );
    assert!(
        !journal.exists(),
        "the journal must be removed after a successful write"
    );
    assert_eq!(
        read_json(&fx.creds)["claudeAiOauth"]["refreshToken"],
        "ref-new"
    );
}

// 17 --------------------------------------------------------------------------------------
fn journal_path(fx: &Fixture) -> PathBuf {
    fx.recovery
        .join(format!("{}.json", FileStore::new(&fx.creds).name()))
}

#[tokio::test]
async fn a_journal_left_by_a_crash_is_replayed_into_the_credential_file() {
    // The previous run got new tokens from the server, wrote the journal, then died before
    // the credential file was updated: the file still holds the (now dead) old tokens.
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW - HOUR)).await;
    std::fs::create_dir_all(&fx.recovery).unwrap();
    let journal = journal_path(&fx);
    write_json(
        &journal,
        &claude_doc("acc-journal", "ref-journal", NOW + 8 * HOUR),
    );

    let got = ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    assert_eq!(got.access_token, "acc-journal");
    assert!(
        !got.refreshed,
        "nothing new was fetched, tokens were restored"
    );
    assert!(
        got.warning.is_some(),
        "the user should hear that a restore happened"
    );
    assert_eq!(fx.requests().await, 0, "no token request is needed");
    let doc = read_json(&fx.creds);
    assert_eq!(doc["claudeAiOauth"]["accessToken"], "acc-journal");
    assert_eq!(doc["claudeAiOauth"]["refreshToken"], "ref-journal");
    assert!(
        !journal.exists(),
        "the journal is removed once it has been replayed"
    );
}

// 18 --------------------------------------------------------------------------------------
#[tokio::test]
async fn a_journal_older_than_the_credential_file_is_ignored_and_removed() {
    let fx = Fixture::claude(&claude_doc("acc-file", "ref-file", NOW + 6 * HOUR)).await;
    let before = std::fs::read(&fx.creds).unwrap();
    std::fs::create_dir_all(&fx.recovery).unwrap();
    let journal = journal_path(&fx);
    write_json(&journal, &claude_doc("acc-j-old", "ref-j-old", NOW + HOUR));

    let got = ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    assert_eq!(got.access_token, "acc-file");
    assert!(got.warning.is_none());
    assert_eq!(
        std::fs::read(&fx.creds).unwrap(),
        before,
        "the newer file must not be overwritten"
    );
    assert!(!journal.exists(), "a stale journal is cleaned up");
}

// 19 --------------------------------------------------------------------------------------
/// The Claude token endpoint answers HTTP 429 to requests whose User-Agent is `claude-code/*`
/// (measured 2026-10-01 with a fake refresh token: that UA -> 429, while `node`, `axios/*`,
/// `claude-cli/*` and a custom name -> the normal 400 invalid_grant). Third-party tools copy
/// that string, so it is a hot bucket. Sending it makes every real refresh fail with 429.
#[tokio::test]
async fn claude_refresh_uses_an_honest_user_agent_never_the_throttled_claude_code_one() {
    let fx = Fixture::claude(&claude_doc("acc-old", "ref-old", NOW - HOUR)).await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(header_regex("user-agent", r"^ai-usage-panel/\d"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "acc-new", "refresh_token": "ref-new", "expires_in": 28800
        })))
        .expect(1)
        .mount(&fx.server)
        .await;

    let got = ensure_fresh(
        &fx.claude_flavor(),
        &FileStore::new(&fx.creds),
        &fx.ctx(),
        Need::IfExpired,
    )
    .await
    .unwrap();

    assert!(got.refreshed);
    assert_eq!(got.access_token, "acc-new");
}
