//! Claude: `GET /api/oauth/usage` (the same call Claude Code's `/usage` and Orca make).

use serde_json::Value;

use crate::http::{get_json, FetchError};
use crate::model::{WindowKind, WindowView};
use crate::util::{clamp_pct, parse_ts};

pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

const SESSION_MINUTES: u32 = 300;
const WEEK_MINUTES: u32 = 10080;

fn scope_name(limit: &Value) -> Option<String> {
    limit
        .pointer("/scope/model/display_name")
        .or_else(|| limit.pointer("/scope/surface"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn window(
    kind: WindowKind,
    scope: Option<String>,
    pct: f64,
    resets_at: Option<&Value>,
    minutes: u32,
) -> WindowView {
    WindowView {
        kind,
        group: None,
        scope,
        used_percent: clamp_pct(pct),
        resets_at_ms: resets_at.and_then(parse_ts),
        window_minutes: Some(minutes),
    }
}

/// Newer payloads carry a `limits` array; older ones only `five_hour` / `seven_day` / ….
pub fn parse_usage(v: &Value) -> Vec<WindowView> {
    let mut out = Vec::new();

    if let Some(limits) = v.get("limits").and_then(Value::as_array) {
        for l in limits {
            let Some(pct) = l.get("percent").and_then(Value::as_f64) else {
                continue;
            };
            let (kind, minutes, scope) = match l.get("kind").and_then(Value::as_str) {
                Some("session") => (WindowKind::Session, SESSION_MINUTES, None),
                Some("weekly_all") => (WindowKind::Weekly, WEEK_MINUTES, None),
                Some("weekly_scoped") => (WindowKind::Weekly, WEEK_MINUTES, scope_name(l)),
                _ => continue,
            };
            out.push(window(kind, scope, pct, l.get("resets_at"), minutes));
        }
        if !out.is_empty() {
            return out;
        }
    }

    let legacy: [(&str, WindowKind, u32, Option<&str>); 4] = [
        ("five_hour", WindowKind::Session, SESSION_MINUTES, None),
        ("seven_day", WindowKind::Weekly, WEEK_MINUTES, None),
        (
            "seven_day_opus",
            WindowKind::Weekly,
            WEEK_MINUTES,
            Some("Opus"),
        ),
        (
            "seven_day_sonnet",
            WindowKind::Weekly,
            WEEK_MINUTES,
            Some("Sonnet"),
        ),
    ];
    for (key, kind, minutes, scope) in legacy {
        let Some(w) = v.get(key).filter(|w| w.is_object()) else {
            continue;
        };
        let Some(pct) = w.get("utilization").and_then(Value::as_f64) else {
            continue;
        };
        out.push(window(
            kind,
            scope.map(String::from),
            pct,
            w.get("resets_at"),
            minutes,
        ));
    }
    out
}

/// "max" + "default_claude_max_20x" -> "Max 20x"; "team" + "…max_5x" -> "Team · Max 5x".
pub fn plan_label(
    subscription_type: Option<&str>,
    rate_limit_tier: Option<&str>,
) -> Option<String> {
    let multiplier = rate_limit_tier.and_then(|t| {
        let idx = t.find("max_")?;
        let rest = &t[idx + 4..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(rest.len());
        let m = &rest[..end];
        (!m.is_empty()).then(|| m.to_string())
    });
    let sub = subscription_type
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(crate::util::title_case);
    match (sub, multiplier) {
        (Some(s), Some(m)) if s.eq_ignore_ascii_case("max") => Some(format!("Max {m}")),
        (Some(s), Some(m)) => Some(format!("{s} · Max {m}")),
        (Some(s), None) => Some(s),
        (None, Some(m)) => Some(format!("Max {m}")),
        (None, None) => None,
    }
}

pub async fn fetch_usage(
    http: &reqwest::Client,
    url: &str,
    access_token: &str,
) -> Result<Vec<WindowView>, FetchError> {
    let bearer = format!("Bearer {access_token}");
    let json = get_json(
        http,
        url,
        &[
            ("Authorization", bearer.as_str()),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("User-Agent", crate::http::USER_AGENT),
        ],
    )
    .await?;
    let windows = parse_usage(&json);
    if windows.is_empty() {
        return Err(FetchError::Parse("回應裡沒有用量資料".to_string()));
    }
    Ok(windows)
}
