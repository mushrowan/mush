use mush_ai::types::Message;
use mush_runtime::{ClientCommand, ClientEvent, ConnectionError, ConnectionState};
use mush_session::tree::TreeSummaryRow;
use serde::{Deserialize, Serialize};

pub type RequestId = u64;
pub type ClientId = String;
pub type CommandId = String;

pub const ENVELOPE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub version: u32,
    pub client: ClientId,
    pub command: CommandId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<CommandId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub generation: String,
    pub sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Attach {
        id: RequestId,
        client: ClientId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Cursor>,
    },
    Command {
        id: RequestId,
        command: ClientCommand,
        envelope: Envelope,
    },
    Transcript {
        id: RequestId,
    },
    History {
        id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    TreeSummary {
        id: RequestId,
    },
    NavigationImpact {
        id: RequestId,
        entry_id: mush_session::tree::EntryId,
    },
    SystemPrompt {
        id: RequestId,
    },
}

impl Request {
    #[must_use]
    pub fn id(&self) -> RequestId {
        match self {
            Self::Attach { id, .. }
            | Self::Command { id, .. }
            | Self::Transcript { id }
            | Self::History { id, .. }
            | Self::TreeSummary { id }
            | Self::NavigationImpact { id, .. }
            | Self::SystemPrompt { id } => *id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecentWindow {
    pub omitted: usize,
    pub usage: mush_ai::types::Usage,
    pub cost: mush_ai::types::Dollars,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Attached {
        cursor: Cursor,
        snapshot: bool,
    },
    Events {
        events: Vec<ClientEvent>,
    },
    Transcript {
        messages: Vec<Message>,
    },
    History {
        messages: Vec<Message>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recent: Option<RecentWindow>,
    },
    TreeSummary {
        rows: Vec<TreeSummaryRow>,
    },
    NavigationImpact {
        abandons: usize,
    },
    SystemPrompt {
        prompt: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Hello {
        state: ConnectionState,
        #[serde(default)]
        build: String,
        cursor: Cursor,
    },
    Event {
        event: ClientEvent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<ConnectionState>,
        #[serde(default)]
        sequence: u64,
    },
    Response {
        id: RequestId,
        result: Result<Reply, ConnectionError>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<ConnectionState>,
    },
    Closed,
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("malformed frame: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn decode<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, CodecError> {
    Ok(serde_json::from_str(line)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_agent::AgentEvent;
    use mush_ai::stream::StreamEvent;
    use mush_ai::types::ThinkingLevel;
    use mush_runtime::{Applied, StreamGeneration};

    use crate::test_support::state;

    #[test]
    fn a_command_request_survives_the_wire() {
        let request = Request::Command {
            id: 7,
            envelope: Envelope {
                version: ENVELOPE_VERSION,
                client: "c".into(),
                command: "c-1".into(),
                ack: None,
            },
            command: ClientCommand::Prompt {
                text: "hello".into(),
                images: Vec::new(),
            },
        };
        let bytes = encode(&request).unwrap();
        let line = std::str::from_utf8(&bytes).unwrap();
        assert!(line.ends_with('\n'));
        assert!(!line.trim_end().contains('\n'));
        let back: Request = decode(line.trim_end()).unwrap();
        assert_eq!(back.id(), 7);
        assert!(matches!(
            back,
            Request::Command {
                command: ClientCommand::Prompt { text, .. },
                ..
            } if text == "hello"
        ));
    }

    #[test]
    fn a_streaming_agent_event_survives_the_wire() {
        let frame = Frame::Event {
            sequence: 9,
            event: ClientEvent::Agent {
                generation: StreamGeneration(3),
                event: AgentEvent::StreamEvent {
                    event: StreamEvent::TextDelta {
                        content_index: 0,
                        delta: "multi\nline".into(),
                    },
                },
                applied: Applied::Nothing,
            },
            state: None,
        };
        let bytes = encode(&frame).unwrap();
        let line = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(line.matches('\n').count(), 1);
        let back: Frame = decode(line.trim_end()).unwrap();
        assert!(matches!(
            back,
            Frame::Event {
                event: ClientEvent::Agent {
                    generation: StreamGeneration(3),
                    event: AgentEvent::StreamEvent {
                        event: StreamEvent::TextDelta { delta, .. }
                    },
                    applied: Applied::Nothing,
                },
                state: None,
                sequence: 9,
            } if delta == "multi\nline"
        ));
    }

    #[test]
    fn history_requests_and_replies_from_older_builds_still_decode() {
        let request: Request = decode(r#"{"type":"history","id":4}"#).unwrap();
        assert!(matches!(request, Request::History { id: 4, limit: None }));

        let reply: Reply = decode(r#"{"type":"history","messages":[]}"#).unwrap();
        assert!(matches!(reply, Reply::History { recent: None, .. }));

        let request = Request::History {
            id: 5,
            limit: Some(2000),
        };
        let bytes = encode(&request).unwrap();
        let back: Request = decode(std::str::from_utf8(&bytes).unwrap().trim_end()).unwrap();
        assert!(matches!(
            back,
            Request::History {
                id: 5,
                limit: Some(2000)
            }
        ));
    }

    #[test]
    fn a_failed_response_carries_the_error_and_state() {
        let frame = Frame::Response {
            id: 1,
            result: Err(ConnectionError::TooShortToCompact),
            state: Some(state()),
        };
        let back: Frame = decode(std::str::from_utf8(&encode(&frame).unwrap()).unwrap()).unwrap();
        assert!(matches!(
            back,
            Frame::Response {
                id: 1,
                result: Err(ConnectionError::TooShortToCompact),
                state: Some(_),
            }
        ));
    }

    #[test]
    fn every_command_shape_survives_the_wire() {
        use mush_runtime::SessionCommand;
        let commands = [
            ClientCommand::SetModel("m".into()),
            ClientCommand::SetThinking(ThinkingLevel::High),
            ClientCommand::SetRlmMaxDepth(3),
            ClientCommand::SetSystemPrompt(None),
            ClientCommand::Session(SessionCommand::New),
            ClientCommand::Session(SessionCommand::Fork {
                before_user_message: 2,
            }),
            ClientCommand::Abort,
            ClientCommand::AbortAndSendQueued,
        ];
        for command in commands {
            let line = encode(&command).unwrap();
            let back: ClientCommand = decode(std::str::from_utf8(&line).unwrap()).unwrap();
            assert_eq!(format!("{back:?}"), format!("{command:?}"));
        }
    }

    #[test]
    fn every_event_shape_survives_the_wire() {
        use mush_ai::types::ToolCallId;
        use mush_runtime::{Replaced, SessionEvent};
        let events = [
            ClientEvent::ToolsCancelled(vec![ToolCallId::from("call_1")]),
            ClientEvent::Session(SessionEvent::Title(Some("t".into()))),
            ClientEvent::Session(SessionEvent::TranscriptReplaced {
                reason: Replaced::New,
                messages: Vec::new(),
                editor_text: None,
            }),
            ClientEvent::TurnEnded { cancelled: true },
            ClientEvent::Compacting,
        ];
        for event in events {
            let line = encode(&event).unwrap();
            let back: ClientEvent = decode(std::str::from_utf8(&line).unwrap()).unwrap();
            assert_eq!(format!("{back:?}"), format!("{event:?}"));
        }
    }

    #[test]
    fn a_garbled_line_is_a_codec_error() {
        let error = decode::<Request>("{not json").unwrap_err();
        assert!(matches!(error, CodecError::Json(_)));
    }
}
