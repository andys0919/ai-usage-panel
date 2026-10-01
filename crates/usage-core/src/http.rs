use std::time::Duration;

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("授權失效(401/403)")]
    Unauthorized,
    #[error("被限流")]
    RateLimited { retry_after_secs: Option<u64> },
    #[error("HTTP {0}")]
    Http(u16),
    #[error("{0}")]
    Network(String),
    #[error("{0}")]
    Parse(String),
}

const FETCH_TIMEOUT: Duration = Duration::from_secs(12);

/// Who we are. Do NOT impersonate `claude-code/*`: Claude's token endpoint answers HTTP 429 to
/// that User-Agent (it is the string every third-party tool copies), while an honest custom
/// name gets the normal answers. Measured 2026-10-01; see tests/oauth_refresh.rs (#19).
pub const USER_AGENT: &str = concat!("ai-usage-panel/", env!("CARGO_PKG_VERSION"));

/// GET a JSON document. Only status codes and short fixed texts are ever put into errors.
pub async fn get_json(
    http: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<Value, FetchError> {
    let mut req = http.get(url).timeout(FETCH_TIMEOUT);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.map_err(|e| {
        FetchError::Network(
            if e.is_timeout() {
                "逾時"
            } else if e.is_connect() {
                "連不上伺服器"
            } else {
                "網路錯誤"
            }
            .to_string(),
        )
    })?;
    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(FetchError::Unauthorized);
    }
    if status.as_u16() == 429 {
        let retry_after_secs = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        return Err(FetchError::RateLimited { retry_after_secs });
    }
    if !status.is_success() {
        return Err(FetchError::Http(status.as_u16()));
    }
    resp.json::<Value>()
        .await
        .map_err(|_| FetchError::Parse("回應不是有效的 JSON".to_string()))
}
