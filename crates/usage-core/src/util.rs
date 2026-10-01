use serde_json::Value;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Epoch seconds, epoch milliseconds, a numeric string or an RFC 3339 string -> epoch millis.
pub fn parse_ts(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_f64().map(secs_or_millis),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            if let Ok(f) = s.parse::<f64>() {
                return Some(secs_or_millis(f));
            }
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|d| d.timestamp_millis())
        }
        _ => None,
    }
}

fn secs_or_millis(f: f64) -> i64 {
    // 1e11 seconds is the year 5138, 1e11 milliseconds is 1973: safe boundary for today's data.
    if f > 1e11 {
        f as i64
    } else {
        (f * 1000.0) as i64
    }
}

pub fn clamp_pct(x: f64) -> f64 {
    if x.is_nan() {
        0.0
    } else {
        x.clamp(0.0, 100.0)
    }
}

pub fn short(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

pub fn title_case(s: &str) -> String {
    let mut chars = s.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
