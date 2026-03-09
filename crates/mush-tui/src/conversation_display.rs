use std::collections::HashMap;

use mush_agent::summarise_tool_args;
use mush_ai::types::{
    AssistantContentPart, Message, ToolCallId, ToolOutcome, ToolResultContentPart,
    ToolResultMessage,
};

use crate::app::{
    App, DisplayMessage, DisplayToolCall, MessageRole, ThinkingDisplay, ToolCallStatus,
};

pub fn rebuild_display(app: &mut App, conversation: &[Message]) {
    app.clear_messages();

    let mut tool_call_positions = HashMap::<ToolCallId, (usize, usize)>::new();
    for message in conversation {
        match message {
            Message::User(user) => app.push_user_message(user.text()),
            Message::Assistant(assistant) => {
                let msg_idx = app.messages.len();
                let mut tool_calls = Vec::new();

                for part in &assistant.content {
                    if let AssistantContentPart::ToolCall(tool_call) = part {
                        let tc_idx = tool_calls.len();
                        tool_calls.push(DisplayToolCall {
                            name: tool_call.name.to_string(),
                            summary: summarise_tool_args(
                                tool_call.name.as_str(),
                                &tool_call.arguments,
                            ),
                            status: ToolCallStatus::Running,
                            output_preview: None,
                            image_data: None,
                        });
                        tool_call_positions.insert(tool_call.id.clone(), (msg_idx, tc_idx));
                    }
                }

                app.messages.push(DisplayMessage {
                    role: MessageRole::Assistant,
                    content: assistant.text().trim_start_matches('\n').to_string(),
                    tool_calls,
                    thinking: assistant.thinking(),
                    thinking_expanded: app.thinking_display == ThinkingDisplay::Expanded,
                    usage: Some(assistant.usage),
                    cost: None,
                    model_id: Some(assistant.model.clone()),
                    queued: false,
                });
                app.stats.update(&assistant.usage, None);
            }
            Message::ToolResult(result) => {
                apply_tool_result(app, &tool_call_positions, result);
            }
        }
    }
}

fn apply_tool_result(
    app: &mut App,
    tool_call_positions: &HashMap<ToolCallId, (usize, usize)>,
    result: &ToolResultMessage,
) {
    let Some(&(msg_idx, tc_idx)) = tool_call_positions.get(&result.tool_call_id) else {
        tracing::trace!(
            tool_call_id = %result.tool_call_id,
            tool_name = %result.tool_name,
            "skipping unmatched tool result during display rebuild"
        );
        return;
    };

    let Some(message) = app.messages.get_mut(msg_idx) else {
        return;
    };
    let Some(tool_call) = message.tool_calls.get_mut(tc_idx) else {
        return;
    };

    tool_call.status = display_tool_status(result.outcome);
    tool_call.output_preview = tool_result_preview(&result.content);
}

fn display_tool_status(outcome: ToolOutcome) -> ToolCallStatus {
    if outcome.is_error() {
        ToolCallStatus::Error
    } else {
        ToolCallStatus::Done
    }
}

fn tool_result_preview(content: &[ToolResultContentPart]) -> Option<String> {
    let text = content
        .iter()
        .filter_map(|part| match part {
            ToolResultContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if !text.is_empty() {
        return Some(crate::app::truncate_output(&text));
    }

    content
        .iter()
        .any(|part| matches!(part, ToolResultContentPart::Image(_)))
        .then(|| "[image]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::models;
    use mush_ai::types::{
        Api, AssistantMessage, Provider, StopReason, TextContent, Timestamp, TokenCount, ToolCall,
        ToolName, Usage, UserContent, UserMessage,
    };

    fn test_model() -> mush_ai::types::Model {
        models::all_models_with_user()
            .into_iter()
            .next()
            .expect("expected at least one model")
    }

    fn app() -> App {
        App::new(test_model().id, TokenCount::new(200_000))
    }

    fn user_message(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp_ms: Timestamp::zero(),
        })
    }

    fn assistant_with_tool(tool_call_id: &str) -> Message {
        let model = test_model();
        Message::Assistant(AssistantMessage {
            content: vec![
                AssistantContentPart::Text(TextContent {
                    text: "i will read that".into(),
                }),
                AssistantContentPart::ToolCall(ToolCall {
                    id: ToolCallId::from(tool_call_id),
                    name: ToolName::from("read"),
                    arguments: serde_json::json!({ "path": "src/main.rs" }),
                }),
            ],
            model: model.id,
            provider: Provider::Anthropic,
            api: Api::AnthropicMessages,
            usage: Usage {
                input_tokens: TokenCount::new(10),
                output_tokens: TokenCount::new(5),
                cache_read_tokens: TokenCount::ZERO,
                cache_write_tokens: TokenCount::ZERO,
            },
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp_ms: Timestamp::zero(),
        })
    }

    fn tool_result(tool_call_id: &str, outcome: ToolOutcome, text: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: ToolCallId::from(tool_call_id),
            tool_name: ToolName::from("read"),
            content: vec![ToolResultContentPart::Text(TextContent {
                text: text.into(),
            })],
            outcome,
            timestamp_ms: Timestamp::zero(),
        })
    }

    #[test]
    fn rebuild_display_maps_messages_one_way() {
        let mut app = app();
        let conversation = vec![
            user_message("hello"),
            assistant_with_tool("call_1"),
            tool_result("call_1", ToolOutcome::Success, "fn main() {}"),
        ];

        rebuild_display(&mut app, &conversation);

        assert_eq!(app.messages.len(), 2);
        assert_eq!(app.messages[0].content, "hello");
        assert_eq!(app.messages[1].content, "i will read that");
        assert_eq!(app.messages[1].tool_calls.len(), 1);
        assert_eq!(app.messages[1].tool_calls[0].name, "read");
        assert_eq!(app.messages[1].tool_calls[0].status, ToolCallStatus::Done);
        assert_eq!(
            app.messages[1].tool_calls[0].output_preview.as_deref(),
            Some("fn main() {}")
        );
        assert_eq!(app.stats.total_tokens, TokenCount::new(15));
    }

    #[test]
    fn rebuild_display_marks_failed_tool_results() {
        let mut app = app();
        let conversation = vec![
            assistant_with_tool("call_2"),
            tool_result("call_2", ToolOutcome::Error, "permission denied"),
        ];

        rebuild_display(&mut app, &conversation);

        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].tool_calls[0].status, ToolCallStatus::Error);
        assert_eq!(
            app.messages[0].tool_calls[0].output_preview.as_deref(),
            Some("permission denied")
        );
    }
}
