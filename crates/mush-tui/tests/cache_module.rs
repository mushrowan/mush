use mush_ai::types::{CacheRetention, Provider, TokenCount, Usage};
use mush_tui::cache::{
    CACHE_COLD_DISPLAY_SECS, CACHE_WARN_SECS, CacheAnomaly, CacheTimer, TokenStats, cache_ttl_secs,
    detect_cache_anomalies,
};

#[test]
fn cache_module_exposes_timer_and_stats() {
    assert_eq!(CACHE_WARN_SECS, 60);
    assert_eq!(CACHE_COLD_DISPLAY_SECS, 30);
    assert_eq!(
        cache_ttl_secs(&Provider::Anthropic, Some(&CacheRetention::Short)),
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

    let anomalies = detect_cache_anomalies(Some(&prev), &curr);
    assert!(
        anomalies
            .iter()
            .any(|a| matches!(a, CacheAnomaly::CacheBust { .. }))
    );

    let mut stats = TokenStats::new(TokenCount::new(200_000));
    let first = stats.update(&prev, None);
    assert!(first.is_empty());
}
