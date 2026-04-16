//! prompt cache tracking and anomaly detection

use std::time::Instant;

use mush_ai::types::*;

/// determine cache TTL in seconds from provider and retention settings
/// returns 0 if caching is disabled or provider doesn't support it
pub fn cache_ttl_secs(provider: &Provider, retention: Option<&CacheRetention>) -> u16 {
    match provider {
        Provider::Anthropic => match retention.copied().unwrap_or(CacheRetention::Short) {
            CacheRetention::None => 0,
            CacheRetention::Short => 300, // 5 minutes
            CacheRetention::Long => 3600, // 1 hour
        },
        // openai: automatic caching, ~5-10 min, use 5 as conservative estimate
        // openrouter: passes through to underlying provider, assume anthropic-like
        _ => 300,
    }
}

/// tracks prompt cache warmth with countdown and notification flags
#[derive(Debug, Clone)]
pub struct CacheTimer {
    /// cache TTL in seconds (determined from provider/retention config)
    pub ttl_secs: u16,
    /// when the cache was last active (read or write)
    last_active: Option<Instant>,
    /// whether we already sent a "cache expiring soon" notification
    pub warn_sent: bool,
    /// whether we already sent a "cache expired" notification
    pub expired_sent: bool,
}

impl CacheTimer {
    #[must_use]
    pub fn new(ttl_secs: u16) -> Self {
        Self {
            ttl_secs,
            last_active: None,
            warn_sent: false,
            expired_sent: false,
        }
    }

    /// refresh the cache warmth timer (call when cache_read or cache_write > 0)
    pub fn refresh(&mut self) {
        self.last_active = Some(Instant::now());
        self.warn_sent = false;
        self.expired_sent = false;
    }

    /// seconds remaining before cache expires, None if no active cache
    #[must_use]
    pub fn remaining_secs(&self) -> Option<u16> {
        let elapsed = self.last_active?.elapsed().as_secs() as u16;
        if elapsed >= self.ttl_secs {
            Some(0)
        } else {
            Some(self.ttl_secs - elapsed)
        }
    }

    /// seconds since last cache activity, None if never active
    #[must_use]
    pub fn elapsed_secs(&self) -> Option<u64> {
        self.last_active.map(|t| t.elapsed().as_secs())
    }
}

/// seconds before cache expiry to trigger a warning notification
pub const CACHE_WARN_SECS: u16 = 60;

/// seconds after cache expires to keep showing "cold" in status bar
pub const CACHE_COLD_DISPLAY_SECS: u64 = 30;

/// detected anomaly in cache behaviour between consecutive API calls
#[derive(Debug, Clone, PartialEq)]
pub enum CacheAnomaly {
    /// context_tokens decreased without a compact/clear
    ContextDecrease { prev: TokenCount, curr: TokenCount },
    /// cache_read dropped significantly while cache_write spiked,
    /// suggesting the cached prefix was evicted
    CacheBust {
        prev_cache_read: TokenCount,
        curr_cache_read: TokenCount,
        curr_cache_write: TokenCount,
    },
}

/// compare consecutive API call usages and detect cache anomalies
///
/// returns an empty vec when prev is None (first call) or when
/// the usage pattern looks normal
#[must_use]
pub fn detect_cache_anomalies(prev: Option<&Usage>, curr: &Usage) -> Vec<CacheAnomaly> {
    let Some(prev) = prev else {
        return Vec::new();
    };

    let mut anomalies = Vec::new();
    let prev_ctx = prev.total_input_tokens();
    let curr_ctx = curr.total_input_tokens();

    // context should grow (or stay the same) without a compact
    if curr_ctx < prev_ctx {
        anomalies.push(CacheAnomaly::ContextDecrease {
            prev: prev_ctx,
            curr: curr_ctx,
        });
    }

    // cache bust: previous call had significant cache_read, this call
    // has much less cache_read with a cache_write spike.
    // threshold: prev cache_read was >50% of prev context, now dropped by >75%
    let prev_read = prev.cache_read_tokens.get();
    let prev_total = prev_ctx.get().max(1);
    let curr_read = curr.cache_read_tokens.get();
    let prev_was_cached = prev_read * 2 > prev_total; // >50% was cached
    let read_dropped = prev_read > 0 && curr_read * 4 < prev_read; // dropped by >75%
    let write_spiked = curr.cache_write_tokens > prev.cache_write_tokens;

    if prev_was_cached && read_dropped && write_spiked {
        anomalies.push(CacheAnomaly::CacheBust {
            prev_cache_read: prev.cache_read_tokens,
            curr_cache_read: curr.cache_read_tokens,
            curr_cache_write: curr.cache_write_tokens,
        });
    }

    anomalies
}

/// cumulative token and cost tracking for the session
#[derive(Debug, Clone, Default)]
pub struct TokenStats {
    /// total cost so far
    pub total_cost: Dollars,
    /// total tokens used (cumulative across all API calls)
    pub total_tokens: TokenCount,
    /// cumulative uncached input tokens
    pub input_tokens: TokenCount,
    /// cumulative output tokens
    pub output_tokens: TokenCount,
    /// cumulative cache-read tokens
    pub cache_read_tokens: TokenCount,
    /// cumulative cache-write tokens
    pub cache_write_tokens: TokenCount,
    /// last call's input tokens (actual context size)
    pub context_tokens: TokenCount,
    /// model's context window size
    pub context_window: TokenCount,
    /// usage from the previous API call (for anomaly detection)
    prev_usage: Option<Usage>,
}

impl TokenStats {
    /// create with a given context window
    #[must_use]
    pub fn new(context_window: TokenCount) -> Self {
        Self {
            context_window,
            ..Default::default()
        }
    }

    /// accumulate usage from an API call, returning any detected cache anomalies
    pub fn update(&mut self, usage: &Usage, cost: Option<Dollars>) -> Vec<CacheAnomaly> {
        if let Some(c) = cost {
            self.total_cost += c;
        }

        let anomalies = detect_cache_anomalies(self.prev_usage.as_ref(), usage);
        for anomaly in &anomalies {
            match anomaly {
                CacheAnomaly::ContextDecrease { prev, curr } => {
                    tracing::warn!(
                        prev_context = prev.get(),
                        curr_context = curr.get(),
                        delta = prev.get() - curr.get(),
                        "cache anomaly: context decreased without compact"
                    );
                }
                CacheAnomaly::CacheBust {
                    prev_cache_read,
                    curr_cache_read,
                    curr_cache_write,
                } => {
                    tracing::warn!(
                        prev_cache_read = prev_cache_read.get(),
                        curr_cache_read = curr_cache_read.get(),
                        curr_cache_write = curr_cache_write.get(),
                        "cache anomaly: probable cache bust (prefix evicted)"
                    );
                }
            }
        }

        self.total_tokens += usage.total_tokens();
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cache_read_tokens += usage.cache_read_tokens;
        self.cache_write_tokens += usage.cache_write_tokens;
        self.context_tokens = usage.total_input_tokens();
        self.prev_usage = Some(*usage);

        anomalies
    }

    /// reset all counters (keeps context_window)
    pub fn reset(&mut self) {
        let window = self.context_window;
        *self = Self::new(window);
    }

    /// snapshot of previous usage for diagnostic dumps
    #[must_use]
    pub fn prev_usage(&self) -> Option<&Usage> {
        self.prev_usage.as_ref()
    }
}

/// diagnostic snapshot written when a cache bust is detected.
/// captures everything needed to figure out why the bust happened
#[derive(Debug, serde::Serialize)]
pub struct CacheBustDiagnostic {
    pub timestamp: String,
    pub secs_since_last_cache_activity: Option<u64>,
    pub cache_ttl_secs: u16,
    pub thinking_level: String,
    pub model_id: String,
    pub prev_usage: Option<Usage>,
    pub curr_usage: Usage,
    pub prev_context_tokens: u64,
    pub curr_context_tokens: u64,
    pub session_total_cost: String,
    pub session_api_calls: u64,
}

/// write a cache bust diagnostic to ~/.local/state/mush/cache-busts/.
/// always writes regardless of log level. returns the path on success
pub fn dump_cache_bust_diagnostic(diag: &CacheBustDiagnostic) -> Option<std::path::PathBuf> {
    let dir = cache_bust_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::error!(error = %e, "failed to create cache bust dump dir");
        return None;
    }

    // include enough of the timestamp to be unique without being unwieldy
    let filename = format!(
        "bust-{}.json",
        diag.timestamp.replace([':', ' '], "-").replace('T', "_")
    );
    let path = dir.join(filename);

    match serde_json::to_string_pretty(diag) {
        Ok(json) => match std::fs::write(&path, json) {
            Ok(()) => Some(path),
            Err(e) => {
                tracing::error!(error = %e, "failed to write cache bust diagnostic");
                None
            }
        },
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize cache bust diagnostic");
            None
        }
    }
}

fn cache_bust_dir() -> std::path::PathBuf {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        std::path::PathBuf::from(state).join("mush/cache-busts")
    } else if let Some(home) = std::env::var_os("HOME") {
        std::path::PathBuf::from(home).join(".local/state/mush/cache-busts")
    } else {
        std::path::PathBuf::from(".mush/cache-busts")
    }
}
