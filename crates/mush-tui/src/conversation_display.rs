use std::collections::HashMap;

use mush_agent::summarise_tool_args;
use mush_ai::types::{
    AssistantContentPart, Message, ToolCallId, ToolOutcome, ToolResultContentPart,
    ToolResultMessage,
};

use crate::app::{
    App, DisplayMessage, DisplayToolCall, MessageRole, ThinkingDisplay, ToolCallStatus,
};
use crate::batch_output::{parse_batch_output, truncate_output, truncate_output_large};

/// extract the summary body from a compacted user message produced by
/// `mush_session::compact::compact_with_summary`. the source text starts
/// with `COMPACTION_SUMMARY_PREFIX` and wraps the summary in `<summary>`
/// tags. returns `None` for non-compaction messages so the caller can fall
/// through to the normal user-message path
fn extract_compaction_summary(text: &str) -> Option<String> {
    let rest = text.strip_prefix(mush_session::COMPACTION_SUMMARY_PREFIX)?;
    let rest = rest.trim_start();
    let inner = rest
        .strip_prefix("<summary>")
        .and_then(|s| s.strip_suffix("</summary>"))
        .unwrap_or(rest);
    Some(inner.trim().to_string())
}

/// decode any image parts on a user message back into raw bytes, in order.
/// used on session reload so the attachment box renders with the same
/// images that were attached when the message was first sent
fn extract_user_images(user: &mush_ai::types::UserMessage) -> Vec<Vec<u8>> {
    use base64::Engine;
    use mush_ai::types::{UserContent, UserContentPart};
    let UserContent::Parts(parts) = &user.content else {
        return Vec::new();
    };
    parts
        .iter()
        .filter_map(|p| match p {
            UserContentPart::Image(img) => base64::engine::general_purpose::STANDARD
                .decode(&img.data)
                .ok(),
            _ => None,
        })
        .collect()
}

pub fn rebuild_display(app: &mut App, conversation: &[Message]) {
    app.clear_display();

    let mut tool_call_positions = HashMap::<ToolCallId, (usize, usize)>::new();
    // (tool_call_id, msg_idx, first_tc_idx, sub_call_count)
    let mut batch_tool_ids: Vec<(ToolCallId, usize, usize, usize)> = Vec::new();
    let mut batch_counter: u32 = 0;
    for message in conversation {
        match message {
            Message::User(user) => {
                let text = user.text();
                if let Some(body) = extract_compaction_summary(&text) {
                    app.push_system_message(format!("━ compacted summary ━\n\n{body}"));
                } else {
                    let images = extract_user_images(user);
                    if images.is_empty() {
                        app.push_user_message(text);
                    } else {
                        app.push_user_message_with_images(text, images);
                    }
                }
            }
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
                    tool_calls,
                    thinking: assistant.thinking(),
                    thinking_expanded: app.thinking_display == ThinkingDisplay::Expanded,
                    usage: Some(assistant.usage),
                    model_id: Some(assistant.model.clone()),
                    ..DisplayMessage::new(
                        MessageRole::Assistant,
                        assistant.text().trim_start_matches('\n'),
                    )
                });
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

/// reconstruct cumulative session stats by walking historical assistant
/// messages and summing their recorded usage. used when loading a
/// session from disk (initial resume or `/resume`) so the user sees
/// "this session has cost $X.XX so far" instead of zero. NOT used for
/// in-session rebuilds (/compact, /branch, /undo) where the live stats
/// already track everything that's been spent and should not be reset.
///
/// also sets `prev_usage` to the most recent historical assistant's
/// usage so the next live call's anomaly detector has a meaningful
/// baseline to diff against.
pub fn accumulate_session_stats(app: &mut App, conversation: &[Message]) {
    for message in conversation {
        if let Message::Assistant(assistant) = message {
            app.stats.update(&assistant.usage, None);
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
    tool_call.output_preview = tool_result_preview(&result.tool_name, &result.content);
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

    let sections = parse_batch_output(&text);

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
                tc.output_preview = Some(truncate_output(&section.content));
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

fn tool_result_preview(
    tool_name: &mush_ai::types::ToolName,
    content: &[ToolResultContentPart],
) -> Option<String> {
    let text = content
        .iter()
        .filter_map(|part| match part {
            ToolResultContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if !text.is_empty() {
        // keep edit diffs at full length so reloaded sessions match the
        // fresh-submit preview budget. other tools still cap at the
        // generous single-tool line count so runaway logs don't blow up
        // the message view
        if tool_name.as_str().eq_ignore_ascii_case("edit") {
            return Some(text);
        }
        return Some(truncate_output_large(&text));
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
    }

    /// rebuild_display is a display-only primitive: it must never
    /// mutate `app.stats`. previously it walked every assistant
    /// message and called `app.stats.update(usage, None)` which (a)
    /// double-counted live usage when run in-session for /compact,
    /// /branch, /undo and (b) polluted `prev_usage` with a
    /// historical assistant's value, causing false ContextDecrease
    /// cache anomalies on the next live call. session-resume paths
    /// that *do* want totals reconstructed call
    /// `accumulate_session_stats` explicitly.
    #[test]
    fn rebuild_display_does_not_mutate_stats() {
        let mut app = app();
        // pretend a previous live call already updated stats
        let live_usage = mush_ai::types::Usage {
            input_tokens: mush_ai::types::TokenCount::new(500),
            output_tokens: mush_ai::types::TokenCount::new(50),
            cache_read_tokens: mush_ai::types::TokenCount::ZERO,
            cache_write_tokens: mush_ai::types::TokenCount::ZERO,
        };
        app.stats.update(&live_usage, None);
        let totals_before = app.stats.total_tokens;
        let prev_before = app.stats.prev_usage().copied();
        let context_before = app.stats.context_tokens;

        let conversation = vec![
            user_message("hello"),
            assistant_with_tool("call_1"),
            tool_result("call_1", ToolOutcome::Success, "fn main() {}"),
        ];
        rebuild_display(&mut app, &conversation);

        assert_eq!(
            app.stats.total_tokens, totals_before,
            "rebuild_display must not change cumulative totals"
        );
        assert_eq!(
            app.stats.prev_usage().copied(),
            prev_before,
            "rebuild_display must not touch prev_usage"
        );
        assert_eq!(
            app.stats.context_tokens, context_before,
            "rebuild_display must not touch context_tokens"
        );
    }

    #[test]
    fn rebuild_display_renders_compaction_summary_as_system_message() {
        let mut app = app();
        let summary = "the user and assistant discussed X, Y, and Z";
        let msg = user_message(&format!(
            "{}\n\n<summary>\n{summary}\n</summary>",
            mush_session::COMPACTION_SUMMARY_PREFIX
        ));
        let conversation = vec![msg, user_message("next user turn after compaction")];
        rebuild_display(&mut app, &conversation);

        assert_eq!(app.messages.len(), 2, "one summary msg + one user msg");
        assert_eq!(app.messages[0].role, MessageRole::System);
        assert!(
            app.messages[0].content.contains("compacted summary"),
            "expected compaction header in display content, got: {:?}",
            app.messages[0].content
        );
        assert!(
            app.messages[0].content.contains(summary),
            "expected summary body in display content, got: {:?}",
            app.messages[0].content
        );
        assert!(
            !app.messages[0].content.contains("<summary>"),
            "raw xml tags leaked into display: {:?}",
            app.messages[0].content
        );
        assert_eq!(app.messages[1].role, MessageRole::User);
        assert_eq!(app.messages[1].content, "next user turn after compaction");
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

    #[test]
    fn rebuild_display_restores_user_images_from_parts() {
        // regression: a user message persisted with UserContent::Parts
        // carrying image data used to rebuild with empty msg.images, so
        // the attachment box would either not show or show blank after a
        // session reload. rebuild_display must decode the base64 image
        // bytes back onto the DisplayMessage so the render path can show
        // them
        use base64::Engine;
        use mush_ai::types::{ImageContent, ImageMimeType, UserContentPart};

        let raw = b"fake-png-bytes".to_vec();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        let parts = vec![
            UserContentPart::Text(TextContent {
                text: "look at this".into(),
            }),
            UserContentPart::Image(ImageContent {
                data: encoded,
                mime_type: ImageMimeType::Png,
            }),
        ];
        let conversation = vec![Message::User(UserMessage {
            content: UserContent::Parts(parts),
            timestamp_ms: Timestamp::now(),
        })];

        let mut app = app();
        rebuild_display(&mut app, &conversation);

        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].role, MessageRole::User);
        assert_eq!(
            app.messages[0].images,
            vec![raw],
            "image bytes should be recovered from the Parts payload"
        );
    }
}
