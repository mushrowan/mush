use mush_ai::types::{CacheRetention, Provider, TokenCount, Usage};
use mush_tui::cache::{
    BustReason, CACHE_COLD_DISPLAY_SECS, CACHE_WARN_SECS, CacheAnomaly, CacheBustDiagnostic,
    CacheTimer, CallConfig, TokenStats, cache_ttl_secs, detect_cache_anomalies,
    dump_cache_bust_diagnostic_in,
};

fn test_config(model: &str, thinking: &str, effort: Option<&str>) -> CallConfig {
    CallConfig {
        model_id: model.into(),
        thinking_level: thinking.into(),
        effort: effort.map(Into::into),
    }
}

fn prev_usage() -> Usage {
    Usage {
        input_tokens: TokenCount::new(5_000),
        output_tokens: TokenCount::new(3_000),
        cache_read_tokens: TokenCount::new(95_000),
        cache_write_tokens: TokenCount::ZERO,
    }
}

fn curr_usage() -> Usage {
    Usage {
        input_tokens: TokenCount::new(5_000),
        output_tokens: TokenCount::new(3_000),
        cache_read_tokens: TokenCount::ZERO,
        cache_write_tokens: TokenCount::new(100_000),
    }
}

fn bust_reason(anomalies: &[CacheAnomaly]) -> Option<BustReason> {
    anomalies.iter().find_map(|a| match a {
        CacheAnomaly::CacheBust { reason, .. } => Some(reason.clone()),
        _ => None,
    })
}

#[test]
fn cache_module_exposes_timer_and_stats() {
    assert_eq!(CACHE_WARN_SECS, 60);
    assert_eq!(CACHE_COLD_DISPLAY_SECS, 30);
    assert_eq!(
        cache_ttl_secs(&Provider::Anthropic, Some(&CacheRetention::Short), false),
        300
    );

    let mut timer = CacheTimer::new(300);
    timer.refresh();
    assert!(timer.remaining_secs().is_some());

    let prev = Usage {
        input_tokens: TokenCount::new(5_000),
        output_tokens: TokenCount::new(3_000),
        cache_read_tokens: TokenCount::new(95_000),
        cache_write_tokens: TokenCount::ZERO,
    };
    let curr = Usage {
        input_tokens: TokenCount::new(5_000),
        output_tokens: TokenCount::new(3_000),
        cache_read_tokens: TokenCount::ZERO,
        cache_write_tokens: TokenCount::new(100_000),
    };

    let anomalies = detect_cache_anomalies(Some(&prev), &curr, None, None);
    assert!(
        anomalies
            .iter()
            .any(|a| matches!(a, CacheAnomaly::CacheBust { .. }))
    );

    let mut stats = TokenStats::new(TokenCount::new(200_000));
    let first = stats.update(&prev, None);
    assert!(first.is_empty());
}

#[test]
fn bust_reason_model_change() {
    let mut stats = TokenStats::new(TokenCount::new(200_000));
    stats.update_with_config(
        &prev_usage(),
        None,
        Some(test_config("opus-4-6", "High", Some("high"))),
    );
    let anomalies = stats.update_with_config(
        &curr_usage(),
        None,
        Some(test_config("opus-4-7", "High", Some("high"))),
    );
    assert_eq!(bust_reason(&anomalies), Some(BustReason::ModelChanged));
}

#[test]
fn bust_reason_thinking_change() {
    let mut stats = TokenStats::new(TokenCount::new(200_000));
    stats.update_with_config(
        &prev_usage(),
        None,
        Some(test_config("opus-4-7", "Xhigh", Some("xhigh"))),
    );
    let anomalies = stats.update_with_config(
        &curr_usage(),
        None,
        Some(test_config("opus-4-7", "High", Some("xhigh"))),
    );
    assert_eq!(bust_reason(&anomalies), Some(BustReason::ThinkingChanged));
}

#[test]
fn bust_reason_effort_change() {
    let mut stats = TokenStats::new(TokenCount::new(200_000));
    stats.update_with_config(
        &prev_usage(),
        None,
        Some(test_config("opus-4-7", "High", Some("high"))),
    );
    let anomalies = stats.update_with_config(
        &curr_usage(),
        None,
        Some(test_config("opus-4-7", "High", Some("xhigh"))),
    );
    assert_eq!(bust_reason(&anomalies), Some(BustReason::EffortChanged));
}

#[test]
fn bust_reason_unexplained_when_config_stable() {
    let cfg = test_config("opus-4-7", "High", Some("high"));
    let mut stats = TokenStats::new(TokenCount::new(200_000));
    stats.update_with_config(&prev_usage(), None, Some(cfg.clone()));
    let anomalies = stats.update_with_config(&curr_usage(), None, Some(cfg));
    assert_eq!(bust_reason(&anomalies), Some(BustReason::Unexplained));
}

#[test]
fn plain_update_defaults_to_unexplained() {
    // legacy callers without config context still detect busts
    let mut stats = TokenStats::new(TokenCount::new(200_000));
    stats.update(&prev_usage(), None);
    let anomalies = stats.update(&curr_usage(), None);
    assert_eq!(bust_reason(&anomalies), Some(BustReason::Unexplained));
}

#[test]
fn dump_pins_referenced_request_snapshots() {
    // when a bust dump references live request snapshots, those snapshots
    // must be copied into a sibling dir so the forensic bundle survives
    // the live ring's rotation. the written json should reference the
    // pinned copies, not the live paths that may have been deleted by
    // the time someone reads the bust file
    let tmp = tempfile::tempdir().unwrap();
    let bust_dir = tmp.path().join("cache-busts");
    let snap_dir = tmp.path().join("snapshots");
    std::fs::create_dir_all(&snap_dir).unwrap();

    let prev_snap = snap_dir.join("anthropic-0001.json");
    let curr_snap = snap_dir.join("anthropic-0002.json");
    std::fs::write(&prev_snap, r#"{"body":"prev"}"#).unwrap();
    std::fs::write(&curr_snap, r#"{"body":"curr"}"#).unwrap();

    let diag = CacheBustDiagnostic {
        timestamp: "1776978542".into(),
        secs_since_last_cache_activity: Some(10),
        cache_ttl_secs: 3600,
        thinking_level: "High".into(),
        model_id: "claude-opus-4-7".into(),
        effort: None,
        bust_reason: BustReason::Unexplained,
        prev_model_id: None,
        prev_thinking_level: None,
        prev_effort: None,
        prev_usage: None,
        curr_usage: Usage::default(),
        prev_context_tokens: 0,
        curr_context_tokens: 0,
        session_total_cost: "0.0".into(),
        session_api_calls: 1,
        recent_request_snapshots: vec![curr_snap.clone(), prev_snap.clone()],
    };

    let bust_path = dump_cache_bust_diagnostic_in(&bust_dir, &diag).expect("dump succeeds");
    assert!(bust_path.exists(), "bust json written");

    // pins live in a sibling dir named after the bust stem
    let pin_dir = bust_dir.join("bust-1776978542-snapshots");
    assert!(pin_dir.is_dir(), "pin dir created next to bust json");

    let pinned_curr = pin_dir.join("anthropic-0002.json");
    let pinned_prev = pin_dir.join("anthropic-0001.json");
    assert!(pinned_curr.exists(), "curr snapshot copied");
    assert!(pinned_prev.exists(), "prev snapshot copied");

    // the bust json should reference the pinned copies, not the live paths
    let body = std::fs::read_to_string(&bust_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let paths = parsed["recent_request_snapshots"].as_array().unwrap();
    assert_eq!(paths.len(), 2);
    assert!(
        paths[0].as_str().unwrap().ends_with(
            std::path::Path::new("bust-1776978542-snapshots/anthropic-0002.json")
                .to_str()
                .unwrap()
        ),
        "path 0 should point to pinned copy, got {}",
        paths[0]
    );

    // rotation of the live dir must not affect the pinned bundle
    std::fs::remove_file(&prev_snap).unwrap();
    std::fs::remove_file(&curr_snap).unwrap();
    assert!(pinned_curr.exists(), "pinned copy survives live deletion");
    assert!(pinned_prev.exists(), "pinned copy survives live deletion");
}
