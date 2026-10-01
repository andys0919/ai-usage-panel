//! Finds every account on this machine: the ones Orca manages plus the CLIs' own defaults.
//! Only reads folder/file structure and account labels; tokens stay inside the credential files.
//!
//! The SAME login can live in several credential files (Orca's managed folder, the CLI's own
//! home, Orca's runtime home). Those are reported as ONE account whose other files are listed
//! in `shared_copies`; the collector then reads the freshest copy and never refreshes it,
//! because rotating the token in one copy would kill the refresh token in all the others.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::oauth::jwt_claims;

#[derive(Debug, Clone)]
pub struct Roots {
    pub home: PathBuf,
    /// %APPDATA% (Roaming)
    pub appdata: PathBuf,
    /// %LOCALAPPDATA%
    pub localappdata: PathBuf,
}

impl Roots {
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
        let home = var("USERPROFILE")
            .or_else(|| var("HOME"))
            .unwrap_or_else(|| PathBuf::from("."));
        let appdata = var("APPDATA").unwrap_or_else(|| home.join("AppData").join("Roaming"));
        let localappdata =
            var("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData").join("Local"));
        Self {
            home,
            appdata,
            localappdata,
        }
    }

    pub fn orca_dir(&self) -> PathBuf {
        self.appdata.join("orca")
    }
}

pub fn read_json(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    let text = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    serde_json::from_slice(text).ok()
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn sub_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn dir_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ---- Claude -------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ClaudeAccount {
    pub key: String,
    pub email: String,
    pub org: Option<String>,
    pub creds_path: PathBuf,
    /// Other files holding the same login (the CLI's `~/.claude` copy while this account is
    /// the active one). Non-empty means: never refresh, read the freshest copy.
    pub shared_copies: Vec<PathBuf>,
}

pub fn discover_claude(roots: &Roots) -> Vec<ClaudeAccount> {
    let sys_creds = roots.home.join(".claude").join(".credentials.json");
    let active = read_json(&roots.home.join(".claude.json"))
        .and_then(|v| v.get("oauthAccount").cloned())
        .unwrap_or(Value::Null);
    let active_uuid = str_of(&active, "accountUuid");

    let mut out = Vec::new();
    let mut system_is_copy = false;

    for dir in sub_dirs(&roots.orca_dir().join("claude-accounts")) {
        let auth = dir.join("auth");
        let creds_path = auth.join(".credentials.json");
        if !creds_path.is_file() {
            continue;
        }
        let meta = read_json(&auth.join("oauth-account.json")).unwrap_or(Value::Null);
        let id = dir_name(&dir);
        let is_active_login = active_uuid.is_some() && str_of(&meta, "accountUuid") == active_uuid;
        let mut shared_copies = Vec::new();
        if is_active_login && sys_creds.is_file() {
            shared_copies.push(sys_creds.clone());
            system_is_copy = true;
        }
        out.push(ClaudeAccount {
            key: format!("claude:{id}"),
            email: str_of(&meta, "emailAddress")
                .unwrap_or_else(|| format!("Claude {}", id.chars().take(8).collect::<String>())),
            org: str_of(&meta, "organizationName"),
            creds_path,
            shared_copies,
        });
    }

    // The CLI's own login, when it is not just a copy of a managed account.
    if sys_creds.is_file() && !system_is_copy {
        out.push(ClaudeAccount {
            key: "claude:system-default".into(),
            email: str_of(&active, "emailAddress").unwrap_or_else(|| "Claude(系統預設)".into()),
            org: str_of(&active, "organizationName"),
            creds_path: sys_creds,
            shared_copies: Vec::new(),
        });
    }

    out.sort_by_key(|a| a.email.to_lowercase());
    out
}

// ---- Codex --------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CodexAccount {
    pub key: String,
    pub email: String,
    pub detail: Option<String>,
    pub plan_hint: Option<String>,
    pub auth_path: PathBuf,
    /// Other auth.json files holding the same login (the CLI's `~/.codex`, Orca's runtime
    /// home, a managed home). Non-empty means: never refresh, read the freshest copy.
    pub shared_copies: Vec<PathBuf>,
}

struct CodexIdentity {
    email: Option<String>,
    account_id: Option<String>,
    plan: Option<String>,
}

fn codex_identity(doc: &Value) -> CodexIdentity {
    let tokens = doc.get("tokens").cloned().unwrap_or(Value::Null);
    let id_claims = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(jwt_claims);
    let access_claims = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .and_then(jwt_claims);
    let email = id_claims
        .as_ref()
        .and_then(|c| str_of(c, "email"))
        .or_else(|| {
            access_claims
                .as_ref()
                .and_then(|c| c.pointer("/https:~1~1api.openai.com~1profile/email"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let plan = id_claims
        .as_ref()
        .or(access_claims.as_ref())
        .and_then(|c| c.pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type"))
        .and_then(Value::as_str)
        .map(crate::util::title_case);
    CodexIdentity {
        email,
        account_id: str_of(&tokens, "account_id"),
        plan,
    }
}

/// Same person AND same workspace. (Team members share an account id, so the e-mail matters.)
fn same_login(a: &CodexIdentity, b: &CodexIdentity) -> bool {
    match (&a.email, &b.email) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y) && a.account_id == b.account_id,
        _ => false,
    }
}

struct CodexCandidate {
    key: String,
    path: PathBuf,
    identity: CodexIdentity,
}

pub fn discover_codex(roots: &Roots) -> Vec<CodexAccount> {
    let orca = roots.orca_dir();
    let mut candidates: Vec<CodexCandidate> = Vec::new();

    // Order matters: the first file of a login becomes the account's primary file.
    for dir in sub_dirs(&orca.join("codex-accounts")) {
        let path = dir.join("home").join("auth.json");
        if let Some(doc) = read_json(&path) {
            candidates.push(CodexCandidate {
                key: format!("codex:{}", dir_name(&dir)),
                path,
                identity: codex_identity(&doc),
            });
        }
    }
    let extra = [
        (
            "codex:system-default",
            roots.home.join(".codex").join("auth.json"),
        ),
        (
            "codex:orca-runtime",
            orca.join("codex-runtime-home")
                .join("home")
                .join("auth.json"),
        ),
    ];
    for (key, path) in extra {
        if let Some(doc) = read_json(&path) {
            candidates.push(CodexCandidate {
                key: key.into(),
                path,
                identity: codex_identity(&doc),
            });
        }
    }

    let mut groups: Vec<Vec<CodexCandidate>> = Vec::new();
    for c in candidates {
        match groups
            .iter_mut()
            .find(|g| same_login(&g[0].identity, &c.identity))
        {
            Some(group) => group.push(c),
            None => groups.push(vec![c]),
        }
    }

    let mut accounts: Vec<CodexAccount> = groups
        .into_iter()
        .map(|mut g| {
            let primary = g.remove(0);
            CodexAccount {
                email: primary.identity.email.clone().unwrap_or_else(|| {
                    format!(
                        "Codex {}",
                        primary
                            .key
                            .trim_start_matches("codex:")
                            .chars()
                            .take(8)
                            .collect::<String>()
                    )
                }),
                key: primary.key,
                detail: None,
                plan_hint: primary.identity.plan,
                auth_path: primary.path,
                shared_copies: g.into_iter().map(|c| c.path).collect(),
            }
        })
        .collect();
    accounts.sort_by_key(|a| a.email.to_lowercase());
    accounts
}

// ---- Gemini (Antigravity) -----------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct GeminiAccount {
    pub key: String,
    pub email: String,
    pub agy_path: PathBuf,
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) { &["exe"] } else { &[""] };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let file = if ext.is_empty() {
                name.to_string()
            } else {
                format!("{name}.{ext}")
            };
            let candidate = dir.join(file);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn discover_gemini(roots: &Roots) -> Option<GeminiAccount> {
    let agy_path = find_on_path("agy").or_else(|| {
        let p = roots.localappdata.join("agy").join("bin").join("agy.exe");
        p.is_file().then_some(p)
    })?;
    let email = read_json(&roots.home.join(".gemini").join("google_accounts.json"))
        .and_then(|v| str_of(&v, "active"))
        .unwrap_or_else(|| "Google 帳號".into());
    Some(GeminiAccount {
        key: "gemini:antigravity".into(),
        email,
        agy_path,
    })
}
