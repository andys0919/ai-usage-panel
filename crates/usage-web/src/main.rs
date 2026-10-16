//! The panel as a web page: same UI as the desktop app, behind HTTP Basic auth, showing ONLY
//! the sandbox's accounts (never Orca's, the CLIs' own logins or agy).
//!
//!   PANEL_USER=... PANEL_PASS=... ai-usage-web [--port 8130]
//!
//! Listens on 127.0.0.1 only; put a tunnel or reverse proxy in front for remote access.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use serde::Deserialize;
use usage_core::collector::CollectorConfig;
use usage_core::panel::{Panel, Snapshot};

/// After this many wrong passwords in a row, every login is refused for `LOCKOUT`.
const FAIL_LIMIT: u32 = 10;
const LOCKOUT: Duration = Duration::from_secs(60);
const DEFAULT_PORT: u16 = 8130;

const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
                   font-src 'self'; connect-src 'self'; frame-ancestors 'none'";

macro_rules! ui {
    ($path:literal, $mime:literal) => {
        ($path, $mime, include_bytes!(concat!("../../../ui", $path)) as &[u8])
    };
}

const FILES: &[(&str, &str, &[u8])] = &[
    ui!("/index.html", "text/html; charset=utf-8"),
    ui!("/app.js", "text/javascript; charset=utf-8"),
    ui!("/web.js", "text/javascript; charset=utf-8"),
    ui!("/styles.css", "text/css; charset=utf-8"),
    ui!("/fonts/fira-code-latin-500-normal.woff2", "font/woff2"),
    ui!("/fonts/fira-code-latin-600-normal.woff2", "font/woff2"),
    ui!("/fonts/fira-sans-latin-400-normal.woff2", "font/woff2"),
    ui!("/fonts/fira-sans-latin-500-normal.woff2", "font/woff2"),
    ui!("/fonts/fira-sans-latin-600-normal.woff2", "font/woff2"),
];

#[derive(Default)]
struct Failures {
    count: u32,
    locked_until: Option<Instant>,
}

struct Auth {
    /// "user:password", as the browser sends it.
    expected: Vec<u8>,
    failures: Mutex<Failures>,
}

fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Auth {
    fn check(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        let mut f = self.failures.lock().unwrap();
        if let Some(until) = f.locked_until {
            if Instant::now() < until {
                return Err(StatusCode::TOO_MANY_REQUESTS);
            }
            *f = Failures::default();
        }
        let given = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Basic "))
            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b.trim()).ok());
        match given {
            Some(g) if same_bytes(&g, &self.expected) => {
                f.count = 0;
                Ok(())
            }
            Some(_) => {
                f.count += 1;
                if f.count >= FAIL_LIMIT {
                    f.locked_until = Some(Instant::now() + LOCKOUT);
                }
                Err(StatusCode::UNAUTHORIZED)
            }
            // No credentials yet: the browser's first request, not a failed attempt.
            None => Err(StatusCode::UNAUTHORIZED),
        }
    }
}

#[derive(Clone)]
struct App {
    panel: Arc<Panel>,
    auth: Arc<Auth>,
}

async fn require_auth(State(app): State<App>, req: Request, next: Next) -> Response {
    if let Err(status) = app.auth.check(req.headers()) {
        let mut res = (status, "需要登入").into_response();
        if status == StatusCode::UNAUTHORIZED {
            res.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"AI usage panel\", charset=\"UTF-8\"".parse().unwrap(),
            );
        }
        return res;
    }
    // A header other sites cannot send without a CORS preflight (which is never granted).
    if req.uri().path().starts_with("/api/")
        && req.headers().get("x-requested-with").map(|v| v.as_bytes()) != Some(&b"panel"[..])
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    h.insert(header::CONTENT_SECURITY_POLICY, CSP.parse().unwrap());
    h.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    h.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    res
}

async fn static_file(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = match uri.path() {
        "/" => "/index.html",
        p => p,
    };
    match FILES.iter().find(|(p, _, _)| *p == path) {
        Some((_, mime, body)) => ([(header::CONTENT_TYPE, *mime)], *body).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn get_state(State(app): State<App>) -> Json<Snapshot> {
    Json(app.panel.snapshot())
}

async fn refresh_now(State(app): State<App>) -> Result<Json<Snapshot>, (StatusCode, String)> {
    app.panel
        .refresh_now()
        .map(Json)
        .map_err(|e| (StatusCode::CONFLICT, e))
}

/// Same argument names as the desktop app's command (Tauri sends them in camelCase).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsBody {
    interval_secs: u64,
    auto_refresh_tokens: bool,
}

async fn set_settings(
    State(app): State<App>,
    Json(body): Json<SettingsBody>,
) -> Result<Json<Snapshot>, (StatusCode, String)> {
    app.panel
        .set_settings(body.interval_secs, body.auto_refresh_tokens)
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
}

#[tokio::main]
async fn main() {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let (Some(user), Some(pass)) = (env("PANEL_USER"), env("PANEL_PASS")) else {
        eprintln!("set PANEL_USER and PANEL_PASS");
        std::process::exit(2);
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let port = match args.iter().position(|a| a == "--port") {
        Some(i) => match args.get(i + 1).and_then(|p| p.parse::<u16>().ok()) {
            Some(p) => p,
            None => {
                eprintln!("--port needs a number");
                std::process::exit(2);
            }
        },
        None => DEFAULT_PORT,
    };

    let mut cfg = CollectorConfig::from_env(true);
    cfg.roots.sandbox_only = true;
    // Own settings and cache, so a desktop panel on the same machine is not affected.
    let data_dir = cfg.roots.appdata.join("ai-usage-panel").join("web");
    let panel = Panel::new(cfg, data_dir);
    tokio::spawn(panel.clone().run_forever());

    let app = App {
        panel,
        auth: Arc::new(Auth {
            expected: format!("{user}:{pass}").into_bytes(),
            failures: Mutex::default(),
        }),
    };
    let router = Router::new()
        .route("/api/get_state", post(get_state))
        .route("/api/refresh_now", post(refresh_now))
        .route("/api/set_settings", post(set_settings))
        .fallback(static_file)
        .layer(middleware::from_fn_with_state(app.clone(), require_auth))
        .with_state(app);

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on 127.0.0.1:{port} ({})", e.kind());
            std::process::exit(1);
        }
    };
    eprintln!("AI usage panel (sandbox accounts only) on http://127.0.0.1:{port}");
    if let Err(e) = axum::serve(listener, router).await {
        eprintln!("server stopped ({})", e.kind());
        std::process::exit(1);
    }
}
