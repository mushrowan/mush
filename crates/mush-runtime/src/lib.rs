pub mod commands;
pub mod compaction;
pub mod connection;
pub mod family;
pub mod goal;
pub mod inbox;
pub mod kernel_channels;
pub mod notes;
pub mod schedule;
pub mod session;
pub mod side_question;
pub mod snapshot;
#[doc(hidden)]
pub mod testing;
pub mod turn;
pub mod worker;

use mush_agent::AgentEvent;
use mush_ai::types::{Message, Timestamp, ToolResultMessage};
use mush_session::ConversationState;

pub use commands::{Replaced, SessionCommand, SessionEvent};
pub use connection::{
    AgentConnection, ClientCommand, ClientEvent, ConnectionError, ConnectionState,
    InProcessConnection, PRIMARY_PANE, SystemPromptBuilder,
};
pub use family::{
    ChildFactory, ChildHandle, ChildId, ChildParts, ChildSpec, ChildStatus, ChildSummary,
    CreateSessionRequest, CreatedSession, DeliveryMode, Family, Identity, Receipt, RlmHandler,
    SessionSpawner,
};
pub use goal::{Goal, GoalCommand, GoalStatus, GoalStore};
pub use kernel_channels::{KernelSignals, KernelSinks, kernel_channels};
pub use notes::Notes;
pub use schedule::{Delivery, Job, JobSource, JobStatus, ScheduleCommand, ScheduleKind, Scheduler};
pub use session::{
    ConfirmReply, ConfirmRequest, PendingConfirmation, PendingImage, PendingPrompt, SessionRuntime,
    StreamGeneration, StreamMeta, TurnEvent, TurnEvents, TurnStream,
};
pub use snapshot::{PaneSnapshot, SessionSaver, SessionSnapshot};
pub use turn::{CompactionTarget, TurnConfig, TurnDeps, TurnRequest};
pub use worker::{HostedWorker, WorkerConfig, host_worker};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Applied {
    Nothing,
    Appended,
    Compacted { after_count: usize },
}

pub fn apply_agent_event(conversation: &mut ConversationState, event: &AgentEvent) -> Applied {
    match event {
        AgentEvent::MessageEnd { message } => {
            conversation.append_message(Message::Assistant(message.clone()));
            Applied::Appended
        }
        AgentEvent::ToolExecEnd {
            tool_call_id,
            tool_name,
            result,
        } => {
            conversation.append_message(Message::ToolResult(ToolResultMessage {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                content: result.content.clone(),
                outcome: result.outcome,
                timestamp_ms: Timestamp::now(),
                details: result.details.clone(),
            }));
            Applied::Appended
        }
        AgentEvent::SteeringInjected { messages } | AgentEvent::FollowUpInjected { messages } => {
            for message in messages {
                conversation.append_message(message.clone());
            }
            Applied::Appended
        }
        AgentEvent::ContextCompacted { compaction, .. } => {
            conversation.apply_compaction(
                compaction.summary.clone(),
                compaction.summarised_count,
                compaction.messages.clone(),
            );
            Applied::Compacted {
                after_count: conversation.context_len(),
            }
        }
        _ => Applied::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{
        Api, AssistantMessage, Provider, TextContent, ToolCallId, ToolName, ToolOutcome,
        ToolResultContentPart, UserContent, UserMessage,
    };

    fn user(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    #[test]
    fn an_assistant_message_end_lands_in_the_tree() {
        let mut conversation = ConversationState::new();
        let message = AssistantMessage::builder()
            .content(vec![mush_ai::types::AssistantContentPart::Text(
                TextContent { text: "hi".into() },
            )])
            .model("m".into())
            .provider(Provider::Anthropic)
            .api(Api::AnthropicMessages)
            .timestamp_ms(Timestamp::zero())
            .build();

        let applied = apply_agent_event(&mut conversation, &AgentEvent::MessageEnd { message });

        assert_eq!(applied, Applied::Appended);
        assert!(matches!(
            conversation.context().as_slice(),
            [Message::Assistant(_)]
        ));
    }

    #[test]
    fn a_tool_result_is_recorded_with_its_call_id() {
        let mut conversation = ConversationState::new();
        let result = mush_agent::tool::ToolResult {
            content: vec![ToolResultContentPart::Text(TextContent {
                text: "ok".into(),
            })],
            outcome: ToolOutcome::Success,
            details: None,
        };

        apply_agent_event(
            &mut conversation,
            &AgentEvent::ToolExecEnd {
                tool_call_id: ToolCallId::from("tc_1"),
                tool_name: ToolName::from("bash"),
                result,
            },
        );

        match conversation.context().as_slice() {
            [Message::ToolResult(tool)] => {
                assert_eq!(tool.tool_call_id.as_str(), "tc_1");
                assert_eq!(tool.outcome, ToolOutcome::Success);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn injected_steering_and_follow_ups_are_persisted() {
        let mut conversation = ConversationState::new();

        apply_agent_event(
            &mut conversation,
            &AgentEvent::SteeringInjected {
                messages: vec![user("steer")],
            },
        );
        apply_agent_event(
            &mut conversation,
            &AgentEvent::FollowUpInjected {
                messages: vec![user("later")],
            },
        );

        let texts: Vec<String> = conversation
            .context()
            .iter()
            .map(|m| match m {
                Message::User(u) => u.text(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(texts, ["steer", "later"]);
    }

    #[test]
    fn stream_deltas_change_nothing() {
        let mut conversation = ConversationState::new();

        let applied = apply_agent_event(&mut conversation, &AgentEvent::AgentStart);

        assert_eq!(applied, Applied::Nothing);
        assert!(conversation.is_empty());
    }
}
