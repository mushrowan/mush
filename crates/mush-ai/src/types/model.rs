use serde::{Deserialize, Serialize};

use super::newtypes::{ApiKey, BaseUrl, ModelId, SessionId, Temperature, TokenCount};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum Api {
    AnthropicMessages,
    OpenaiCompletions,
    OpenaiResponses,
}

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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputModality {
    Text,
    Image,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
pub struct Model {
    pub id: ModelId,
    pub name: String,
    pub api: Api,
    pub provider: Provider,
    pub base_url: BaseUrl,
    #[builder(default)]
    pub reasoning: bool,
    #[builder(default = vec![InputModality::Text])]
    pub input: Vec<InputModality>,
    #[builder(default)]
    pub cost: ModelCost,
    pub context_window: TokenCount,
    #[serde(default)]
    pub effective_context_window: Option<TokenCount>,
    pub max_output_tokens: TokenCount,
    #[serde(default)]
    #[builder(default)]
    pub supports_adaptive_thinking: bool,
    #[serde(default)]
    #[builder(default)]
    pub supported_thinking_levels: Vec<ThinkingLevel>,
    #[serde(default)]
    pub default_thinking_level: Option<ThinkingLevel>,
}

impl Model {
    #[must_use]
    pub fn clamp_thinking(&self, level: &ThinkingLevel) -> ThinkingLevel {
        let available = &self.supported_thinking_levels;
        if available.is_empty() || available.contains(level) {
            return level.clone();
        }
        let Some(at) = ThinkingLevel::LADDER.iter().position(|rung| rung == level) else {
            return available[0].clone();
        };
        ThinkingLevel::LADDER[at..]
            .iter()
            .chain(ThinkingLevel::LADDER[..at].iter().rev())
            .find(|rung| available.contains(rung))
            .unwrap_or(&available[0])
            .clone()
    }

    #[must_use]
    pub fn compaction_window(&self) -> TokenCount {
        self.effective_context_window.unwrap_or(self.context_window)
    }

    pub fn uses_patch_tool(&self) -> bool {
        let id = self.id.as_str();
        id.contains("gpt-") && !id.contains("oss") && !id.contains("gpt-4")
    }

    #[must_use]
    pub fn supports_caching(&self) -> bool {
        self.provider == Provider::Anthropic
            || self.cost.cache_read > 0.0
            || self.cost.cache_write > 0.0
    }

    pub fn supports_native_parallel_calls(&self) -> bool {
        let id = self.id.as_str();
        self.provider == Provider::Anthropic
            || id.contains("gpt-")
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
    #[serde(untagged)]
    Other(String),
}

impl ThinkingLevel {
    pub const LADDER: [Self; 6] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
    ];

    #[must_use]
    pub fn normalize_visible(&self) -> Self {
        match self {
            Self::Minimal => Self::Low,
            other => other.clone(),
        }
    }

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EffortLevel {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    #[serde(untagged)]
    Other(String),
}

impl EffortLevel {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Other(name) => name.as_str(),
        }
    }

    #[must_use]
    pub fn from_effort_str(effort: &str) -> Option<Self> {
        match effort.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => None,
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            other => Some(Self::Other(other.to_string())),
        }
    }
}

impl ThinkingLevel {
    #[must_use]
    pub fn effort_level(&self) -> Option<EffortLevel> {
        match self {
            ThinkingLevel::Off => None,
            ThinkingLevel::Minimal => Some(EffortLevel::Minimal),
            ThinkingLevel::Low => Some(EffortLevel::Low),
            ThinkingLevel::Medium => Some(EffortLevel::Medium),
            ThinkingLevel::High => Some(EffortLevel::High),
            ThinkingLevel::Xhigh => Some(EffortLevel::Xhigh),
            ThinkingLevel::Other(name) => Some(EffortLevel::Other(name.clone())),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamOptions {
    pub temperature: Option<Temperature>,
    pub max_tokens: Option<TokenCount>,
    pub api_key: Option<ApiKey>,
    pub thinking: Option<ThinkingLevel>,
    pub effort: Option<EffortLevel>,
    pub session_id: Option<SessionId>,
    pub account_id: Option<String>,
    pub cache_retention: Option<CacheRetention>,
    pub anthropic_betas: Option<AnthropicBetas>,
    pub force_budget_thinking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct AnthropicBetas {
    pub context_1m: bool,
    pub effort: bool,
    pub context_management: bool,
    pub thinking_token_count: bool,
    pub thinking_updates: bool,
    pub advisor: bool,
    pub advanced_tool_use: bool,
    pub fallback_credit: bool,
}

impl Default for AnthropicBetas {
    fn default() -> Self {
        Self {
            context_1m: true,
            effort: true,
            context_management: true,
            thinking_token_count: true,
            thinking_updates: true,
            advisor: true,
            advanced_tool_use: true,
            fallback_credit: false,
        }
    }
}

impl AnthropicBetas {
    #[must_use]
    pub fn gated_for_context_window(&self, context_window: TokenCount) -> Self {
        const STANDARD_CONTEXT_WINDOW: u64 = 200_000;
        let mut gated = self.clone();
        if context_window.get() <= STANDARD_CONTEXT_WINDOW {
            gated.context_1m = false;
        }
        gated
    }

    #[must_use]
    pub fn to_header_value(&self) -> String {
        let mut betas: Vec<&'static str> = vec!["claude-code-20250219", "oauth-2025-04-20"];
        if self.context_1m {
            betas.push("context-1m-2025-08-07");
        }
        betas.push("interleaved-thinking-2025-05-14");
        if self.thinking_token_count {
            betas.push("thinking-token-count-2026-05-13");
        }
        if self.context_management {
            betas.push("context-management-2025-06-27");
        }
        betas.extend([
            "prompt-caching-scope-2026-01-05",
            "mid-conversation-system-2026-04-07",
            "per-turn-control-2026-07-01",
            "mid-conversation-tool-changes-2026-07-01",
        ]);
        if self.advisor {
            betas.push("advisor-tool-2026-03-01");
        }
        if self.advanced_tool_use {
            betas.push("advanced-tool-use-2025-11-20");
        }
        betas.push("mid-conversation-system-clear-at-2026-08-21");
        if self.effort {
            betas.push("effort-2025-11-24");
        }
        if self.fallback_credit {
            betas.push("fallback-credit-2026-07-01");
        }
        betas.extend([
            "dangerous-tool-use-2026-09-03",
            "thinking-binding-controls-2026-08-01",
        ]);
        if self.thinking_updates {
            betas.push("thinking-display-updates-2026-08-18");
        }
        betas.extend([
            "afk-mode-2026-01-31",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolResultTrimming {
    #[default]
    SlidingWindow,
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
        assert_eq!(ThinkingLevel::Off.effort_name(), None);
    }

    #[test]
    fn thinking_level_other_serde_round_trips_untagged() {
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
        let model = model_with_cost(0.0, 0.0);
        assert_eq!(model.effective_context_window, None);
        assert_eq!(model.compaction_window(), TokenCount::new(200_000));
    }

    #[test]
    fn compaction_window_prefers_effective_when_set() {
        let mut model = model_with_cost(0.0, 0.0);
        model.context_window = TokenCount::new(400_000);
        model.effective_context_window = Some(TokenCount::new(272_000));
        assert_eq!(model.compaction_window(), TokenCount::new(272_000));
    }

    #[test]
    fn supports_caching_is_true_when_either_cache_rate_is_nonzero() {
        assert!(model_with_cost(0.3, 3.75).supports_caching());
        assert!(model_with_cost(0.1, 0.0).supports_caching());
        assert!(model_with_cost(0.0, 0.5).supports_caching());
    }

    #[test]
    fn an_unpriced_model_caches_only_when_it_is_anthropic() {
        assert!(model_with_cost(0.0, 0.0).supports_caching());
        let mut other = model_with_cost(0.0, 0.0);
        other.provider = Provider::Custom("fixture".into());
        assert!(!other.supports_caching());
    }

    #[test]
    fn anthropic_models_support_native_parallel_calls() {
        let model = model_with_cost(0.0, 0.0);
        assert_eq!(model.provider, Provider::Anthropic);
        assert_eq!(model.api, Api::AnthropicMessages);
        assert!(model.supports_native_parallel_calls());
    }

    #[test]
    fn gated_betas_drop_1m_for_standard_context_models() {
        let betas = AnthropicBetas::default();
        assert!(betas.context_1m, "default should request 1M");
        let gated = betas.gated_for_context_window(TokenCount::new(200_000));
        assert!(!gated.context_1m, "200K model must not send context-1m");
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
