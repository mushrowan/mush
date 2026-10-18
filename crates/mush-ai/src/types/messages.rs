use serde::{Deserialize, Deserializer, Serialize};

use super::content::{AssistantContentPart, ThinkingContent, ToolResultContentPart, UserContent};
use super::model::{Api, Model, Provider};
use super::newtypes::{Dollars, ModelId, PromptId, Timestamp, TokenCount, ToolCallId, ToolName};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: TokenCount,
    pub output_tokens: TokenCount,
    pub cache_read_tokens: TokenCount,
    pub cache_write_tokens: TokenCount,
    #[serde(default, skip_serializing_if = "TokenCount::is_zero")]
    pub cache_write_1h_tokens: TokenCount,
}

impl Usage {
    pub fn total_tokens(&self) -> TokenCount {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    pub fn total_input_tokens(&self) -> TokenCount {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Cost {
    pub input: Dollars,
    pub output: Dollars,
    pub cache_read: Dollars,
    pub cache_write: Dollars,
}

impl Cost {
    pub fn total(&self) -> Dollars {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct UserMessage {
    pub content: UserContent,
    pub timestamp_ms: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<PromptId>,
}

impl UserMessage {
    #[must_use]
    pub fn text(&self) -> String {
        self.content.text()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, bon::Builder)]
#[builder(on(String, into))]
pub struct AssistantMessage {
    #[builder(default)]
    pub content: Vec<AssistantContentPart>,
    pub model: ModelId,
    pub provider: Provider,
    pub api: Api,
    #[builder(default)]
    pub usage: Usage,
    #[builder(default = StopReason::Stop)]
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp_ms: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl AssistantMessage {
    #[must_use]
    pub fn is_replayable(&self) -> bool {
        !matches!(self.stop_reason, StopReason::Error | StopReason::Aborted)
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                AssistantContentPart::Text(t) if !t.text.is_empty() => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    #[must_use]
    pub fn produced_by(&self, model: &Model) -> bool {
        self.provider == model.provider && self.api == model.api && self.model == model.id
    }

    #[must_use]
    pub fn thinking(&self) -> Option<String> {
        let blocks: Vec<&str> = self
            .content
            .iter()
            .filter_map(|p| match p {
                AssistantContentPart::Thinking(ThinkingContent::Thinking { thinking, .. }) => {
                    Some(thinking.trim_end())
                }
                _ => None,
            })
            .filter(|thinking| !thinking.trim().is_empty())
            .collect();
        (!blocks.is_empty()).then(|| blocks.join("\n\n"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum ToolOutcome {
    Success,
    Error,
}

impl ToolOutcome {
    #[must_use]
    pub fn is_error(self) -> bool {
        self == Self::Error
    }

    #[must_use]
    pub fn is_success(self) -> bool {
        self == Self::Success
    }
}

impl From<bool> for ToolOutcome {
    fn from(is_error: bool) -> Self {
        if is_error { Self::Error } else { Self::Success }
    }
}

fn deserialise_outcome<'de, D: Deserializer<'de>>(d: D) -> Result<ToolOutcome, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Enum(ToolOutcome),
        Bool(bool),
    }
    match Raw::deserialize(d)? {
        Raw::Enum(o) => Ok(o),
        Raw::Bool(b) => Ok(ToolOutcome::from(b)),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultMessage {
    pub tool_call_id: ToolCallId,
    pub tool_name: ToolName,
    pub content: Vec<ToolResultContentPart>,
    #[serde(alias = "is_error", deserialize_with = "deserialise_outcome")]
    pub outcome: ToolOutcome,
    pub timestamp_ms: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

pub const DETAIL_TEXT_LIMIT: usize = 64 * 1024;

pub fn cap_detail_text(details: &mut serde_json::Value, limit: usize) -> bool {
    let Some(fields) = details.as_object_mut() else {
        return false;
    };
    let mut capped = false;
    for value in fields.values_mut() {
        let serde_json::Value::String(text) = value else {
            continue;
        };
        if text.len() <= limit {
            continue;
        }
        let start = text.ceil_char_boundary(text.len() - limit.saturating_sub(64));
        let start = text[start..]
            .find('\n')
            .filter(|&newline| newline < 4096)
            .map_or(start, |newline| start + newline + 1);
        *text = format!("[{} earlier bytes not kept]\n{}", start, &text[start..]);
        capped = true;
    }
    capped
}

impl ToolResultMessage {
    pub fn cap_details(&mut self) -> bool {
        self.details
            .as_mut()
            .is_some_and(|details| cap_detail_text(details, DETAIL_TEXT_LIMIT))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    Event(super::EventMessage),
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "role", rename_all = "snake_case")]
        enum Stored {
            User(UserMessage),
            Assistant(AssistantMessage),
            ToolResult(ToolResultMessage),
            Event(super::EventMessage),
        }
        Ok(match Stored::deserialize(deserializer)? {
            Stored::User(user) => {
                if let Some(id) = &user.prompt_id
                    && (id.starts_with("orchestrator-event-") || id.starts_with("local-control-"))
                {
                    Self::Event(super::EventMessage {
                        content: user.text(),
                        timestamp_ms: user.timestamp_ms,
                        kind: super::EventKind::Legacy,
                        receipts: vec![super::EventReceipt {
                            id: id.clone(),
                            timestamp_ms: user.timestamp_ms,
                            kind: super::EventKind::Legacy,
                            origin: super::EventOrigin::Legacy,
                        }],
                    })
                } else {
                    Self::User(user)
                }
            }
            Stored::Assistant(message) => Self::Assistant(message),
            Stored::ToolResult(message) => Self::ToolResult(message),
            Stored::Event(message) => Self::Event(message),
        })
    }
}

#[must_use]
pub fn find_recent_boundary(messages: &[Message], keep: usize) -> Option<usize> {
    let nth = keep.checked_sub(1)?;
    messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, msg)| matches!(msg, Message::User(_) | Message::Event(_)))
        .nth(nth)
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_detail_text_keeps_its_tail_and_small_fields_stay() {
        let stdout: String = (0..20_000).map(|n| format!("line {n}\n")).collect();
        let mut details = serde_json::json!({"stdout": stdout, "stderr": "small", "images": 3});

        assert!(cap_detail_text(&mut details, 1024));

        let capped = details["stdout"].as_str().unwrap();
        assert!(capped.len() < 1100, "{}", capped.len());
        assert!(capped.starts_with('['), "{capped}");
        assert!(
            capped.contains("earlier bytes not kept]\nline "),
            "{capped}"
        );
        assert!(capped.ends_with("line 19999\n"));
        assert_eq!(details["stderr"], "small");
        assert_eq!(details["images"], 3);
        assert!(capped.len() <= 1024);
        assert!(
            !cap_detail_text(&mut details, 1024),
            "capping twice changes nothing"
        );
    }

    #[test]
    fn thinking_joins_every_block_with_text_and_leaves_signatures_and_redacted_out() {
        let block = |thinking: &str| {
            AssistantContentPart::Thinking(ThinkingContent::Thinking {
                thinking: thinking.into(),
                signature: Some("sig".into()),
            })
        };
        let message = AssistantMessage::builder()
            .content(vec![
                block(""),
                AssistantContentPart::Thinking(ThinkingContent::Redacted {
                    data: "opaque".into(),
                }),
                block("Weighing the two options.\n"),
                block("  "),
                block("Checked the cache, editing it next."),
            ])
            .model("m".into())
            .provider(Provider::Anthropic)
            .api(Api::AnthropicMessages)
            .timestamp_ms(Timestamp::zero())
            .build();

        assert_eq!(
            message.thinking().as_deref(),
            Some("Weighing the two options.\n\nChecked the cache, editing it next.")
        );

        let hidden = AssistantMessage::builder()
            .content(vec![block("")])
            .model("m".into())
            .provider(Provider::Anthropic)
            .api(Api::AnthropicMessages)
            .timestamp_ms(Timestamp::zero())
            .build();
        assert_eq!(hidden.thinking(), None);
    }

    #[test]
    fn usage_total_tokens() {
        let usage = Usage {
            input_tokens: TokenCount::new(100),
            output_tokens: TokenCount::new(50),
            cache_read_tokens: TokenCount::new(25),
            cache_write_tokens: TokenCount::new(10),
            ..Default::default()
        };
        assert_eq!(usage.total_tokens(), TokenCount::new(185));
    }

    #[test]
    fn usage_total_input_tokens() {
        let usage = Usage {
            input_tokens: TokenCount::new(100),
            output_tokens: TokenCount::new(50),
            cache_read_tokens: TokenCount::new(25),
            cache_write_tokens: TokenCount::new(10),
            ..Default::default()
        };
        assert_eq!(usage.total_input_tokens(), TokenCount::new(135));
    }

    #[test]
    fn usage_without_hour_long_writes_reads_and_writes_as_before() {
        let saved = serde_json::json!({
            "input_tokens": 1,
            "output_tokens": 2,
            "cache_read_tokens": 3,
            "cache_write_tokens": 4
        });

        let usage: Usage = serde_json::from_value(saved.clone()).unwrap();

        assert_eq!(usage.cache_write_1h_tokens, TokenCount::ZERO);
        assert_eq!(serde_json::to_value(usage).unwrap(), saved);
        let hourly = Usage {
            cache_write_1h_tokens: TokenCount::new(4),
            ..usage
        };
        assert_eq!(
            serde_json::to_value(hourly).unwrap()["cache_write_1h_tokens"],
            4
        );
    }

    #[test]
    fn cost_total() {
        let cost = Cost {
            input: Dollars::new(0.003),
            output: Dollars::new(0.015),
            cache_read: Dollars::new(0.001),
            cache_write: Dollars::new(0.002),
        };
        assert!((cost.total().get() - 0.021).abs() < f64::EPSILON);
    }

    #[test]
    fn message_serialisation_roundtrip() {
        let msg = Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hello".into()))
                .timestamp_ms(Timestamp::from_ms(1_234_567_890))
                .build(),
        );
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn stop_reason_serde() {
        let json = serde_json::to_string(&StopReason::ToolUse).unwrap();
        assert_eq!(json, r#""tool_use""#);
    }

    #[test]
    fn tool_outcome_serialises_as_enum() {
        let json = serde_json::to_string(&ToolOutcome::Error).unwrap();
        assert_eq!(json, r#""error""#);
        let json = serde_json::to_string(&ToolOutcome::Success).unwrap();
        assert_eq!(json, r#""success""#);
    }

    #[test]
    fn tool_result_message_deserialises_legacy_is_error() {
        let json = r#"{
            "tool_call_id": "tc_1",
            "tool_name": "bash",
            "content": [],
            "is_error": true,
            "timestamp_ms": 0
        }"#;
        let msg: ToolResultMessage = serde_json::from_str(json).unwrap();
        assert!(msg.outcome.is_error());

        let json = r#"{
            "tool_call_id": "tc_2",
            "tool_name": "read",
            "content": [],
            "is_error": false,
            "timestamp_ms": 0
        }"#;
        let msg: ToolResultMessage = serde_json::from_str(json).unwrap();
        assert!(msg.outcome.is_success());
    }

    #[test]
    fn tool_result_message_deserialises_new_outcome() {
        let json = r#"{
            "tool_call_id": "tc_1",
            "tool_name": "bash",
            "content": [],
            "outcome": "error",
            "timestamp_ms": 0
        }"#;
        let msg: ToolResultMessage = serde_json::from_str(json).unwrap();
        assert!(msg.outcome.is_error());
    }

    #[test]
    fn tool_outcome_from_bool() {
        assert_eq!(ToolOutcome::from(true), ToolOutcome::Error);
        assert_eq!(ToolOutcome::from(false), ToolOutcome::Success);
    }

    fn user(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    fn assistant() -> Message {
        Message::Assistant(
            AssistantMessage::builder()
                .model(ModelId::from("test"))
                .provider(Provider::Anthropic)
                .api(Api::AnthropicMessages)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    fn tool_result() -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: ToolCallId::from("tc"),
            tool_name: ToolName::from("bash"),
            content: vec![],
            outcome: ToolOutcome::Success,
            timestamp_ms: Timestamp::zero(),
            details: None,
        })
    }

    #[test]
    fn find_recent_boundary_returns_none_when_not_enough_pivots() {
        let msgs = vec![user("a"), assistant(), user("b")];
        assert_eq!(
            find_recent_boundary(&msgs, 3),
            None,
            "only 2 users, keep=3 should be None"
        );
    }

    #[test]
    fn find_recent_boundary_locates_nth_pivot_from_end() {
        let msgs = vec![
            user("old"), // 0
            assistant(),
            tool_result(),
            user("mid"), // 3
            assistant(),
            user("recent"), // 5
            assistant(),
        ];

        assert_eq!(find_recent_boundary(&msgs, 3), Some(0));
        assert_eq!(find_recent_boundary(&msgs, 2), Some(3));
        assert_eq!(find_recent_boundary(&msgs, 1), Some(5));
    }

    #[test]
    fn find_recent_boundary_zero_keep_returns_none() {
        let msgs = vec![user("a"), assistant()];
        assert_eq!(find_recent_boundary(&msgs, 0), None);
    }
}
