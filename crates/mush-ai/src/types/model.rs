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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ThinkingLevel {
    /// Keep `Minimal` for compatibility with older configs/runtime state, but
    /// treat it as `Low` in visible mush controls and persisted visible prefs.
    #[must_use]
    pub const fn normalize_visible(self) -> Self {
        match self {
            Self::Minimal => Self::Low,
            other => other,
        }
    }

    /// map a codex `effort` string to the corresponding [`ThinkingLevel`].
    /// returns `None` for unknown values so callers can drop them on the
    /// floor without bricking on an upstream rename.
    #[must_use]
    pub fn from_codex_str(effort: &str) -> Option<Self> {
        match effort {
            "off" | "none" => Some(Self::Off),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "max" => Some(Self::Xhigh),
            _ => None,
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
/// - `redact_thinking`, `advisor`, `advanced_tool_use` default to `false`
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
            redact_thinking: false,
            advisor: false,
            advanced_tool_use: false,
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
        let mut betas: Vec<&'static str> = vec![
            "claude-code-20250219",
            "oauth-2025-04-20",
            "fine-grained-tool-streaming-2025-05-14",
            "interleaved-thinking-2025-05-14",
            "prompt-caching-scope-2026-01-05",
        ];
        if self.context_1m {
            betas.push("context-1m-2025-08-07");
        }
        if self.effort {
            betas.push("effort-2025-11-24");
        }
        if self.context_management {
            betas.push("context-management-2025-06-27");
        }
        if self.redact_thinking {
            betas.push("redact-thinking-2026-02-12");
        }
        if self.advisor {
            betas.push("advisor-tool-2026-03-01");
        }
        if self.advanced_tool_use {
            betas.push("advanced-tool-use-2025-11-20");
        }
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
    fn thinking_level_from_codex_str_maps_known_efforts() {
        // codex effort strings are lowercase short tokens. unknown values
        // return None so callers can drop them on the floor without
        // bricking on an upstream rename
        assert_eq!(
            ThinkingLevel::from_codex_str("minimal"),
            Some(ThinkingLevel::Minimal)
        );
        assert_eq!(
            ThinkingLevel::from_codex_str("low"),
            Some(ThinkingLevel::Low)
        );
        assert_eq!(
            ThinkingLevel::from_codex_str("medium"),
            Some(ThinkingLevel::Medium)
        );
        assert_eq!(
            ThinkingLevel::from_codex_str("high"),
            Some(ThinkingLevel::High)
        );
        assert_eq!(
            ThinkingLevel::from_codex_str("none"),
            Some(ThinkingLevel::Off)
        );
        // unknown variant: drop instead of guess
        assert_eq!(ThinkingLevel::from_codex_str("turbocharged"), None);
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
            max_output_tokens: TokenCount::new(8_192),
            supports_adaptive_thinking: false,
            supported_thinking_levels: vec![],
            default_thinking_level: None,
        }
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
