use serde::{Deserialize, Serialize};

use crate::types::{AssistantMessage, StopReason};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StreamEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        text: String,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
        thinking: String,
    },
    ToolCallStart {
        content_index: usize,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
    },
    ToolCallEnd {
        content_index: usize,
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
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
        let msg = AssistantMessage::builder()
            .model("test".into())
            .provider(Provider::Custom("test".into()))
            .api(Api::AnthropicMessages)
            .timestamp_ms(Timestamp::zero())
            .build();

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
