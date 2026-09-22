use std::time::Instant;

use mush_ai::types::*;

#[must_use]
pub fn cache_ttl_secs(
    provider: &Provider,
    retention: Option<&CacheRetention>,
    is_oauth: bool,
) -> u16 {
    match provider {
        Provider::Anthropic => {
            if is_oauth {
                return 3600;
            }
            match retention.copied().unwrap_or(CacheRetention::Short) {
                CacheRetention::None => 0,
                CacheRetention::Short => 300,
                CacheRetention::Long => 3600,
            }
        }
        _ => 300,
    }
}

#[must_use]
pub fn cache_warning_tiers(provider: &Provider, retention: Option<&CacheRetention>) -> Vec<u16> {
    match provider {
        Provider::Anthropic => match retention.copied().unwrap_or(CacheRetention::Short) {
            CacheRetention::None => Vec::new(),
            CacheRetention::Short => vec![240],
            CacheRetention::Long => vec![240, 3540],
        },
        Provider::Custom(name) if name == "openai" || name == "openai-codex" => vec![300, 3540],
        Provider::OpenRouter => vec![240, 3540],
        _ => vec![240],
    }
}

#[must_use]
pub fn cache_warning_display(
    provider: &Provider,
    retention: Option<&CacheRetention>,
    elapsed_secs: Option<u64>,
) -> Option<String> {
    let elapsed = elapsed_secs?;
    let active = cache_warning_tiers(provider, retention)
        .into_iter()
        .filter_map(|tier| tier.checked_sub(elapsed.min(u16::MAX as u64) as u16))
        .filter(|remaining| *remaining > 0)
        .map(format_duration)
        .collect::<Vec<_>>();

    (!active.is_empty()).then(|| active.join("/"))
}

fn format_duration(secs: u16) -> String {
    let mins = secs / 60;
    let secs = secs % 60;
    format!("{mins}:{secs:02}")
}

#[must_use]
pub fn direct_route_serving_provider(
    provider: &Provider,
) -> Option<std::borrow::Cow<'static, str>> {
    match provider {
        Provider::Anthropic => Some(std::borrow::Cow::Borrowed("Anthropic")),
        Provider::OpenRouter => None,
        Provider::Custom(id) => Some(std::borrow::Cow::Owned(id.clone())),
    }
}

#[must_use]
pub fn resolve_cache_ttl_secs(
    model: &Model,
    options: &StreamOptions,
    cache_timer_enabled: bool,
    last_serving_provider: Option<&str>,
) -> u16 {
    if !cache_timer_enabled || !model.supports_caching() {
        return 0;
    }
    if let Some(name) = last_serving_provider
        && let Some(behaviour) = crate::serving_provider::serving_provider_cache(name)
    {
        return behaviour.ttl_secs;
    }
    let is_oauth = options.api_key.as_ref().is_some_and(|k| k.is_oauth_token());
    cache_ttl_secs(&model.provider, options.cache_retention.as_ref(), is_oauth)
}

#[derive(Debug, Clone)]
pub struct CacheTimer {
    pub ttl_secs: u16,
    last_active: Option<Instant>,
    pub warn_sent: bool,
    pub expired_sent: bool,
    pub last_serving_provider: Option<String>,
}

impl CacheTimer {
    #[must_use]
    pub fn new(ttl_secs: u16) -> Self {
        Self {
            ttl_secs,
            last_active: None,
            warn_sent: false,
            expired_sent: false,
            last_serving_provider: None,
        }
    }

    pub fn refresh(&mut self) {
        self.last_active = Some(Instant::now());
        self.warn_sent = false;
        self.expired_sent = false;
    }

    #[must_use]
    pub fn remaining_secs(&self) -> Option<u16> {
        self.remaining_after(self.elapsed_secs()?)
    }

    fn remaining_after(&self, elapsed: u64) -> Option<u16> {
        if self.ttl_secs == 0 {
            return None;
        }
        let elapsed = u16::try_from(elapsed).unwrap_or(u16::MAX);
        Some(self.ttl_secs.saturating_sub(elapsed))
    }

    #[must_use]
    pub fn elapsed_secs(&self) -> Option<u64> {
        self.last_active.map(|t| t.elapsed().as_secs())
    }

    pub fn seed_for_model(
        &mut self,
        model: &Model,
        options: &StreamOptions,
        cache_timer_enabled: bool,
    ) {
        let initial = direct_route_serving_provider(&model.provider).map(Into::into);
        self.set_serving_provider(initial, model, options, cache_timer_enabled);
    }

    pub fn apply_serving_provider(
        &mut self,
        name: impl Into<String>,
        model: &Model,
        options: &StreamOptions,
        cache_timer_enabled: bool,
    ) {
        self.set_serving_provider(Some(name.into()), model, options, cache_timer_enabled);
    }

    fn set_serving_provider(
        &mut self,
        name: Option<String>,
        model: &Model,
        options: &StreamOptions,
        cache_timer_enabled: bool,
    ) {
        self.last_serving_provider = name;
        self.ttl_secs = resolve_cache_ttl_secs(
            model,
            options,
            cache_timer_enabled,
            self.last_serving_provider.as_deref(),
        );
    }
}

pub const CACHE_WARN_SECS: u16 = 60;

pub const CACHE_COLD_DISPLAY_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextManagementMode {
    LocalMonotonic,
    ProviderManaged,
    SessionManaged,
}

impl ContextManagementMode {
    #[must_use]
    pub fn from_call_config(cfg: Option<&CallConfig>) -> Self {
        cfg.map_or(Self::LocalMonotonic, |c| c.context_management)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CacheAnomaly {
    ContextDecrease {
        prev: TokenCount,
        curr: TokenCount,
    },
    CacheBust {
        prev_cache_read: TokenCount,
        curr_cache_read: TokenCount,
        curr_cache_write: TokenCount,
        reason: BustReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BustReason {
    Unexplained,
    ModelChanged,
    ThinkingChanged,
    EffortChanged,
    HistoryReshaped,
    SystemPromptChanged,
    ToolsChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CallConfig {
    pub model_id: String,
    pub thinking_level: String,
    pub effort: Option<String>,
    pub context_management: ContextManagementMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages_prefix_fingerprint: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_fingerprint: Option<u64>,
}

impl CallConfig {
    #[must_use]
    fn classify_change(prev: &Self, curr: &Self) -> BustReason {
        if prev.model_id != curr.model_id {
            BustReason::ModelChanged
        } else if prev.thinking_level != curr.thinking_level {
            BustReason::ThinkingChanged
        } else if prev.effort != curr.effort {
            BustReason::EffortChanged
        } else if let (Some(p), Some(c)) = (prev.tools_fingerprint, curr.tools_fingerprint)
            && p != c
        {
            BustReason::ToolsChanged
        } else if let (Some(p), Some(c)) = (prev.system_fingerprint, curr.system_fingerprint)
            && p != c
        {
            BustReason::SystemPromptChanged
        } else if let (Some(p), Some(c)) = (
            prev.messages_prefix_fingerprint,
            curr.messages_prefix_fingerprint,
        ) && p != c
        {
            BustReason::HistoryReshaped
        } else {
            BustReason::Unexplained
        }
    }
}

#[must_use]
pub fn detect_cache_anomalies(
    prev: Option<&Usage>,
    curr: &Usage,
    prev_config: Option<&CallConfig>,
    curr_config: Option<&CallConfig>,
) -> Vec<CacheAnomaly> {
    let Some(prev) = prev else {
        return Vec::new();
    };

    let mut anomalies = Vec::new();
    let prev_ctx = prev.total_input_tokens();
    let curr_ctx = curr.total_input_tokens();

    let prev_mode = ContextManagementMode::from_call_config(prev_config);
    let curr_mode = ContextManagementMode::from_call_config(curr_config);

    if curr_ctx < prev_ctx
        && prev_mode == ContextManagementMode::LocalMonotonic
        && curr_mode == ContextManagementMode::LocalMonotonic
    {
        anomalies.push(CacheAnomaly::ContextDecrease {
            prev: prev_ctx,
            curr: curr_ctx,
        });
    }

    let prev_read = prev.cache_read_tokens.get();
    let prev_total = prev_ctx.get().max(1);
    let curr_read = curr.cache_read_tokens.get();
    let prev_was_cached = prev_read * 2 > prev_total;
    let read_dropped = prev_read > 0 && curr_read * 4 < prev_read;
    let write_spiked = curr.cache_write_tokens > prev.cache_write_tokens;

    let provider_context_shrank = curr_ctx < prev_ctx
        && prev_mode != ContextManagementMode::LocalMonotonic
        && curr_mode != ContextManagementMode::LocalMonotonic;

    if prev_was_cached && read_dropped && write_spiked {
        let reason = match (prev_config, curr_config) {
            (Some(p), Some(c)) => CallConfig::classify_change(p, c),
            _ => BustReason::Unexplained,
        };
        if provider_context_shrank && reason == BustReason::Unexplained {
            return anomalies;
        }
        anomalies.push(CacheAnomaly::CacheBust {
            prev_cache_read: prev.cache_read_tokens,
            curr_cache_read: curr.cache_read_tokens,
            curr_cache_write: curr.cache_write_tokens,
            reason,
        });
    }

    anomalies
}

#[derive(Debug, Clone, Default)]
pub struct TokenStats {
    pub total_cost: Dollars,
    pub total_tokens: TokenCount,
    pub input_tokens: TokenCount,
    pub output_tokens: TokenCount,
    pub cache_read_tokens: TokenCount,
    pub cache_write_tokens: TokenCount,
    pub context_tokens: TokenCount,
    pub context_window: TokenCount,
    prev_usage: Option<Usage>,
    prev_call_config: Option<CallConfig>,
}

impl TokenStats {
    #[must_use]
    pub fn new(context_window: TokenCount) -> Self {
        Self {
            context_window,
            ..Default::default()
        }
    }

    pub fn update(&mut self, usage: &Usage, cost: Option<Dollars>) -> Vec<CacheAnomaly> {
        self.update_with_config(usage, cost, None)
    }

    pub fn update_with_config(
        &mut self,
        usage: &Usage,
        cost: Option<Dollars>,
        call_config: Option<CallConfig>,
    ) -> Vec<CacheAnomaly> {
        if let Some(c) = cost {
            self.total_cost += c;
        }

        let anomalies = detect_cache_anomalies(
            self.prev_usage.as_ref(),
            usage,
            self.prev_call_config.as_ref(),
            call_config.as_ref(),
        );
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
                    reason,
                } => {
                    tracing::warn!(
                        prev_cache_read = prev_cache_read.get(),
                        curr_cache_read = curr_cache_read.get(),
                        curr_cache_write = curr_cache_write.get(),
                        reason = ?reason,
                        "cache anomaly: prefix rewritten"
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
        if call_config.is_some() {
            self.prev_call_config = call_config;
        }

        anomalies
    }

    pub fn reset(&mut self) {
        let window = self.context_window;
        *self = Self::new(window);
    }

    pub fn reset_live_state(&mut self, context_tokens: TokenCount) {
        self.prev_usage = None;
        self.prev_call_config = None;
        self.context_tokens = context_tokens;
    }

    #[must_use]
    pub fn prev_usage(&self) -> Option<&Usage> {
        self.prev_usage.as_ref()
    }

    #[must_use]
    pub fn prev_call_config(&self) -> Option<&CallConfig> {
        self.prev_call_config.as_ref()
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheBustDiagnostic {
    pub timestamp: String,
    pub secs_since_last_cache_activity: Option<u64>,
    pub cache_ttl_secs: u16,
    pub thinking_level: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub bust_reason: BustReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_messages_prefix_fingerprint: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curr_messages_prefix_fingerprint: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_system_fingerprint: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curr_system_fingerprint: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_tools_fingerprint: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curr_tools_fingerprint: Option<u64>,
    pub prev_usage: Option<Usage>,
    pub curr_usage: Usage,
    pub prev_context_tokens: u64,
    pub curr_context_tokens: u64,
    pub session_total_cost: String,
    pub session_api_calls: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_request_snapshots: Vec<std::path::PathBuf>,
}

pub fn dump_cache_bust_diagnostic(diag: &CacheBustDiagnostic) -> Option<std::path::PathBuf> {
    dump_cache_bust_diagnostic_in(&cache_bust_dir(), diag)
}

pub fn dump_cache_bust_diagnostic_in(
    dir: &std::path::Path,
    diag: &CacheBustDiagnostic,
) -> Option<std::path::PathBuf> {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::error!(error = %e, "failed to create cache bust dump dir");
        return None;
    }

    let stem = format!(
        "bust-{}",
        diag.timestamp.replace([':', ' '], "-").replace('T', "_")
    );
    let path = dir.join(format!("{stem}.json"));

    let mut diag_out = diag.clone();
    if !diag.recent_request_snapshots.is_empty() {
        let pin_dir = dir.join(format!("{stem}-snapshots"));
        if let Err(e) = std::fs::create_dir_all(&pin_dir) {
            tracing::error!(error = %e, "failed to create pin dir for bust snapshots");
        } else {
            let mut pinned = Vec::with_capacity(diag.recent_request_snapshots.len());
            for (i, src) in diag.recent_request_snapshots.iter().enumerate() {
                let filename = src.file_name().map_or_else(
                    || std::ffi::OsString::from(format!("snapshot-{i}.json")),
                    std::ffi::OsStr::to_os_string,
                );
                let dst = pin_dir.join(&filename);
                match std::fs::copy(src, &dst) {
                    Ok(_) => pinned.push(dst),
                    Err(e) => {
                        tracing::warn!(
                            src = %src.display(),
                            error = %e,
                            "failed to pin request snapshot for bust bundle"
                        );
                    }
                }
            }
            diag_out.recent_request_snapshots = pinned;
        }
    }

    match serde_json::to_string_pretty(&diag_out) {
        Ok(json) => match mush_ai::private_io::write_private(&path, json) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{Api, BaseUrl, ModelCost, ModelId, Provider, StreamOptions, TokenCount};

    fn openrouter_model_with_caching() -> Model {
        Model::builder()
            .id(ModelId::from("anthropic/claude-sonnet-4.5"))
            .name("test".into())
            .api(Api::AnthropicMessages)
            .provider(Provider::OpenRouter)
            .base_url(BaseUrl::from("https://openrouter.ai/api/v1"))
            .context_window(TokenCount::new(200_000))
            .max_output_tokens(TokenCount::new(8_192))
            .supported_thinking_levels(vec![])
            .cost(ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            })
            .build()
    }

    #[test]
    fn resolve_uses_serving_provider_ttl_when_known() {
        let model = openrouter_model_with_caching();
        let opts = StreamOptions::default();

        let baseline = resolve_cache_ttl_secs(&model, &opts, true, None);
        assert_eq!(baseline, 300);

        let with_openai = resolve_cache_ttl_secs(&model, &opts, true, Some("OpenAI"));
        assert_eq!(with_openai, 600);
    }

    #[test]
    fn resolve_falls_back_when_serving_provider_unknown() {
        let model = openrouter_model_with_caching();
        let opts = StreamOptions::default();
        let ttl = resolve_cache_ttl_secs(&model, &opts, true, Some("Unknown"));
        assert_eq!(ttl, 300);
    }

    #[test]
    fn resolve_returns_zero_when_caching_off_regardless_of_serving_provider() {
        let mut model = openrouter_model_with_caching();
        model.cost.cache_read = 0.0;
        model.cost.cache_write = 0.0;
        let opts = StreamOptions::default();
        let ttl = resolve_cache_ttl_secs(&model, &opts, true, Some("Unknown"));
        assert_eq!(ttl, 0);
    }

    #[test]
    fn direct_route_serving_provider_named_per_first_party_provider() {
        assert_eq!(
            direct_route_serving_provider(&Provider::Anthropic).as_deref(),
            Some("Anthropic")
        );
        assert_eq!(
            direct_route_serving_provider(&Provider::Custom("openai".into())).as_deref(),
            Some("openai")
        );
        assert_eq!(
            direct_route_serving_provider(&Provider::Custom("custom".into())).as_deref(),
            Some("custom")
        );
    }

    #[test]
    fn direct_route_serving_provider_is_none_for_openrouter() {
        assert!(direct_route_serving_provider(&Provider::OpenRouter).is_none());
    }

    #[test]
    fn cache_warning_tiers_include_short_and_long_windows() {
        assert_eq!(cache_warning_tiers(&Provider::Anthropic, None), vec![240]);
        assert_eq!(
            cache_warning_tiers(&Provider::Anthropic, Some(&CacheRetention::Long)),
            vec![240, 3540]
        );
        assert_eq!(
            cache_warning_tiers(&Provider::Custom("openai-codex".into()), None),
            vec![300, 3540]
        );
    }

    #[test]
    fn cache_warning_display_counts_down_active_tiers() {
        assert_eq!(
            cache_warning_display(&Provider::Anthropic, Some(&CacheRetention::Long), Some(30))
                .as_deref(),
            Some("3:30/58:30")
        );
        assert_eq!(
            cache_warning_display(&Provider::Anthropic, Some(&CacheRetention::Long), Some(250))
                .as_deref(),
            Some("54:50")
        );
    }

    #[test]
    fn an_idle_night_leaves_no_cache() {
        let timer = CacheTimer::new(3600);
        assert_eq!(timer.remaining_after(18 * 60 * 60 + 1800), Some(0));
    }

    #[test]
    fn a_disabled_timer_has_no_remaining_time() {
        let timer = CacheTimer::new(0);
        assert_eq!(timer.remaining_after(10), None);
    }

    #[test]
    fn apply_serving_provider_updates_state_and_recomputes_ttl() {
        let model = openrouter_model_with_caching();
        let opts = StreamOptions::default();
        let mut timer = CacheTimer::new(300);
        timer.apply_serving_provider("Unknown", &model, &opts, true);
        assert_eq!(timer.last_serving_provider.as_deref(), Some("Unknown"));
        assert_eq!(timer.ttl_secs, 300);
        timer.apply_serving_provider("OpenAI", &model, &opts, true);
        assert_eq!(timer.last_serving_provider.as_deref(), Some("OpenAI"));
        assert_eq!(timer.ttl_secs, 600);
    }

    #[test]
    fn detect_cache_anomalies_ignores_context_decrease_for_openai_auto_truncation() {
        let prev = Usage {
            input_tokens: TokenCount::new(131_000),
            output_tokens: TokenCount::new(100_000),
            cache_read_tokens: TokenCount::ZERO,
            cache_write_tokens: TokenCount::ZERO,
        };
        let curr = Usage {
            input_tokens: TokenCount::new(126_000),
            output_tokens: TokenCount::new(100_000),
            cache_read_tokens: TokenCount::ZERO,
            cache_write_tokens: TokenCount::ZERO,
        };
        let prev_cfg = CallConfig {
            model_id: "gpt-5.4".into(),
            thinking_level: "High".into(),
            effort: None,
            context_management: ContextManagementMode::ProviderManaged,
            messages_prefix_fingerprint: None,
            system_fingerprint: None,
            tools_fingerprint: None,
        };
        let curr_cfg = prev_cfg.clone();

        let anomalies =
            detect_cache_anomalies(Some(&prev), &curr, Some(&prev_cfg), Some(&curr_cfg));
        assert!(
            !anomalies
                .iter()
                .any(|a| matches!(a, CacheAnomaly::ContextDecrease { .. })),
            "openai responses auto truncation should not be flagged: {anomalies:?}"
        );
    }

    #[test]
    fn detect_cache_anomalies_ignores_context_decrease_for_codex_session_route() {
        let prev = Usage {
            input_tokens: TokenCount::new(131_000),
            output_tokens: TokenCount::new(100_000),
            cache_read_tokens: TokenCount::ZERO,
            cache_write_tokens: TokenCount::ZERO,
        };
        let curr = Usage {
            input_tokens: TokenCount::new(126_000),
            output_tokens: TokenCount::new(100_000),
            cache_read_tokens: TokenCount::ZERO,
            cache_write_tokens: TokenCount::ZERO,
        };
        let prev_cfg = CallConfig {
            model_id: "gpt-5.3-codex".into(),
            thinking_level: "High".into(),
            effort: None,
            context_management: ContextManagementMode::SessionManaged,
            messages_prefix_fingerprint: None,
            system_fingerprint: None,
            tools_fingerprint: None,
        };
        let curr_cfg = prev_cfg.clone();

        let anomalies =
            detect_cache_anomalies(Some(&prev), &curr, Some(&prev_cfg), Some(&curr_cfg));
        assert!(
            !anomalies
                .iter()
                .any(|a| matches!(a, CacheAnomaly::ContextDecrease { .. })),
            "codex session route should not be flagged: {anomalies:?}"
        );
    }
}
