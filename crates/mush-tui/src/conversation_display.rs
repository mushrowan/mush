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
    // (tool_call_id, msg_idx, first_tc_idx, sub_call_count)
    let mut batch_tool_ids: Vec<(ToolCallId, usize, usize, usize)> = Vec::new();
    let mut batch_counter: u32 = 0;
    for message in conversation {
        match message {
            Message::User(user) => app.push_user_message(user.text()),
            Message::Assistant(assistant) => {
                let msg_idx = app.messages.len();
                let mut tool_calls = Vec::new();

                let has_tools = assistant
                    .content
                    .iter()
                    .any(|p| matches!(p, AssistantContentPart::ToolCall(_)));
                if has_tools {
                    batch_counter += 1;
                }

                for part in &assistant.content {
                    if let AssistantContentPart::ToolCall(tool_call) = part {
                        if tool_call.name.as_str().eq_ignore_ascii_case("batch") {
                            // expand batch into individual sub-call entries
                            let sub_calls = tool_call.arguments["tool_calls"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                            // track the batch tool_call_id so apply_tool_result
                            // can parse and distribute the combined output later
                            let first_tc_idx = tool_calls.len();
                            for sub in &sub_calls {
                                let name = sub["tool"].as_str().unwrap_or("?").to_string();
                                let summary = summarise_tool_args(&name, &sub["parameters"]);
                                tool_calls.push(DisplayToolCall {
                                    name,
                                    summary,
                                    status: ToolCallStatus::Running,
                                    output_preview: None,
                                    image_data: None,
                                    batch: batch_counter,
                                });
                            }
                            // map the batch id to (msg_idx, first_tc_idx) with a
                            // special marker so apply_tool_result knows to expand
                            batch_tool_ids.push((
                                tool_call.id.clone(),
                                msg_idx,
                                first_tc_idx,
                                sub_calls.len(),
                            ));
                        } else {
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
                                batch: batch_counter,
                            });
                            tool_call_positions.insert(tool_call.id.clone(), (msg_idx, tc_idx));
                        }
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
                // check if this is a batch result
                if let Some(pos) = batch_tool_ids
                    .iter()
                    .position(|(id, _, _, _)| *id == result.tool_call_id)
                {
                    let (_, msg_idx, first_tc, count) = batch_tool_ids.remove(pos);
                    apply_batch_result(app, msg_idx, first_tc, count, result);
                } else {
                    apply_tool_result(app, &tool_call_positions, result);
                }
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

/// distribute a batch tool result to individual sub-call display entries
fn apply_batch_result(
    app: &mut App,
    msg_idx: usize,
    first_tc: usize,
    count: usize,
    result: &ToolResultMessage,
) {
    let text = result
        .content
        .iter()
        .filter_map(|p| match p {
            ToolResultContentPart::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    let sections = crate::app::parse_batch_output(&text);

    let Some(message) = app.messages.get_mut(msg_idx) else {
        return;
    };

    for (i, section) in sections.iter().enumerate() {
        let tc_idx = first_tc + i;
        if tc_idx >= first_tc + count {
            break;
        }
        if let Some(tc) = message.tool_calls.get_mut(tc_idx) {
            tc.status = if section.is_error {
                ToolCallStatus::Error
            } else {
                ToolCallStatus::Done
            };
            if !section.content.is_empty() {
                tc.output_preview = Some(crate::app::truncate_output(&section.content));
            }
        }
    }
    // mark any remaining unmatched sub-calls as done
    for i in sections.len()..count {
        if let Some(tc) = message.tool_calls.get_mut(first_tc + i) {
            tc.status = ToolCallStatus::Done;
        }
    }
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
