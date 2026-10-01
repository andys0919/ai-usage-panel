//! Boundary under test: which credential file the collector reads, and when it is allowed to
//! refresh a token. This protects the user's logins when they switch accounts:
//!
//! The SAME login can exist in two copies (Orca's managed folder and the CLI's own home
//! folder). Refresh tokens rotate, so refreshing one copy kills the other copy's refresh
//! token and the user's running CLI would drop its login. Therefore:
//!
//!  S1 shared login, managed copy stale but the CLI's copy fresh -> the fresh copy is read,
//!     nothing is refreshed, no file is touched
//!  S2 shared login, every copy expired -> state TokenExpired, NO token request, NO usage
//!     request, files untouched
//!  S3 NOT shared (managed login is not the active one) + expired -> refreshed, written back,
//!     usage fetched with the new token
//!  S4 Codex: the CLI's default home holds the same identity as an Orca-managed home ->
//!     treated as ONE shared account, fresh copy read, no refresh
//!  S5 Codex: different identities -> two accounts; only the expired one is refreshed
//!  S6 refresh switched off in settings -> expired token is reported, never refreshed

use std::path::PathBuf;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use tempfile::TempDir;
use usage_core::accounts::Roots;
use usage_core::collector::{Collector, CollectorConfig, Endpoints};
use usage_core::model::{AccountView, StateKind};
use usage_core::util::now_ms;
use wiremock::matchers::{header, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HOUR: i64 = 3_600_000;

struct World {
    _tmp: TempDir,
    roots: Roots,
    server: MockServer,
}

fn write_json(p: &PathBuf, v: &Value) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

fn jwt(claims: &Value) -> String {
    let h = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let p = URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("{h}.{p}.sig")
}

fn claude_doc(access: &str, refresh: &str, expires_at: i64) -> Value {
    json!({ "claudeAiOauth": {
        "accessToken": access, "refreshToken": refresh, "expiresAt": expires_at,
        "refreshTokenExpiresAt": now_ms() + 20 * 24 * HOUR,
        "scopes": ["user:inference", "user:profile"],
        "subscriptionType": "max", "rateLimitTier": "default_claude_max_20x"
    }})
}

impl World {
    async fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let roots = Roots {
            home: tmp.path().join("home"),
            appdata: tmp.path().join("AppData").join("Roaming"),
            localappdata: tmp.path().join("AppData").join("Local"),
        };
        for d in [&roots.home, &roots.appdata, &roots.localappdata] {
            std::fs::create_dir_all(d).unwrap();
        }
        Self {
            _tmp: tmp,
            roots,
            server: MockServer::start().await,
        }
    }

    fn cfg(&self, only: &str, auto_refresh_tokens: bool) -> CollectorConfig {
        let base = self.server.uri();
        CollectorConfig {
            roots: self.roots.clone(),
            endpoints: Endpoints {
                claude_usage: format!("{base}/api/oauth/usage"),
                claude_token: format!("{base}/v1/oauth/token"),
                codex_usage: format!("{base}/wham/usage"),
                codex_token: format!("{base}/oauth/token"),
            },
            auto_refresh_tokens,
            recovery_dir: self.roots.appdata.join("recovery"),
            only: Some(only.to_string()),
        }
    }

    async fn collect(&self, only: &str, auto_refresh_tokens: bool) -> Vec<AccountView> {
        let collector = Collector::new();
        let mut views = Vec::new();
        collector
            .collect(&self.cfg(only, auto_refresh_tokens), |v| views.push(v))
            .await;
        views
    }

    // ---- Claude fixtures ----
    fn claude_managed(&self, id: &str, uuid: &str, email: &str, doc: &Value) -> PathBuf {
        let auth = self
            .roots
            .orca_dir()
            .join("claude-accounts")
            .join(id)
            .join("auth");
        write_json(
            &auth.join("oauth-account.json"),
            &json!({
                "accountUuid": uuid, "emailAddress": email, "organizationName": "Org"
            }),
        );
        let creds = auth.join(".credentials.json");
        write_json(&creds, doc);
        creds
    }

    /// The CLI's own login: ~/.claude/.credentials.json + the active account in ~/.claude.json
    fn claude_system(&self, uuid: &str, email: &str, doc: &Value) -> PathBuf {
        write_json(
            &self.roots.home.join(".claude.json"),
            &json!({ "oauthAccount": { "accountUuid": uuid, "emailAddress": email } }),
        );
        let creds = self.roots.home.join(".claude").join(".credentials.json");
        write_json(&creds, doc);
        creds
    }

    async fn mock_claude_usage(&self, bearer: &str, calls: u64) {
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .and(header("authorization", format!("Bearer {bearer}").as_str()))
            .and(header_regex("user-agent", r"^ai-usage-panel/\d"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("fixtures/claude_usage_real.json"))
                    .insert_header("content-type", "application/json"),
            )
            .expect(calls)
            .mount(&self.server)
            .await;
    }

    async fn mock_claude_token(&self, calls: u64) {
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "acc-new", "refresh_token": "ref-new", "expires_in": 28800,
                "scope": "user:inference user:profile"
            })))
            .expect(calls)
            .mount(&self.server)
            .await;
    }

    // ---- Codex fixtures ----
    fn codex_auth(
        &self,
        file: PathBuf,
        email: &str,
        account_id: &str,
        access_exp_secs: i64,
        tag: &str,
    ) -> (PathBuf, String) {
        let access = jwt(&json!({ "exp": access_exp_secs, "tag": tag }));
        write_json(
            &file,
            &json!({
                "auth_mode": "chatgpt", "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": jwt(&json!({ "email": email, "exp": access_exp_secs - 86_400 })),
                    "access_token": access, "refresh_token": format!("ref-{tag}"), "account_id": account_id
                },
                "last_refresh": "2026-09-21T02:52:20.575514200Z"
            }),
        );
        (file, access)
    }

    fn codex_managed(
        &self,
        id: &str,
        email: &str,
        account_id: &str,
        exp: i64,
        tag: &str,
    ) -> (PathBuf, String) {
        let file = self
            .roots
            .orca_dir()
            .join("codex-accounts")
            .join(id)
            .join("home")
            .join("auth.json");
        self.codex_auth(file, email, account_id, exp, tag)
    }

    fn codex_system(
        &self,
        email: &str,
        account_id: &str,
        exp: i64,
        tag: &str,
    ) -> (PathBuf, String) {
        let file = self.roots.home.join(".codex").join("auth.json");
        self.codex_auth(file, email, account_id, exp, tag)
    }

    async fn mock_codex_usage(&self, bearer: &str, account_id: &str, calls: u64) {
        Mock::given(method("GET"))
            .and(path("/wham/usage"))
            .and(header("authorization", format!("Bearer {bearer}").as_str()))
            .and(header("chatgpt-account-id", account_id))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!("fixtures/codex_usage_team.json"))
                    .insert_header("content-type", "application/json"),
            )
            .expect(calls)
            .mount(&self.server)
            .await;
    }

    async fn mock_codex_token(&self, new_access: &str, calls: u64) {
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id_token": "new-id", "access_token": new_access, "refresh_token": "ref-new"
            })))
            .expect(calls)
            .mount(&self.server)
            .await;
    }
}

fn bytes(p: &PathBuf) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

// S1 --------------------------------------------------------------------------------------
#[tokio::test]
async fn s1_shared_login_reads_the_fresh_cli_copy_and_never_refreshes() {
    let w = World::new().await;
    let managed = w.claude_managed(
        "m1",
        "u-active",
        "active@example.com",
        &claude_doc("acc-managed-old", "ref-managed-old", now_ms() - 2 * HOUR),
    );
    let system = w.claude_system(
        "u-active",
        "active@example.com",
        &claude_doc("acc-home-fresh", "ref-home", now_ms() + 5 * HOUR),
    );
    let (before_m, before_s) = (bytes(&managed), bytes(&system));
    w.mock_claude_usage("acc-home-fresh", 1).await;
    w.mock_claude_token(0).await;

    let views = w.collect("claude:", true).await;

    assert_eq!(
        views.len(),
        1,
        "the CLI copy must not show up as a second account: {views:?}"
    );
    assert_eq!(views[0].state, StateKind::Ok, "{:?}", views[0].message);
    assert!(!views[0].token_refreshed);
    assert_eq!(bytes(&managed), before_m);
    assert_eq!(bytes(&system), before_s);
}

// S2 --------------------------------------------------------------------------------------
#[tokio::test]
async fn s2_shared_login_with_every_copy_expired_is_reported_not_refreshed() {
    let w = World::new().await;
    let managed = w.claude_managed(
        "m1",
        "u-active",
        "active@example.com",
        &claude_doc("acc-managed-old", "ref-managed-old", now_ms() - 2 * HOUR),
    );
    let system = w.claude_system(
        "u-active",
        "active@example.com",
        &claude_doc("acc-home-old", "ref-home-old", now_ms() - HOUR),
    );
    let (before_m, before_s) = (bytes(&managed), bytes(&system));
    w.mock_claude_usage("acc-home-old", 0).await;
    w.mock_claude_usage("acc-managed-old", 0).await;
    w.mock_claude_token(0).await;

    let views = w.collect("claude:", true).await;

    assert_eq!(views.len(), 1);
    assert_eq!(
        views[0].state,
        StateKind::TokenExpired,
        "{:?}",
        views[0].message
    );
    assert!(views[0].message.as_deref().is_some_and(|m| !m.is_empty()));
    assert_eq!(bytes(&managed), before_m);
    assert_eq!(bytes(&system), before_s);
}

// S3 --------------------------------------------------------------------------------------
#[tokio::test]
async fn s3_a_login_that_is_not_the_active_one_is_refreshed_and_written_back() {
    let w = World::new().await;
    let managed = w.claude_managed(
        "m2",
        "u-other",
        "other@example.com",
        &claude_doc("acc-old", "ref-old", now_ms() - 3 * HOUR),
    );
    // the CLI's active login is a different account, so m2 is not shared with it
    w.claude_system(
        "u-active",
        "active@example.com",
        &claude_doc("acc-home", "ref-home", now_ms() + 5 * HOUR),
    );
    w.mock_claude_token(1).await;
    w.mock_claude_usage("acc-new", 1).await;

    let views = w.collect("claude:m2", true).await;

    assert_eq!(views.len(), 1);
    assert_eq!(views[0].state, StateKind::Ok, "{:?}", views[0].message);
    assert!(views[0].token_refreshed);
    let doc: Value = serde_json::from_slice(&bytes(&managed)).unwrap();
    assert_eq!(doc["claudeAiOauth"]["accessToken"], "acc-new");
    assert_eq!(doc["claudeAiOauth"]["refreshToken"], "ref-new");
}

// S4 --------------------------------------------------------------------------------------
#[tokio::test]
async fn s4_codex_same_identity_in_two_homes_is_one_shared_account_and_never_refreshed() {
    let w = World::new().await;
    let now_s = now_ms() / 1000;
    let (managed, _) = w.codex_managed(
        "c1",
        "same@example.com",
        "acct-1",
        now_s - 3600,
        "managed-old",
    );
    let (system, fresh) = w.codex_system(
        "same@example.com",
        "acct-1",
        now_s + 5 * 86_400,
        "home-fresh",
    );
    let (before_m, before_s) = (bytes(&managed), bytes(&system));
    w.mock_codex_usage(&fresh, "acct-1", 1).await;
    w.mock_codex_token("unused", 0).await;

    let views = w.collect("codex:", true).await;

    assert_eq!(views.len(), 1, "{views:?}");
    assert_eq!(views[0].state, StateKind::Ok, "{:?}", views[0].message);
    assert_eq!(bytes(&managed), before_m);
    assert_eq!(bytes(&system), before_s);
}

// S5 --------------------------------------------------------------------------------------
#[tokio::test]
async fn s5_codex_different_identities_are_two_accounts_and_only_the_expired_one_is_refreshed() {
    let w = World::new().await;
    let now_s = now_ms() / 1000;
    let (managed, _) = w.codex_managed(
        "c1",
        "work@example.com",
        "acct-work",
        now_s - 3600,
        "managed-old",
    );
    let (system, fresh_default) = w.codex_system(
        "home@example.com",
        "acct-home",
        now_s + 5 * 86_400,
        "home-fresh",
    );
    let before_s = bytes(&system);
    let new_access = jwt(&json!({ "exp": now_s + 10 * 86_400, "tag": "refreshed" }));
    w.mock_codex_token(&new_access, 1).await;
    w.mock_codex_usage(&new_access, "acct-work", 1).await;
    w.mock_codex_usage(&fresh_default, "acct-home", 1).await;

    let views = w.collect("codex:", true).await;

    assert_eq!(views.len(), 2, "{views:?}");
    assert!(views.iter().all(|v| v.state == StateKind::Ok), "{views:?}");
    let doc: Value = serde_json::from_slice(&bytes(&managed)).unwrap();
    assert_eq!(doc["tokens"]["access_token"], new_access);
    assert_eq!(
        bytes(&system),
        before_s,
        "the untouched default login must stay byte-identical"
    );
}

// S6 --------------------------------------------------------------------------------------
#[tokio::test]
async fn s6_refresh_switched_off_reports_the_expired_token_and_never_calls_the_token_endpoint() {
    let w = World::new().await;
    let managed = w.claude_managed(
        "m2",
        "u-other",
        "other@example.com",
        &claude_doc("acc-old", "ref-old", now_ms() - 3 * HOUR),
    );
    let before = bytes(&managed);
    w.mock_claude_token(0).await;
    w.mock_claude_usage("acc-old", 0).await;

    let views = w.collect("claude:m2", false).await;

    assert_eq!(views.len(), 1);
    assert_eq!(
        views[0].state,
        StateKind::TokenExpired,
        "{:?}",
        views[0].message
    );
    assert_eq!(bytes(&managed), before);
}
