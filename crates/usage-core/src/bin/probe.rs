//! End-to-end probe: runs one real collection against this machine's accounts and prints
//! the result (never any token). Used as the repeatable live check.
//!
//!   cargo run -p usage-core --bin probe              human-readable table
//!   cargo run -p usage-core --bin probe -- --json    full JSON
//!   cargo run -p usage-core --bin probe -- --no-refresh   never refresh/write any token
//!   cargo run -p usage-core --bin probe -- --only work     only accounts whose key or label contains "work"
//!   cargo run -p usage-core --bin probe -- --accounts      no network: list accounts + sharing

use usage_core::collector::{Collector, CollectorConfig};
use usage_core::model::{AccountView, Provider, StateKind, WindowKind, WindowView};

fn provider_rank(p: Provider) -> u8 {
    match p {
        Provider::Claude => 0,
        Provider::Codex => 1,
        Provider::Gemini => 2,
    }
}

fn reset_in(ms: Option<i64>, now: i64) -> String {
    let Some(ms) = ms else { return "-".into() };
    let mins = (ms - now) / 60_000;
    if mins < 0 {
        "已重置".into()
    } else if mins < 90 {
        format!("{mins}分")
    } else if mins < 48 * 60 {
        format!("{:.1}時", mins as f64 / 60.0)
    } else {
        format!("{:.1}天", mins as f64 / 1440.0)
    }
}

fn describe(w: &WindowView, now: i64) -> String {
    let label = match (w.kind, &w.scope, &w.group) {
        (WindowKind::Session, _, Some(g)) => format!("{g} 5h"),
        (WindowKind::Weekly, _, Some(g)) => format!("{g} 週"),
        (WindowKind::Session, _, _) => "5h".into(),
        (WindowKind::Weekly, Some(s), _) => format!("{s} 週"),
        (WindowKind::Weekly, _, _) => "週".into(),
        (WindowKind::Other, _, _) => format!("{}分鐘", w.window_minutes.unwrap_or(0)),
    };
    format!(
        "{label}:{:.0}%(重置 {})",
        w.used_percent,
        reset_in(w.resets_at_ms, now)
    )
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let auto_refresh = !args.iter().any(|a| a == "--no-refresh");

    if args.iter().any(|a| a == "--accounts") {
        // No network at all: only show what was found and which logins are shared.
        let roots = usage_core::accounts::Roots::from_env();
        let rule = |n: usize| {
            if n == 0 {
                "can be refreshed".to_string()
            } else {
                format!("SHARED with {n} other file(s): read the freshest copy, never refreshed")
            }
        };
        for a in usage_core::accounts::discover_claude(&roots) {
            println!(
                "[Claude] {} ({}) -> {}",
                a.email,
                a.key,
                rule(a.shared_copies.len())
            );
        }
        for a in usage_core::accounts::discover_codex(&roots) {
            println!(
                "[Codex ] {} ({}) -> {}",
                a.email,
                a.key,
                rule(a.shared_copies.len())
            );
        }
        if let Some(a) = usage_core::accounts::discover_gemini(&roots) {
            println!(
                "[Gemini] {} ({}) -> asked through the agy CLI, no credentials read",
                a.email, a.key
            );
        }
        return;
    }

    let mut cfg = CollectorConfig::from_env(auto_refresh);
    cfg.only = args
        .iter()
        .position(|a| a == "--only")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let collector = Collector::new();
    let mut views: Vec<AccountView> = Vec::new();
    collector.collect(&cfg, |v| views.push(v)).await;
    views.sort_by(|a, b| {
        (provider_rank(a.provider), a.label.to_lowercase())
            .cmp(&(provider_rank(b.provider), b.label.to_lowercase()))
    });

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&views).expect("serialise")
        );
        return;
    }

    let now = usage_core::util::now_ms();
    for v in &views {
        let state = match v.state {
            StateKind::Ok => "OK",
            StateKind::Stale => "STALE",
            StateKind::Waiting => "WAITING",
            StateKind::TokenExpired => "TOKEN-EXPIRED",
            StateKind::LoginRequired => "LOGIN-REQUIRED",
            StateKind::Error => "ERROR",
            StateKind::Loading => "LOADING",
        };
        println!(
            "[{:?}] {} | {} | {}{}",
            v.provider,
            v.label,
            v.plan.as_deref().unwrap_or("-"),
            state,
            if v.token_refreshed {
                " (token refreshed)"
            } else {
                ""
            }
        );
        for w in &v.windows {
            println!("      {}", describe(w, now));
        }
        for e in &v.extras {
            println!("      + {} = {}", e.key, e.value);
        }
        if let Some(m) = &v.message {
            println!("      ! {m}");
        }
    }
    println!("\n{} account(s)", views.len());
}
