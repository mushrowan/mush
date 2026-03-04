//! streaming event types for LLM responses

use crate::types::{AssistantMessage, StopReason};

/// events emitted during assistant message streaming
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// stream started, initial partial message
    Start { partial: AssistantMessage },
    /// new text content block started
    TextStart { content_index: usize },
    /// incremental text
    TextDelta { content_index: usize, delta: String },
    /// text content block finished
    TextEnd { content_index: usize, text: String },
    /// thinking block started
    ThinkingStart { content_index: usize },
    /// incremental thinking
    ThinkingDelta { content_index: usize, delta: String },
    /// thinking block finished
    ThinkingEnd {
        content_index: usize,
        thinking: String,
    },
    /// tool call started
    ToolCallStart { content_index: usize },
    /// incremental tool call arguments
    ToolCallDelta { content_index: usize, delta: String },
    /// tool call finished
    ToolCallEnd {
        content_index: usize,
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// stream completed successfully
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    /// stream ended with an error
    Error {
        reason: StopReason,
        message: AssistantMessage,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    #[test]
    fn stream_event_variants_are_constructable() {
        // just verifying the types compile and can be instantiated
        let msg = AssistantMessage {
            content: vec![],
            model: "test".into(),
            provider: Provider::Custom("test".into()),
            api: Api::AnthropicMessages,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::zero(),
        };

        let _start = StreamEvent::Start {
            partial: msg.clone(),
        };
        let _delta = StreamEvent::TextDelta {
            content_index: 0,
            delta: "hello".into(),
        };
        let _done = StreamEvent::Done {
            reason: StopReason::Stop,
            message: msg,
        };
    }
}
