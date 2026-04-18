use mush_ai::types::{CacheRetention, Provider, TokenCount, Usage};
use mush_tui::cache::{
    BustReason, CACHE_COLD_DISPLAY_SECS, CACHE_WARN_SECS, CacheAnomaly, CacheTimer, CallConfig,
    TokenStats, cache_ttl_secs, detect_cache_anomalies,
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
