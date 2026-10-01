//! Boundary under test: turning each provider's raw answer into `WindowView`s.
//!
//! The fixtures are REAL responses captured from the three providers (ids and e-mail
//! addresses replaced). Failure modes covered:
//!
//! Claude  C1 real payload              C2 legacy payload (no `limits`)
//!         C3 out-of-range / unknown / incomplete entries   C4 garbage input
//!         C5 `limits` present but unusable -> fall back to the legacy fields
//! Codex   X1 team (5h + weekly)        X2 pro (weekly only, no 5h window)
//!         X3 unusual window length is NOT mislabelled as 5h/weekly
//!         X4 no `plan_type` -> not a usage payload        X5 zero reset credits not shown
//!         X6 window without `used_percent` skipped
//! Gemini  G1 real agy payload          G2 status != SUCCESS     G3 not JSON
//!         G4 log noise before the JSON G5 no groups            G6 unknown window label / clamping

use serde_json::{json, Value};
use usage_core::model::{Extra, WindowKind::*, WindowView};
use usage_core::{claude, codex, gemini};

fn json_of(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

fn win(
    kind: usage_core::model::WindowKind,
    group: Option<&str>,
    scope: Option<&str>,
    used: f64,
    resets: i64,
    minutes: u32,
) -> WindowView {
    WindowView {
        kind,
        group: group.map(String::from),
        scope: scope.map(String::from),
        used_percent: used,
        resets_at_ms: Some(resets),
        window_minutes: Some(minutes),
    }
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

// ---- Claude ----------------------------------------------------------------------------

#[test]
fn c1_real_payload_maps_session_weekly_and_scoped_weekly() {
    let got = claude::parse_usage(&json_of(include_str!("fixtures/claude_usage_real.json")));
    assert_eq!(
        got,
        vec![
            win(Session, None, None, 7.0, 1_790_828_999_641, 300),
            win(Weekly, None, None, 21.0, 1_791_298_799_641, 10080),
            win(Weekly, None, Some("Fable"), 0.0, 1_791_298_800_000, 10080),
        ]
    );
}

#[test]
fn c2_legacy_payload_without_limits_still_works() {
    let got = claude::parse_usage(&json_of(include_str!("fixtures/claude_usage_legacy.json")));
    assert_eq!(got.len(), 3, "sonnet is null and must be skipped: {got:?}");
    assert_eq!(
        (got[0].kind, got[0].used_percent, got[0].scope.as_deref()),
        (Session, 35.5, None)
    );
    assert_eq!(
        (got[1].kind, got[1].used_percent, got[1].scope.as_deref()),
        (Weekly, 60.0, None)
    );
    assert_eq!(
        (got[2].kind, got[2].used_percent, got[2].scope.as_deref()),
        (Weekly, 12.0, Some("Opus"))
    );
}

#[test]
fn c3_out_of_range_unknown_and_incomplete_entries() {
    let v = json!({ "limits": [
        { "kind": "session", "percent": 140, "resets_at": "2026-10-01T04:00:00+00:00" },
        { "kind": "weekly_all", "percent": -5, "resets_at": "2026-10-06T00:00:00+00:00" },
        { "kind": "weekly_all" },
        { "kind": "brand_new_kind_from_the_future", "percent": 50 },
        { "kind": "weekly_scoped", "percent": 10, "scope": { "model": null, "surface": "cowork" } }
    ]});
    let got = claude::parse_usage(&v);
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got[0].used_percent, 100.0);
    assert_eq!(got[1].used_percent, 0.0);
    assert_eq!(got[2].scope.as_deref(), Some("cowork"));
    assert_eq!(got[2].resets_at_ms, None);
}

#[test]
fn c4_garbage_input_gives_no_windows_and_no_panic() {
    for v in [
        json!(null),
        json!({}),
        json!([]),
        json!("x"),
        json!({ "limits": "nope" }),
    ] {
        assert!(claude::parse_usage(&v).is_empty(), "{v}");
    }
}

#[test]
fn c5_unusable_limits_fall_back_to_legacy_fields() {
    let v = json!({
        "limits": [ { "kind": "future_thing", "percent": 1 } ],
        "five_hour": { "utilization": 20, "resets_at": "2026-10-01T04:00:00+00:00" }
    });
    let got = claude::parse_usage(&v);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].kind, got[0].used_percent), (Session, 20.0));
}

// ---- Codex -----------------------------------------------------------------------------

#[test]
fn x1_team_payload_has_session_and_weekly() {
    let u = codex::parse_usage(&json_of(include_str!("fixtures/codex_usage_team.json"))).unwrap();
    assert_eq!(u.plan.as_deref(), Some("Team"));
    assert_eq!(
        u.windows,
        vec![
            win(Session, None, None, 2.0, 1_790_837_210_000, 300),
            win(Weekly, None, None, 11.0, 1_791_023_855_000, 10080),
        ]
    );
    assert_eq!(
        u.extras,
        vec![Extra {
            key: "reset_credits".into(),
            value: "3".into()
        }]
    );
}

#[test]
fn x2_pro_payload_has_only_a_weekly_window() {
    let u = codex::parse_usage(&json_of(include_str!("fixtures/codex_usage_pro.json"))).unwrap();
    assert_eq!(u.plan.as_deref(), Some("Pro"));
    assert_eq!(
        u.windows,
        vec![win(Weekly, None, None, 60.0, 1_791_263_032_000, 10080)]
    );
    assert_eq!(
        u.extras,
        vec![Extra {
            key: "reset_credits".into(),
            value: "2".into()
        }]
    );
}

#[test]
fn x3_unusual_window_length_is_not_mislabelled() {
    let mut v = json_of(include_str!("fixtures/codex_usage_team.json"));
    v["rate_limit"]["primary_window"]["limit_window_seconds"] = json!(3600);
    let u = codex::parse_usage(&v).unwrap();
    assert_eq!(u.windows[0].kind, Other);
    assert_eq!(u.windows[0].window_minutes, Some(60));
    assert_eq!(u.windows[1].kind, Weekly);
}

#[test]
fn x4_payload_without_plan_type_is_rejected() {
    assert!(codex::parse_usage(&json!({ "detail": "unauthorized" })).is_none());
    assert!(codex::parse_usage(&json!(null)).is_none());
}

#[test]
fn x5_zero_reset_credits_are_not_listed() {
    let mut v = json_of(include_str!("fixtures/codex_usage_team.json"));
    v["rate_limit_reset_credits"]["available_count"] = json!(0);
    assert!(codex::parse_usage(&v).unwrap().extras.is_empty());
}

#[test]
fn x6_window_without_used_percent_is_skipped() {
    let mut v = json_of(include_str!("fixtures/codex_usage_team.json"));
    v["rate_limit"]["primary_window"]
        .as_object_mut()
        .unwrap()
        .remove("used_percent");
    let u = codex::parse_usage(&v).unwrap();
    assert_eq!(u.windows.len(), 1);
    assert_eq!(u.windows[0].kind, Weekly);
}

// ---- Gemini (Antigravity `agy /quota`) -------------------------------------------------

#[test]
fn g1_real_payload_maps_two_groups_with_5h_and_weekly() {
    let got = gemini::parse_quota(include_str!("fixtures/agy_quota_real.json")).unwrap();
    assert_eq!(got.len(), 4);

    // order follows the payload: weekly first, then 5h, per group
    assert_eq!(
        (got[0].kind, got[0].group.as_deref()),
        (Weekly, Some("Gemini Models"))
    );
    assert!(near(
        got[0].used_percent,
        (1.0 - 0.3911486566066742) * 100.0
    ));
    assert_eq!(got[0].resets_at_ms, Some(1_790_822_697_000));
    assert_eq!(got[0].window_minutes, Some(10080));

    assert_eq!(
        (got[1].kind, got[1].group.as_deref()),
        (Session, Some("Gemini Models"))
    );
    assert!(near(got[1].used_percent, (1.0 - 0.981113076210022) * 100.0));
    assert_eq!(got[1].resets_at_ms, Some(1_790_832_159_000));
    assert_eq!(got[1].window_minutes, Some(300));

    assert_eq!(
        (got[2].kind, got[2].group.as_deref()),
        (Weekly, Some("Claude and GPT models"))
    );
    assert!(near(
        got[2].used_percent,
        (1.0 - 0.6588366627693176) * 100.0
    ));
    assert_eq!(got[2].resets_at_ms, Some(1_791_259_275_000));

    assert_eq!(
        (got[3].kind, got[3].group.as_deref()),
        (Session, Some("Claude and GPT models"))
    );
    assert!(near(got[3].used_percent, 0.0));
    assert_eq!(got[3].resets_at_ms, Some(1_790_837_725_000));
}

#[test]
fn g2_failed_status_is_an_error() {
    let out = r#"{"status":"ERROR","response":"Please sign in first"}"#;
    let err = gemini::parse_quota(out).unwrap_err();
    assert!(!err.is_empty());
}

#[test]
fn g3_non_json_output_is_an_error() {
    assert!(gemini::parse_quota("").is_err());
    assert!(gemini::parse_quota("agy: command not found").is_err());
}

#[test]
fn g4_log_noise_before_the_json_is_tolerated() {
    let noisy = format!(
        "ERROR: logging before google.Init: W0903 something harmless\n{}\n",
        include_str!("fixtures/agy_quota_real.json").trim()
    );
    assert_eq!(gemini::parse_quota(&noisy).unwrap().len(), 4);
}

#[test]
fn g5_success_without_groups_is_an_error() {
    let out =
        r#"{"status":"SUCCESS","response":"","command":{"name":"usage","data":{"groups":[]}}}"#;
    assert!(gemini::parse_quota(out).is_err());
    let out = r#"{"status":"SUCCESS","response":""}"#;
    assert!(gemini::parse_quota(out).is_err());
}

#[test]
fn g6_unknown_window_label_and_out_of_range_fraction() {
    let out = json!({
        "status": "SUCCESS",
        "command": { "data": { "groups": [ { "name": "G", "buckets": [
            { "window": "daily", "remaining_fraction": 1.7, "reset_time": "2026-10-01T00:00:00Z" },
            { "window": "5h", "remaining_fraction": -0.2 }
        ]}]}}
    })
    .to_string();
    let got = gemini::parse_quota(&out).unwrap();
    assert_eq!(got[0].kind, Other);
    assert_eq!(got[0].used_percent, 0.0);
    assert_eq!(got[1].kind, Session);
    assert_eq!(got[1].used_percent, 100.0);
    assert_eq!(got[1].resets_at_ms, None);
}
