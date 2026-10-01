//! Data handed to the UI. Everything here is plain, serialisable and free of secrets.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    Codex,
    Gemini,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    /// The rolling 5-hour limit.
    Session,
    Weekly,
    /// Some other window length (see `window_minutes`).
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowView {
    pub kind: WindowKind,
    /// Section heading when a card has several groups (Gemini: "Gemini Models").
    pub group: Option<String>,
    /// Qualifier such as a model name ("Fable").
    pub scope: Option<String>,
    /// 0..=100, how much of the window is already used.
    pub used_percent: f64,
    pub resets_at_ms: Option<i64>,
    pub window_minutes: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Extra {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateKind {
    /// Discovered, first fetch not finished yet.
    Loading,
    Ok,
    /// Showing the last good numbers; the latest attempt failed.
    Stale,
    /// No numbers yet, but the program retries by itself (rate limit, network trouble).
    Waiting,
    /// Access token expired and automatic refresh is switched off.
    TokenExpired,
    /// Needs an interactive login.
    LoginRequired,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountView {
    /// Stable id, e.g. "claude:<account-folder-id>".
    pub key: String,
    pub provider: Provider,
    pub label: String,
    /// Organisation / workspace line.
    pub detail: Option<String>,
    pub plan: Option<String>,
    pub windows: Vec<WindowView>,
    pub extras: Vec<Extra>,
    pub state: StateKind,
    /// Why the state is not Ok, or a warning the user should read.
    pub message: Option<String>,
    /// A terminal command that fixes the problem (shown with a copy button).
    pub fix_command: Option<String>,
    /// When the shown numbers were obtained (epoch ms).
    pub fetched_at_ms: Option<i64>,
    /// True when this cycle refreshed the account's OAuth token.
    pub token_refreshed: bool,
}

impl AccountView {
    pub fn new(key: impl Into<String>, provider: Provider, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            provider,
            label: label.into(),
            detail: None,
            plan: None,
            windows: Vec::new(),
            extras: Vec::new(),
            state: StateKind::Error,
            message: None,
            fix_command: None,
            fetched_at_ms: None,
            token_refreshed: false,
        }
    }
}
