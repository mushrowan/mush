//! model definitions, provider config, and streaming options

use serde::{Deserialize, Serialize};

use super::newtypes::{ApiKey, BaseUrl, ModelId, SessionId, Temperature, TokenCount};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum Api {
    AnthropicMessages,
    OpenaiCompletions,
    OpenaiResponses,
}

/// known first-party providers
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Anthropic,
    OpenRouter,
    #[serde(untagged)]
    Custom(String),
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Anthropic => write!(f, "anthropic"),
            Self::OpenRouter => write!(f, "openrouter"),
            Self::Custom(name) => write!(f, "{name}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCost {
    /// cost per million input tokens
    pub input: f64,
    /// cost per million output tokens
    pub output: f64,
    /// cost per million cache read tokens
    pub cache_read: f64,
    /// cost per million cache write tokens
    pub cache_write: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputModality {
    Text,
    Image,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Model {
    pub id: ModelId,
    pub name: String,
    pub api: Api,
    pub provider: Provider,
    pub base_url: BaseUrl,
    pub reasoning: bool,
    pub input: Vec<InputModality>,
    pub cost: ModelCost,
    pub context_window: TokenCount,
    /// the input-token budget to compact against, when it differs from
    /// the displayed [`context_window`]. for a route that displays a total
    /// window larger than the input ceiling it actually honours (an
    /// "input + output" split), set this to the input ceiling so
    /// auto-compaction fires before the provider errors while the status
    /// bar still shows the full window. `None` means "same as
    /// context_window" - the common case
    #[serde(default)]
    pub effective_context_window: Option<TokenCount>,
    pub max_output_tokens: TokenCount,
    /// when set, the provider should request anthropic's adaptive
    /// thinking mode instead of fixed-budget thinking. only meaningful
    /// when [`reasoning`] is true. older anthropic models and
    /// non-anthropic models leave this `false`
    #[serde(default)]
    pub supports_adaptive_thinking: bool,
    /// the ordered set of thinking levels this model accepts (cheap →
    /// expensive). drives the `ctrl+t` cycler so users can only land
    /// on levels the model actually supports.
    ///
    /// empty = the cycler falls back to the legacy hardcoded walk
    /// (Off → Low → Medium → High → Xhigh) since we haven't curated
    /// a list for this model yet
    #[serde(default)]
    pub supported_thinking_levels: Vec<ThinkingLevel>,
    /// the model's recommended starting level. applied on the first
    /// switch to this model when the user has no per-cwd override.
    /// `None` means "fall back to config / Off"
    #[serde(default)]
    pub default_thinking_level: Option<ThinkingLevel>,
}

impl Model {
    /// the token budget compaction should target. defaults to the full
    /// [`context_window`] but falls back to [`effective_context_window`]
    /// when a route's usable input budget is smaller than its marketed
    /// window, so auto-compaction fires before the provider's real input
    /// ceiling rather than after it
    #[must_use]
    pub fn compaction_window(&self) -> TokenCount {
        self.effective_context_window.unwrap_or(self.context_window)
    }

    /// whether this model prefers the patch-based edit tool.
    /// GPT models (except gpt-4 and OSS variants) are trained on the patch format
    pub fn uses_patch_tool(&self) -> bool {
        let id = self.id.as_str();
        id.contains("gpt-") && !id.contains("oss") && !id.contains("gpt-4")
    }

    /// whether this model has prompt caching available, inferred from
    /// pricing. any non-zero cache_read or cache_write rate means the
    /// upstream provider charges for (and therefore offers) a cached
    /// path. used by the TUI to decide whether to surface the cache
    /// warmth timer at all
    #[must_use]
    pub fn supports_caching(&self) -> bool {
        self.cost.cache_read > 0.0 || self.cost.cache_write > 0.0
    }

    /// whether this model handles parallel tool calls natively (no batch tool needed).
    /// OpenAI responses API and reasoning models support this
    pub fn supports_native_parallel_calls(&self) -> bool {
        let id = self.id.as_str();
        id.contains("gpt-")
            || id.contains("codex")
            || id.starts_with("o1")
            || id.starts_with("o3")
            || id.starts_with("o4")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    /// any provider-defined effort level we don't model with a typed
    /// variant (e.g. anthropic's `max`, or a future `ultra-max`). carries
    /// the raw effort name so newly-added upstream levels are supported by
    /// autodetection without a code change. only appears on effort-based
    /// models, never on the budget-token ladder
    #[serde(untagged)]
    Other(String),
}

impl ThinkingLevel {
    /// Keep `Minimal` for compatibility with older configs/runtime state, but
    /// treat it as `Low` in visible mush controls and persisted visible prefs.
    #[must_use]
    pub fn normalize_visible(&self) -> Self {
        match self {
            Self::Minimal => Self::Low,
            other => other.clone(),
        }
    }

    /// the canonical effort name sent on the wire (anthropic
    /// `output_config.effort`, openai `reasoning_effort`). `Off` returns
    /// `None` since callers treat "no reasoning" separately.
    #[must_use]
    pub fn effort_name(&self) -> Option<&str> {
        match self {
            Self::Off => None,
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::Xhigh => Some("xhigh"),
            Self::Other(name) => Some(name.as_str()),
        }
    }

    /// parse a provider effort string into a level. known names map to
    /// typed variants; `off`/`none` to `Off`; anything else (e.g. `max`,
    /// a future `ultra-max`) becomes `Other` so new upstream levels work
    /// via autodetection without a code change.
    #[must_use]
    pub fn from_effort_str(effort: &str) -> Self {
        match effort.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Self::Off,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::Xhigh,
            other => Self::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub temperature: Option<Temperature>,
    pub max_tokens: Option<TokenCount>,
    pub api_key: Option<ApiKey>,
    pub thinking: Option<ThinkingLevel>,
    /// stable session identifier for provider-side prompt caching
    pub session_id: Option<SessionId>,
    /// optional account id for providers that need account-scoped headers
    pub account_id: Option<String>,
    /// prompt cache retention preference for providers that support it
    pub cache_retention: Option<CacheRetention>,
    /// anthropic oauth beta flags. `None` falls back to `AnthropicBetas::default()`
    pub anthropic_betas: Option<AnthropicBetas>,
    /// when true, ignore the model's `supports_adaptive_thinking` flag
    /// and use fixed-budget thinking. lets users opt out of adaptive
    /// for cost predictability or A/B comparisons. default: false
    pub force_budget_thinking: bool,
}

/// anthropic beta flags enabled via the `anthropic-beta` header on oauth
/// requests. api-key users don't typically need these; they're claude-code
/// parity flags for oauth sessions.
///
/// defaults follow the policy agreed with mush's primary user:
/// - `context_1m`, `effort`, `context_management` default to `true`
/// - `redact_thinking`, `advisor`, `advanced_tool_use` default to `true`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct AnthropicBetas {
    /// `context-1m-2025-08-07` - unlocks 1M context window on compatible models
    pub context_1m: bool,
    /// `effort-2025-11-24` - enables the `output_config.effort` request field
    pub effort: bool,
    /// `context-management-2025-06-27` - server-side context edits
    pub context_management: bool,
    /// `redact-thinking-2026-02-12` - allows thinking redaction in responses
    pub redact_thinking: bool,
    /// `advisor-tool-2026-03-01` - claude-code advisor tool support
    pub advisor: bool,
    /// `advanced-tool-use-2025-11-20` - advanced tool-use features
    pub advanced_tool_use: bool,
}

impl Default for AnthropicBetas {
    fn default() -> Self {
        Self {
            context_1m: true,
            effort: true,
            context_management: true,
            redact_thinking: true,
            advisor: true,
            advanced_tool_use: true,
        }
    }
}

impl AnthropicBetas {
    /// drop the 1M context beta when the model can't actually use it.
    /// the `context-1m-2025-08-07` beta is only legal for models whose
    /// context window exceeds the standard 200K. sending it for a 200K
    /// model is rejected with a misleading "long context beta is not yet
    /// available for this subscription" 400, so gate it on the model's
    /// discovered context window rather than the account.
    #[must_use]
    pub fn gated_for_context_window(&self, context_window: TokenCount) -> Self {
        const STANDARD_CONTEXT_WINDOW: u64 = 200_000;
        let mut gated = self.clone();
        if context_window.get() <= STANDARD_CONTEXT_WINDOW {
            gated.context_1m = false;
        }
        gated
    }

    /// compose the `anthropic-beta` header value for an oauth request.
    /// always includes the persistent flags required for claude-code parity
    /// (identity, oauth, streaming, thinking, caching scope). toggleable flags
    /// are appended based on `self`.
    #[must_use]
    pub fn to_header_value(&self) -> String {
        let mut betas: Vec<&'static str> = vec!["claude-code-20250219", "oauth-2025-04-20"];
        if self.context_1m {
            betas.push("context-1m-2025-08-07");
        }
        betas.push("interleaved-thinking-2025-05-14");
        if self.redact_thinking {
            betas.push("redact-thinking-2026-02-12");
        }
        if self.context_management {
            betas.push("context-management-2025-06-27");
        }
        betas.extend([
            "prompt-caching-scope-2026-01-05",
            "mid-conversation-system-2026-04-07",
        ]);
        if self.advisor {
            betas.push("advisor-tool-2026-03-01");
        }
        if self.advanced_tool_use {
            betas.push("advanced-tool-use-2025-11-20");
        }
        if self.effort {
            betas.push("effort-2025-11-24");
        }
        betas.extend([
            "extended-cache-ttl-2025-04-11",
            "cache-diagnosis-2026-04-07",
        ]);
        betas.join(",")
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheRetention {
    None,
    Short,
    Long,
}

/// strategy for trimming old tool results in the message array
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolResultTrimming {
    /// trim old tool results at request time using a sliding window.
    /// suitable for models/providers without prompt caching
    #[default]
    SlidingWindow,
    /// never trim tool results at request time, preserving prefix stability
    /// for providers with prompt caching (anthropic, etc.)
    Preserve,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_serde() {
        let json = serde_json::to_string(&Api::AnthropicMessages).unwrap();
        assert_eq!(json, r#""anthropic-messages""#);
    }

    #[test]
    fn thinking_level_normalize_visible_maps_minimal_to_low() {
        assert_eq!(
            ThinkingLevel::Minimal.normalize_visible(),
            ThinkingLevel::Low
        );
        assert_eq!(ThinkingLevel::High.normalize_visible(), ThinkingLevel::High);
    }

    #[test]
    fn thinking_level_from_effort_str_maps_known_efforts() {
        // known effort names map to typed variants; case/whitespace is
        // normalised
        assert_eq!(
            ThinkingLevel::from_effort_str("minimal"),
            ThinkingLevel::Minimal
        );
        assert_eq!(ThinkingLevel::from_effort_str("low"), ThinkingLevel::Low);
        assert_eq!(
            ThinkingLevel::from_effort_str("medium"),
            ThinkingLevel::Medium
        );
        assert_eq!(ThinkingLevel::from_effort_str("High"), ThinkingLevel::High);
        assert_eq!(
            ThinkingLevel::from_effort_str("xhigh"),
            ThinkingLevel::Xhigh
        );
        assert_eq!(ThinkingLevel::from_effort_str("none"), ThinkingLevel::Off);
    }

    #[test]
    fn thinking_level_unknown_efforts_become_other() {
        // the whole point of the hybrid: a level we don't model (today's
        // `max`, a future `ultra-max`) is captured verbatim so the cycler
        // can autodetect and use it without a code change
        assert_eq!(
            ThinkingLevel::from_effort_str("max"),
            ThinkingLevel::Other("max".into())
        );
        assert_eq!(
            ThinkingLevel::from_effort_str("ultra-max"),
            ThinkingLevel::Other("ultra-max".into())
        );
    }

    #[test]
    fn thinking_level_effort_name_round_trips() {
        for level in [
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Other("max".into()),
        ] {
            let name = level.effort_name().unwrap();
            assert_eq!(ThinkingLevel::from_effort_str(name), level);
        }
        // Off carries no wire name
        assert_eq!(ThinkingLevel::Off.effort_name(), None);
    }

    #[test]
    fn thinking_level_other_serde_round_trips_untagged() {
        // Other serialises as its bare name (like Provider::Custom) and
        // parses back. known names still parse to their typed variants
        let level = ThinkingLevel::Other("max".into());
        let json = serde_json::to_string(&level).unwrap();
        assert_eq!(json, r#""max""#);
        assert_eq!(
            serde_json::from_str::<ThinkingLevel>(r#""max""#).unwrap(),
            ThinkingLevel::Other("max".into())
        );
        assert_eq!(
            serde_json::from_str::<ThinkingLevel>(r#""high""#).unwrap(),
            ThinkingLevel::High
        );
    }

    fn model_with_cost(cache_read: f64, cache_write: f64) -> Model {
        Model {
            id: ModelId::from("test/model"),
            name: "test".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: BaseUrl::from("https://example.invalid"),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read,
                cache_write,
            },
            context_window: TokenCount::new(200_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(8_192),
            supports_adaptive_thinking: false,
            supported_thinking_levels: vec![],
            default_thinking_level: None,
        }
    }

    #[test]
    fn compaction_window_falls_back_to_context_window_when_effective_unset() {
        // most models leave effective_context_window None: the budget we
        // compact against is just the displayed context window
        let model = model_with_cost(0.0, 0.0);
        assert_eq!(model.effective_context_window, None);
        assert_eq!(model.compaction_window(), TokenCount::new(200_000));
    }

    #[test]
    fn compaction_window_prefers_effective_when_set() {
        // gpt-5.5 codex case: the marketed/displayed window is 400k but
        // the real input budget before the provider errors is 272k. the
        // status bar shows context_window; compaction keys off the
        // smaller effective_context_window so we compact before the
        // provider's input ceiling rather than after it
        let mut model = model_with_cost(0.0, 0.0);
        model.context_window = TokenCount::new(400_000);
        model.effective_context_window = Some(TokenCount::new(272_000));
        assert_eq!(model.compaction_window(), TokenCount::new(272_000));
    }

    #[test]
    fn supports_caching_is_true_when_either_cache_rate_is_nonzero() {
        // anthropic-style models price both directions
        assert!(model_with_cost(0.3, 3.75).supports_caching());
        // openrouter sometimes only reports input_cache_read
        assert!(model_with_cost(0.1, 0.0).supports_caching());
        // a write-only price is unusual but still implies caching exists
        assert!(model_with_cost(0.0, 0.5).supports_caching());
    }

    #[test]
    fn supports_caching_is_false_when_neither_rate_is_priced() {
        assert!(!model_with_cost(0.0, 0.0).supports_caching());
    }

    #[test]
    fn gated_betas_drop_1m_for_standard_context_models() {
        // the long-context beta is illegal for 200K models; sending it
        // triggers a misleading "not available for this subscription"
        // 400. gate it on the model's actual context window
        let betas = AnthropicBetas::default();
        assert!(betas.context_1m, "default should request 1M");
        let gated = betas.gated_for_context_window(TokenCount::new(200_000));
        assert!(!gated.context_1m, "200K model must not send context-1m");
        // unrelated flags stay untouched
        assert_eq!(gated.effort, betas.effort);
        assert_eq!(gated.context_management, betas.context_management);
    }

    #[test]
    fn gated_betas_keep_1m_for_long_context_models() {
        let gated = AnthropicBetas::default().gated_for_context_window(TokenCount::new(1_000_000));
        assert!(gated.context_1m, "1M model may send context-1m");
    }

    #[test]
    fn gated_betas_leave_disabled_1m_off() {
        let betas = AnthropicBetas {
            context_1m: false,
            ..AnthropicBetas::default()
        };
        let gated = betas.gated_for_context_window(TokenCount::new(1_000_000));
        assert!(!gated.context_1m);
    }
}
