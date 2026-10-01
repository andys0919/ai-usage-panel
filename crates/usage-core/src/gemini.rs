//! Gemini through the Antigravity CLI: `agy --print "/quota" --output-format json`.
//!
//! Google retired the Gemini CLI client for individuals, and Antigravity keeps its login in
//! the Windows Credential Manager. Asking the official CLI is therefore both the only way
//! and the safest one: this program never touches Google credentials.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;

use crate::model::{WindowKind, WindowView};
use crate::util::{clamp_pct, parse_ts, short};

const RUN_TIMEOUT: Duration = Duration::from_secs(60);

fn find_json(stdout: &str) -> Option<Value> {
    let whole = stdout.trim();
    if let Ok(v) = serde_json::from_str::<Value>(whole) {
        return Some(v);
    }
    // Some builds print log lines before the JSON object.
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .find_map(|l| serde_json::from_str::<Value>(l).ok())
}

pub fn parse_quota(stdout: &str) -> Result<Vec<WindowView>, String> {
    let v = find_json(stdout).ok_or_else(|| "agy 的輸出不是 JSON".to_string())?;
    if v.get("status").and_then(Value::as_str) != Some("SUCCESS") {
        let why = v
            .get("response")
            .and_then(Value::as_str)
            .unwrap_or("沒有說明");
        return Err(format!("agy 回報失敗:{}", short(why.trim(), 120)));
    }
    let groups = v
        .pointer("/command/data/groups")
        .and_then(Value::as_array)
        .filter(|g| !g.is_empty())
        .ok_or_else(|| "agy 沒有回額度資料".to_string())?;

    let mut out = Vec::new();
    for g in groups {
        let name = g.get("name").and_then(Value::as_str).map(str::to_string);
        let Some(buckets) = g.get("buckets").and_then(Value::as_array) else {
            continue;
        };
        for b in buckets {
            let Some(remaining) = b.get("remaining_fraction").and_then(Value::as_f64) else {
                continue;
            };
            let (kind, minutes) = match b.get("window").and_then(Value::as_str) {
                Some("5h") => (WindowKind::Session, Some(300)),
                Some("weekly") => (WindowKind::Weekly, Some(10080)),
                _ => (WindowKind::Other, None),
            };
            out.push(WindowView {
                kind,
                group: name.clone(),
                scope: None,
                used_percent: clamp_pct((1.0 - remaining) * 100.0),
                resets_at_ms: b.get("reset_time").and_then(parse_ts),
                window_minutes: minutes,
            });
        }
    }
    if out.is_empty() {
        return Err("agy 沒有回額度資料".to_string());
    }
    Ok(out)
}

/// Run the CLI once. `cwd` should be a folder agy already trusts (the user's home) so the
/// call never prompts and never changes agy's trusted-workspace list.
pub async fn fetch_quota(agy: &Path, cwd: &Path) -> Result<Vec<WindowView>, String> {
    let mut cmd = tokio::process::Command::new(agy);
    cmd.args([
        "--print",
        "/quota",
        "--output-format",
        "json",
        "--print-timeout",
        "45s",
    ])
    .current_dir(cwd)
    .env("NO_COLOR", "1")
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flashing on screen

    let child = cmd
        .spawn()
        .map_err(|e| format!("啟動不了 agy({})", e.kind()))?;
    let out = tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "agy 執行逾時".to_string())?
        .map_err(|e| format!("agy 執行失敗({})", e.kind()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_quota(&stdout).map_err(|e| {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let hint = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        if hint.is_empty() {
            e
        } else {
            format!("{e}({})", short(hint.trim(), 80))
        }
    })
}
