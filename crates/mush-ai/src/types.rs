//! core message, model, and provider types

use serde::{Deserialize, Serialize};

// -- content types --

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextContent {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThinkingContent {
    pub thinking: String,
    /// opaque signature for multi-turn continuity
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default)]
    pub redacted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageContent {
    /// base64 encoded image data
    pub data: String,
    /// eg "image/png", "image/jpeg"
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

// -- user content --

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Parts(Vec<UserContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContentPart {
    Text(TextContent),
    Image(ImageContent),
}

// -- assistant content --

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContentPart {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

// -- tool result content --

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContentPart {
    Text(TextContent),
    Image(ImageContent),
}

// -- usage --

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl Usage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Cost {
    pub fn total(&self) -> f64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

// -- stop reason --

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

// -- messages --

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserMessage {
    pub content: UserContent,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantMessage {
    pub content: Vec<AssistantContentPart>,
    pub model: String,
    pub provider: String,
    pub api: Api,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ToolResultContentPart>,
    pub is_error: bool,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

// -- api + provider --

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

// -- model + cost --

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
    pub id: String,
    pub name: String,
    pub api: Api,
    pub provider: Provider,
    pub base_url: String,
    pub reasoning: bool,
    pub input: Vec<InputModality>,
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_output_tokens: u64,
}

// -- thinking level --

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
}

// -- stream options --

#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u64>,
    pub api_key: Option<String>,
    pub thinking: Option<ThinkingLevel>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_total_tokens() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 25,
            cache_write_tokens: 10,
        };
        assert_eq!(usage.total_tokens(), 185);
    }

    #[test]
    fn cost_total() {
        let cost = Cost {
            input: 0.003,
            output: 0.015,
            cache_read: 0.001,
            cache_write: 0.002,
        };
        assert!((cost.total() - 0.021).abs() < f64::EPSILON);
    }

    #[test]
    fn message_serialisation_roundtrip() {
        let msg = Message::User(UserMessage {
            content: UserContent::Text("hello".into()),
            timestamp_ms: 1234567890,
        });
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn api_serde() {
        let json = serde_json::to_string(&Api::AnthropicMessages).unwrap();
        assert_eq!(json, r#""anthropic-messages""#);
    }

    #[test]
    fn stop_reason_serde() {
        let json = serde_json::to_string(&StopReason::ToolUse).unwrap();
        assert_eq!(json, r#""tool_use""#);
    }
}
