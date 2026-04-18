//! message types, usage tracking, and cost accounting

use serde::{Deserialize, Deserializer, Serialize};

use super::content::{AssistantContentPart, ToolResultContentPart, UserContent};
use super::model::{Api, Provider};
use super::newtypes::{Dollars, ModelId, Timestamp, TokenCount, ToolCallId, ToolName};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: TokenCount,
    pub output_tokens: TokenCount,
    pub cache_read_tokens: TokenCount,
    pub cache_write_tokens: TokenCount,
}

impl Usage {
    /// total tokens processed in this API call (all categories)
    pub fn total_tokens(&self) -> TokenCount {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    /// total input tokens (context size for this call).
    /// for anthropic: input_tokens is non-cached, cache_read + cache_write are the rest
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserMessage {
    pub content: UserContent,
    pub timestamp_ms: Timestamp,
}

impl UserMessage {
    /// extract the text content of this message
    #[must_use]
    pub fn text(&self) -> String {
        self.content.text()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantMessage {
    pub content: Vec<AssistantContentPart>,
    pub model: ModelId,
    pub provider: Provider,
    pub api: Api,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp_ms: Timestamp,
}

impl AssistantMessage {
    /// extract concatenated text content (excludes thinking and tool calls)
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

    /// extract the thinking text, if any
    #[must_use]
    pub fn thinking(&self) -> Option<String> {
        self.content.iter().find_map(|p| match p {
            AssistantContentPart::Thinking(t) => Some(t.text().to_string()),
            _ => None,
        })
    }
}

/// whether a tool execution succeeded or failed
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

/// deserialise from either `"outcome": "success"` or legacy `"is_error": true`
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

/// pivot role used by [`find_recent_boundary`] to decide which messages
/// count as turn boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPivot {
    /// count user messages (provider sliding-window uses this)
    User,
    /// count assistant messages (compaction observation masking uses this)
    Assistant,
}

/// scan `messages` from the end and return the index where the N most
/// recent pivot messages begin. returns `None` if the list contains fewer
/// than `keep` pivots, meaning "not enough turns to define a cutoff".
///
/// callers typically treat `None` as "no-op, don't trim/mask anything".
#[must_use]
pub fn find_recent_boundary(messages: &[Message], pivot: TurnPivot, keep: usize) -> Option<usize> {
    if keep == 0 {
        return None;
    }
    let mut count = 0;
    for (i, msg) in messages.iter().enumerate().rev() {
        let is_pivot = match pivot {
            TurnPivot::User => matches!(msg, Message::User(_)),
            TurnPivot::Assistant => matches!(msg, Message::Assistant(_)),
        };
        if is_pivot {
            count += 1;
            if count == keep {
                return Some(i);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_total_tokens() {
        let usage = Usage {
            input_tokens: TokenCount::new(100),
            output_tokens: TokenCount::new(50),
            cache_read_tokens: TokenCount::new(25),
            cache_write_tokens: TokenCount::new(10),
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
        };
        assert_eq!(usage.total_input_tokens(), TokenCount::new(135));
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
        let msg = Message::User(UserMessage {
            content: UserContent::Text("hello".into()),
            timestamp_ms: Timestamp::from_ms(1_234_567_890),
        });
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
        Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp_ms: Timestamp::zero(),
        })
    }

    fn assistant() -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![],
            model: ModelId::from("test"),
            provider: Provider::Anthropic,
            api: Api::AnthropicMessages,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::zero(),
        })
    }

    fn tool_result() -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: ToolCallId::from("tc"),
            tool_name: ToolName::from("bash"),
            content: vec![],
            outcome: ToolOutcome::Success,
            timestamp_ms: Timestamp::zero(),
        })
    }

    #[test]
    fn find_recent_boundary_returns_none_when_not_enough_pivots() {
        let msgs = vec![user("a"), assistant(), user("b")];
        assert_eq!(
            find_recent_boundary(&msgs, TurnPivot::User, 3),
            None,
            "only 2 users, keep=3 should be None"
        );
        assert_eq!(
            find_recent_boundary(&msgs, TurnPivot::Assistant, 2),
            None,
            "only 1 assistant, keep=2 should be None"
        );
    }

    #[test]
    fn find_recent_boundary_locates_nth_pivot_from_end() {
        // indices:            0       1       2       3       4         5       6
        let msgs = vec![
            user("old"),    // 0
            assistant(),    // 1
            tool_result(),  // 2
            user("mid"),    // 3
            assistant(),    // 4
            user("recent"), // 5
            assistant(),    // 6
        ];

        // 3 most recent user messages: the oldest of those 3 is at idx 0
        assert_eq!(find_recent_boundary(&msgs, TurnPivot::User, 3), Some(0));
        // 2 most recent user messages: boundary at idx 3 (user "mid")
        assert_eq!(find_recent_boundary(&msgs, TurnPivot::User, 2), Some(3));
        // 1 most recent: user "recent" at idx 5
        assert_eq!(find_recent_boundary(&msgs, TurnPivot::User, 1), Some(5));

        // assistants: 3 total. keep=3 → idx 1. keep=2 → idx 4. keep=1 → idx 6.
        assert_eq!(
            find_recent_boundary(&msgs, TurnPivot::Assistant, 3),
            Some(1)
        );
        assert_eq!(
            find_recent_boundary(&msgs, TurnPivot::Assistant, 2),
            Some(4)
        );
        assert_eq!(
            find_recent_boundary(&msgs, TurnPivot::Assistant, 1),
            Some(6)
        );
    }

    #[test]
    fn find_recent_boundary_zero_keep_returns_none() {
        let msgs = vec![user("a"), assistant()];
        assert_eq!(find_recent_boundary(&msgs, TurnPivot::User, 0), None);
    }
}
