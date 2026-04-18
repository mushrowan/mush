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
}

impl Model {
    /// whether this model prefers the patch-based edit tool.
    /// GPT models (except gpt-4 and OSS variants) are trained on the patch format
    pub fn uses_patch_tool(&self) -> bool {
        let id = self.id.as_str();
        id.contains("gpt-") && !id.contains("oss") && !id.contains("gpt-4")
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
}

/// anthropic beta flags enabled via the `anthropic-beta` header on oauth
/// requests. api-key users don't typically need these; they're claude-code
/// parity flags for oauth sessions.
///
/// defaults follow the policy agreed with mush's primary user:
/// - `context_1m`, `effort`, `context_management` default to `true`
/// - `redact_thinking`, `advisor`, `advanced_tool_use` default to `false`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
}
