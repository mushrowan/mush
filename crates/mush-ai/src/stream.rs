use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::{
    AssistantContentPart, AssistantMessage, StopReason, TextContent, ThinkingContent, Timestamp,
    ToolCall,
};

pub const ABORTED_MESSAGE: &str = "request was aborted";

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

#[derive(Debug, Clone)]
pub struct PartialMessage {
    message: AssistantMessage,
    parts: BTreeMap<usize, AssistantContentPart>,
}

impl PartialMessage {
    #[must_use]
    pub fn new(start: AssistantMessage) -> Self {
        let parts = start.content.iter().cloned().enumerate().collect();
        Self {
            message: start,
            parts,
        }
    }

    pub fn apply(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::Start { partial } => *self = Self::new(partial.clone()),
            StreamEvent::TextStart { content_index } => {
                self.parts.insert(*content_index, text_part(String::new()));
            }
            StreamEvent::TextDelta {
                content_index,
                delta,
            } => match self.parts.get_mut(content_index) {
                Some(AssistantContentPart::Text(text)) => text.text.push_str(delta),
                _ => {
                    self.parts.insert(*content_index, text_part(delta.clone()));
                }
            },
            StreamEvent::TextEnd {
                content_index,
                text,
            } => {
                self.parts.insert(*content_index, text_part(text.clone()));
            }
            StreamEvent::ThinkingStart { content_index } => {
                self.parts
                    .insert(*content_index, thinking_part(String::new()));
            }
            StreamEvent::ThinkingDelta {
                content_index,
                delta,
            } => match self.parts.get_mut(content_index) {
                Some(AssistantContentPart::Thinking(ThinkingContent::Thinking {
                    thinking,
                    ..
                })) => thinking.push_str(delta),
                _ => {
                    self.parts
                        .insert(*content_index, thinking_part(delta.clone()));
                }
            },
            StreamEvent::ThinkingEnd {
                content_index,
                thinking,
            } => {
                self.parts
                    .insert(*content_index, thinking_part(thinking.clone()));
            }
            StreamEvent::ToolCallEnd {
                content_index,
                id,
                name,
                arguments,
            } => {
                self.parts.insert(
                    *content_index,
                    AssistantContentPart::ToolCall(ToolCall {
                        id: id.as_str().into(),
                        name: name.as_str().into(),
                        arguments: arguments.clone(),
                    }),
                );
            }
            StreamEvent::Done { message, .. } | StreamEvent::Error { message, .. } => {
                *self = Self::new(message.clone());
            }
            StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallDelta { .. } => {}
        }
    }

    #[must_use]
    pub fn has_content(&self) -> bool {
        !self.parts.is_empty()
    }

    #[must_use]
    pub fn finish(self, stop_reason: StopReason, error: impl Into<String>) -> AssistantMessage {
        AssistantMessage {
            content: self.parts.into_values().collect(),
            stop_reason,
            error_message: Some(error.into()),
            timestamp_ms: Timestamp::now(),
            ..self.message
        }
    }
}

fn text_part(text: String) -> AssistantContentPart {
    AssistantContentPart::Text(TextContent { text })
}

fn thinking_part(thinking: String) -> AssistantContentPart {
    AssistantContentPart::Thinking(ThinkingContent::Thinking {
        thinking,
        signature: None,
    })
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

    #[test]
    fn a_partial_rebuilds_streamed_parts_in_content_order() {
        let start = AssistantMessage::builder()
            .model("test".into())
            .provider(Provider::Custom("test".into()))
            .api(Api::AnthropicMessages)
            .usage(Usage {
                input_tokens: TokenCount::new(12),
                ..Usage::default()
            })
            .timestamp_ms(Timestamp::zero())
            .build();
        let mut partial = PartialMessage::new(start.clone());
        assert!(!partial.has_content());
        for event in [
            StreamEvent::ThinkingStart { content_index: 0 },
            StreamEvent::ThinkingDelta {
                content_index: 0,
                delta: "hmm".into(),
            },
            StreamEvent::ToolCallStart { content_index: 3 },
            StreamEvent::ToolCallDelta {
                content_index: 3,
                delta: "{".into(),
            },
            StreamEvent::ToolCallEnd {
                content_index: 2,
                id: "call".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "ls"}),
            },
            StreamEvent::TextStart { content_index: 1 },
            StreamEvent::TextDelta {
                content_index: 1,
                delta: "hel".into(),
            },
            StreamEvent::TextDelta {
                content_index: 1,
                delta: "lo".into(),
            },
        ] {
            partial.apply(&event);
        }
        assert!(partial.has_content());
        let message = partial.finish(StopReason::Aborted, ABORTED_MESSAGE);
        assert_eq!(message.stop_reason, StopReason::Aborted);
        assert_eq!(message.error_message.as_deref(), Some(ABORTED_MESSAGE));
        assert_eq!(message.usage, start.usage);
        assert_eq!(message.text(), "hello");
        assert!(matches!(
            &message.content[..],
            [
                AssistantContentPart::Thinking(ThinkingContent::Thinking { thinking, .. }),
                AssistantContentPart::Text(_),
                AssistantContentPart::ToolCall(call),
            ] if thinking == "hmm" && call.id.as_str() == "call"
        ));
    }
}
