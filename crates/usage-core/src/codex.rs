//! Codex: `GET chatgpt.com/backend-api/wham/usage` (the same call Orca makes).

use serde_json::Value;

use crate::http::{get_json, FetchError};
use crate::model::{Extra, WindowKind, WindowView};
use crate::oauth::CODEX_USER_AGENT;
use crate::util::{clamp_pct, parse_ts, title_case};

pub const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

#[derive(Debug, Clone, PartialEq)]
pub struct CodexUsage {
    pub windows: Vec<WindowView>,
    pub plan: Option<String>,
    pub extras: Vec<Extra>,
}

fn classify(minutes: Option<u32>) -> WindowKind {
    match minutes {
        Some(m) if m.abs_diff(300) <= 1 => WindowKind::Session,
        Some(m) if m.abs_diff(10080) <= 1 => WindowKind::Weekly,
        _ => WindowKind::Other,
    }
}

/// A window is told apart by its LENGTH, never by whether it is "primary" or "secondary":
/// Pro accounts currently report only a weekly window as the primary one.
pub fn parse_usage(v: &Value) -> Option<CodexUsage> {
    let plan = v.get("plan_type")?.as_str()?;
    let mut windows = Vec::new();
    if let Some(rate_limit) = v.get("rate_limit") {
        for key in ["primary_window", "secondary_window"] {
            let Some(w) = rate_limit.get(key).filter(|w| w.is_object()) else {
                continue;
            };
            let Some(pct) = w.get("used_percent").and_then(Value::as_f64) else {
                continue;
            };
            let minutes = w
                .get("limit_window_seconds")
                .and_then(Value::as_f64)
                .filter(|s| *s > 0.0)
                .map(|s| (s / 60.0).round() as u32);
            windows.push(WindowView {
                kind: classify(minutes),
                group: None,
                scope: None,
                used_percent: clamp_pct(pct),
                resets_at_ms: w.get("reset_at").and_then(parse_ts),
                window_minutes: minutes,
            });
        }
    }

    let mut extras = Vec::new();
    let credits = v
        .pointer("/rate_limit_reset_credits/available_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if credits > 0 {
        extras.push(Extra {
            key: "reset_credits".into(),
            value: credits.to_string(),
        });
    }

    Some(CodexUsage {
        windows,
        plan: Some(title_case(plan)),
        extras,
    })
}

pub async fn fetch_usage(
    http: &reqwest::Client,
    url: &str,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<CodexUsage, FetchError> {
    let bearer = format!("Bearer {access_token}");
    let mut headers = vec![
        ("Authorization", bearer.as_str()),
        ("User-Agent", CODEX_USER_AGENT),
        ("OpenAI-Beta", "codex-1"),
        ("originator", "Codex Desktop"),
    ];
    if let Some(id) = account_id {
        headers.push(("ChatGPT-Account-Id", id));
    }
    let json = get_json(http, url, &headers).await?;
    parse_usage(&json)
        .ok_or_else(|| FetchError::Parse("回應不像用量資料(沒有 plan_type)".to_string()))
}
