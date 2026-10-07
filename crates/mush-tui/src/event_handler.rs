use mush_agent::{AgentEvent, summarise_tool_args};
use mush_ai::models;
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;

use crate::app::App;

pub struct EventCtx<'a> {
    pub app: &'a mut App,
    pub image_protos: &'a mut std::collections::HashMap<
        (usize, usize),
        ratatui_image::protocol::StatefulProtocol,
    >,
}

pub fn handle_agent_event(
    ctx: &mut EventCtx<'_>,
    event: &AgentEvent,
    applied: mush_runtime::Applied,
    model: &Model,
    debug_cache: bool,
    image_picker: &Option<ratatui_image::picker::Picker>,
) {
    let EventCtx { app, image_protos } = ctx;
    match event {
        AgentEvent::StreamEvent { event } => match event {
            StreamEvent::TextDelta { delta, .. } => app.push_text_delta(delta),
            StreamEvent::ThinkingDelta { delta, .. } => app.push_thinking_delta(delta),
            StreamEvent::ToolCallDelta { delta, .. } => app.push_tool_args_delta(delta),
            _ => {}
        },
        AgentEvent::MessageEnd { message } => {
            app.announce_tool_calls(message);
            apply_message_end(app, message, model, debug_cache);
        }
        AgentEvent::ToolExecStart {
            tool_call_id,
            tool_name,
            args,
        } => show_tool_start(app, tool_call_id, tool_name.as_str(), args),
        AgentEvent::ToolExecEnd {
            tool_call_id,
            tool_name,
            result,
        } => {
            if !app.turn_shows_tool_call(tool_call_id) {
                let args = app
                    .announced_tool_call(tool_call_id)
                    .map(|tc| tc.arguments.clone())
                    .unwrap_or_default();
                show_tool_start(app, tool_call_id, tool_name.as_str(), &args);
            }
            let output_text = result.content.iter().find_map(|p| match p {
                ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            });

            if tool_name.as_str().eq_ignore_ascii_case("batch") {
                app.end_batch_tool(tool_call_id, result.outcome, output_text);
            } else {
                let image_data = result.content.iter().find_map(|p| match p {
                    ToolResultContentPart::Image(img) => {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD
                            .decode(&img.data)
                            .ok()
                    }
                    _ => None,
                });
                if let Some(ref data) = image_data
                    && let Some(picker) = image_picker
                    && let Ok(dyn_image) = image::load_from_memory(data)
                {
                    let msg_idx = app.messages.len().saturating_sub(1);
                    let tc_idx = app.messages.last().map(|m| m.tool_calls.len()).unwrap_or(0);
                    let proto = picker.new_resize_protocol(dyn_image);
                    image_protos.insert((msg_idx, tc_idx), proto);
                }
                app.end_tool(
                    tool_call_id,
                    tool_name.as_str(),
                    result.outcome,
                    output_text,
                    image_data,
                );
                if let Some(details) = &result.details {
                    app.set_tool_details(tool_call_id, details);
                }
            }
        }
        AgentEvent::PartialDiscarded { error } => {
            app.start_streaming();
            app.status = Some(format!("provider error, retrying: {error}").into());
        }
        AgentEvent::TurnStart { .. } => {
            app.active_tools.clear();
            if !app.stream.active {
                app.start_streaming();
            }
        }
        AgentEvent::SteeringInjected { messages } | AgentEvent::FollowUpInjected { messages } => {
            let typed = messages
                .iter()
                .filter(|m| match m {
                    Message::User(user) => {
                        mush_tools::kernel_jobs::BashDone::parse(&user.text()).is_none()
                    }
                    _ => true,
                })
                .count();
            for mut msg in app.messages.iter_mut().filter(|m| m.queued).take(typed) {
                msg.queued = false;
            }
        }
        AgentEvent::ContextCompacted {
            before_count,
            compaction,
        } => {
            let tokens_after = mush_session::compact::estimate_tokens(&compaction.messages);
            app.stats
                .reset_live_state(TokenCount::new(tokens_after as u64));
            let after_count = match applied {
                mush_runtime::Applied::Compacted { after_count } => after_count,
                _ => compaction.messages.len(),
            };
            app.push_system_message(format!(
                "━ compacted: {before_count} → {after_count} messages ━"
            ));
            app.status = Some(format!("compacted: {before_count} → {after_count} messages").into());
        }
        AgentEvent::MaxTurnsReached { max_turns } => {
            app.stream.active = false;
            app.push_system_message(format!("hit max turns limit ({max_turns})"));
        }
        AgentEvent::Error { error } => {
            app.stream.active = false;
            tracing::error!(%error, "agent error");
            app.push_system_message(format!("error: {error}"));
        }
        AgentEvent::AgentEnd => {
            app.stream.active = false;
            app.active_tools.clear();
        }
        _ => {}
    }
}

fn show_tool_start(
    app: &mut App,
    tool_call_id: &ToolCallId,
    tool_name: &str,
    args: &serde_json::Value,
) {
    if tool_name.eq_ignore_ascii_case("batch") {
        let sub_calls: Vec<(String, String)> = args["tool_calls"]
            .as_array()
            .map(|calls| {
                calls
                    .iter()
                    .map(|c| {
                        let name = c["tool"].as_str().unwrap_or("?").to_string();
                        let sub_summary = summarise_tool_args(&name, &c["parameters"]);
                        (name, sub_summary)
                    })
                    .collect()
            })
            .unwrap_or_default();
        app.start_batch_tool(tool_call_id, &sub_calls);
    } else {
        let summary = summarise_tool_args(tool_name, args);
        app.start_tool(tool_call_id, tool_name, &summary);
    }
}

fn apply_message_end(app: &mut App, message: &AssistantMessage, model: &Model, debug_cache: bool) {
    if message.stop_reason == StopReason::Error || message.error_message.is_some() {
        let err = message
            .error_message
            .as_deref()
            .unwrap_or("stream ended with an error");
        tracing::error!(
            error = err,
            model = %message.model,
            provider = %message.provider,
            stop_reason = ?message.stop_reason,
            "assistant turn ended in an error"
        );
        app.push_system_message(format!("⚠ API error: {err}"));
        app.finish_streaming(None, None);
        return;
    }

    let cost = models::calculate_cost(model, &message.usage);
    app.finish_streaming(Some(message.usage), Some(cost.total()));
    if message.usage.cache_read_tokens > TokenCount::ZERO
        || message.usage.cache_write_tokens > TokenCount::ZERO
    {
        app.cache.refresh();
    }
    if debug_cache && message.usage.cache_read_tokens > TokenCount::ZERO {
        app.push_system_message(format!(
            "cache read detected: {} tokens",
            message.usage.cache_read_tokens
        ));
    }
}

pub use mush_runtime::turn::resolve_auth_for_model;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ToolCallStatus;

    #[test]
    fn agent_error_becomes_system_message() {
        use crate::app::{App, MessageRole};
        use mush_ai::types::TokenCount;

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            image_protos: &mut image_protos,
        };

        let event = AgentEvent::Error {
            error: "anthropic returned 400 Bad Request".into(),
        };
        handle_agent_event(
            &mut ctx,
            &event,
            mush_runtime::Applied::Nothing,
            &model,
            false,
            &None,
        );

        assert!(
            ctx.app.status.is_none(),
            "error should not be in status bar"
        );
        let last = ctx.app.messages.last().expect("should have a message");
        assert_eq!(last.role, MessageRole::System);
        assert!(last.content.contains("400 Bad Request"));
    }

    #[test]
    fn a_discarded_partial_clears_the_stream_and_says_why() {
        use mush_ai::types::TokenCount;

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.start_streaming();
        app.push_thinking_delta("pondering");
        app.push_text_delta("half an ans");
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            image_protos: &mut image_protos,
        };

        let event = AgentEvent::PartialDiscarded {
            error: "connection reset by peer".into(),
        };
        handle_agent_event(
            &mut ctx,
            &event,
            mush_runtime::Applied::Nothing,
            &model,
            false,
            &None,
        );

        assert!(ctx.app.stream.active);
        assert_eq!(ctx.app.stream.text, "");
        assert_eq!(ctx.app.stream.thinking, "");
        assert!(ctx.app.messages.is_empty());
        assert!(
            ctx.app
                .status
                .as_ref()
                .is_some_and(|status| status.text.contains("connection reset by peer"))
        );
    }

    #[test]
    fn max_turns_becomes_system_message() {
        use crate::app::{App, MessageRole};
        use mush_ai::types::TokenCount;

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            image_protos: &mut image_protos,
        };

        let event = AgentEvent::MaxTurnsReached { max_turns: 10 };
        handle_agent_event(
            &mut ctx,
            &event,
            mush_runtime::Applied::Nothing,
            &model,
            false,
            &None,
        );

        assert!(ctx.app.status.is_none(), "should not be in status bar");
        let last = ctx.app.messages.last().expect("should have a message");
        assert_eq!(last.role, MessageRole::System);
        assert!(last.content.contains("max turns"));
    }

    #[test]
    fn errored_message_end_surfaces_error_and_skips_stats() {
        use crate::app::{App, MessageRole};
        use mush_ai::types::{TokenCount, Usage};

        let mut app = App::new("test".into(), TokenCount::new(200_000));

        let prev_usage = Usage {
            input_tokens: TokenCount::new(32_000),
            output_tokens: TokenCount::new(100),
            cache_read_tokens: TokenCount::ZERO,
            cache_write_tokens: TokenCount::ZERO,
        };
        app.stats.update(&prev_usage, None);

        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();

        let errored = AssistantMessage::builder()
            .model(model.id.clone())
            .provider(model.provider.clone())
            .api(model.api)
            .stop_reason(StopReason::Error)
            .error_message("Overloaded")
            .timestamp_ms(Timestamp::now())
            .build();
        let mut ctx = EventCtx {
            app: &mut app,
            image_protos: &mut image_protos,
        };
        handle_agent_event(
            &mut ctx,
            &AgentEvent::MessageEnd { message: errored },
            mush_runtime::Applied::Nothing,
            &model,
            false,
            &None,
        );

        let has_context_decrease = ctx
            .app
            .messages
            .iter()
            .any(|m| m.role == MessageRole::System && m.content.contains("context decreased"));
        assert!(
            !has_context_decrease,
            "zero-usage error message should not trigger context-decrease anomaly"
        );

        let has_error_msg = ctx
            .app
            .messages
            .iter()
            .any(|m| m.role == MessageRole::System && m.content.contains("Overloaded"));
        assert!(
            has_error_msg,
            "the actual error (Overloaded) should be shown to the user"
        );
    }

    #[test]
    fn text_before_and_between_tool_calls_all_reaches_the_display() {
        use crate::app::App;
        use mush_ai::types::TokenCount;

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            image_protos: &mut image_protos,
        };

        let tool_call = |id: &str| ToolCall {
            id: ToolCallId::from(id.to_string()),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "ls"}),
        };
        let assistant = |text: &str, id: &str| {
            AssistantMessage::builder()
                .content(vec![
                    AssistantContentPart::Text(TextContent { text: text.into() }),
                    AssistantContentPart::ToolCall(tool_call(id)),
                ])
                .model(model.id.clone())
                .provider(model.provider.clone())
                .api(model.api)
                .stop_reason(StopReason::ToolUse)
                .timestamp_ms(Timestamp::now())
                .build()
        };
        let text_delta = |delta: &str| AgentEvent::StreamEvent {
            event: StreamEvent::TextDelta {
                content_index: 0,
                delta: delta.into(),
            },
        };
        let tool_start = |id: &str| AgentEvent::ToolExecStart {
            tool_call_id: ToolCallId::from(id.to_string()),
            tool_name: "bash".into(),
            args: serde_json::json!({"command": "ls"}),
        };

        let events = [
            AgentEvent::TurnStart { turn_index: 0 },
            text_delta("yes, because a, b, c"),
            AgentEvent::MessageEnd {
                message: assistant("yes, because a, b, c", "tc1"),
            },
            tool_start("tc1"),
            AgentEvent::ToolExecEnd {
                tool_call_id: ToolCallId::from("tc1".to_string()),
                tool_name: "bash".into(),
                result: mush_agent::tool::ToolResult::text("file.rs"),
            },
            AgentEvent::TurnStart { turn_index: 1 },
            text_delta("now the second step"),
            AgentEvent::MessageEnd {
                message: assistant("now the second step", "tc2"),
            },
            tool_start("tc2"),
        ];
        for event in &events {
            handle_agent_event(
                &mut ctx,
                event,
                mush_runtime::Applied::Nothing,
                &model,
                false,
                &None,
            );
        }

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).unwrap();
        let mut screen = crate::screen::Screen::new();
        terminal
            .draw(|frame| screen.render(ctx.app, frame.area(), frame.buffer_mut()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();

        let position = |needle: &str| {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} missing from {rows:#?}"))
        };
        assert!(position("yes, because a, b, c") < position("file.rs"));
        assert!(position("file.rs") < position("now the second step"));
        assert!(position("now the second step") < position("bash · running"));
    }

    fn bash_call(id: &str, command: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::from(id.to_string()),
            name: "bash".into(),
            arguments: serde_json::json!({"command": command}),
        }
    }

    fn tool_turn(model: &Model, text: &str, calls: Vec<ToolCall>) -> AssistantMessage {
        let mut content = vec![AssistantContentPart::Text(TextContent {
            text: text.into(),
        })];
        content.extend(calls.into_iter().map(AssistantContentPart::ToolCall));
        AssistantMessage::builder()
            .content(content)
            .model(model.id.clone())
            .provider(model.provider.clone())
            .api(model.api)
            .stop_reason(StopReason::ToolUse)
            .timestamp_ms(Timestamp::now())
            .build()
    }

    fn tool_start(call: &ToolCall) -> AgentEvent {
        AgentEvent::ToolExecStart {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: call.arguments.clone(),
        }
    }

    fn tool_end(call: &ToolCall, result: mush_agent::tool::ToolResult) -> AgentEvent {
        AgentEvent::ToolExecEnd {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            result,
        }
    }

    fn shown_tool_calls(app: &crate::app::App) -> Vec<(String, ToolCallStatus, Option<String>)> {
        app.messages
            .last()
            .unwrap()
            .tool_calls
            .iter()
            .map(|tc| (tc.summary.clone(), tc.status, tc.output.clone()))
            .collect()
    }

    #[test]
    fn two_same_name_tools_in_one_turn_each_get_their_own_result() {
        use crate::app::App;
        use mush_agent::tool::ToolResult;
        use mush_ai::types::TokenCount;

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            image_protos: &mut image_protos,
        };
        let a = bash_call("a", "ls");
        let b = bash_call("b", "pwd");
        let events = [
            AgentEvent::TurnStart { turn_index: 0 },
            AgentEvent::MessageEnd {
                message: tool_turn(&model, "checking both", vec![a.clone(), b.clone()]),
            },
            tool_start(&a),
            tool_start(&b),
            tool_end(&a, ToolResult::text("file.rs")),
            tool_end(&b, ToolResult::text("/home")),
        ];
        for event in &events {
            handle_agent_event(
                &mut ctx,
                event,
                mush_runtime::Applied::Nothing,
                &model,
                false,
                &None,
            );
        }

        assert_eq!(
            shown_tool_calls(ctx.app),
            vec![
                ("ls".into(), ToolCallStatus::Done, Some("file.rs".into())),
                ("pwd".into(), ToolCallStatus::Done, Some("/home".into())),
            ]
        );
    }

    #[test]
    fn batch_sub_calls_next_to_a_parallel_tool_each_get_their_own_result() {
        use crate::app::App;
        use mush_agent::tool::ToolResult;
        use mush_ai::types::TokenCount;

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            image_protos: &mut image_protos,
        };
        let bash = bash_call("bash1", "ls");
        let batch = ToolCall {
            id: ToolCallId::from("batch1".to_string()),
            name: "batch".into(),
            arguments: serde_json::json!({"tool_calls": [
                {"tool": "read", "parameters": {"path": "a.rs"}},
                {"tool": "read", "parameters": {"path": "b.rs"}},
            ]}),
        };
        let events = [
            AgentEvent::TurnStart { turn_index: 0 },
            AgentEvent::MessageEnd {
                message: tool_turn(&model, "reading", vec![bash.clone(), batch.clone()]),
            },
            tool_start(&bash),
            tool_start(&batch),
            tool_end(
                &batch,
                ToolResult::text(
                    "--- [0] read [ok] ---\nA\n\n--- [1] read [error] ---\nno b\n\nbatch: 1/2 succeeded, 1 failed",
                ),
            ),
            tool_end(&bash, ToolResult::text("file.rs")),
        ];
        for event in &events {
            handle_agent_event(
                &mut ctx,
                event,
                mush_runtime::Applied::Nothing,
                &model,
                false,
                &None,
            );
        }

        assert_eq!(
            shown_tool_calls(ctx.app),
            vec![
                ("ls".into(), ToolCallStatus::Done, Some("file.rs".into())),
                ("a.rs".into(), ToolCallStatus::Done, Some("A".into())),
                ("b.rs".into(), ToolCallStatus::Error, Some("no b".into())),
            ]
        );
    }

    #[test]
    fn a_denied_tool_call_still_shows_in_the_turn() {
        use crate::app::App;
        use mush_agent::tool::ToolResult;
        use mush_ai::types::TokenCount;

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            image_protos: &mut image_protos,
        };
        let call = bash_call("a", "rm -rf /");
        let events = [
            AgentEvent::TurnStart { turn_index: 0 },
            AgentEvent::MessageEnd {
                message: tool_turn(&model, "let me wipe it", vec![call.clone()]),
            },
            tool_end(&call, ToolResult::error("tool call denied by user")),
        ];
        for event in &events {
            handle_agent_event(
                &mut ctx,
                event,
                mush_runtime::Applied::Nothing,
                &model,
                false,
                &None,
            );
        }

        assert_eq!(
            shown_tool_calls(ctx.app),
            vec![(
                "rm -rf /".into(),
                ToolCallStatus::Error,
                Some("tool call denied by user".into()),
            )]
        );
    }

    #[test]
    fn injecting_the_first_queued_message_leaves_the_second_queued() {
        use crate::app::App;
        use mush_ai::types::{TokenCount, UserContent, UserMessage};

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.push_queued_message("first");
        app.push_queued_message("second");
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            image_protos: &mut image_protos,
        };
        let injected = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("first".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        handle_agent_event(
            &mut ctx,
            &AgentEvent::SteeringInjected { messages: injected },
            mush_runtime::Applied::Nothing,
            &model,
            false,
            &None,
        );

        let queued: Vec<(&str, bool)> = ctx
            .app
            .messages
            .iter()
            .map(|m| (m.content.as_str(), m.queued))
            .collect();
        assert_eq!(queued, vec![("first", false), ("second", true)]);
    }

    #[test]
    fn a_follow_up_injection_dequeues_the_typed_message_too() {
        use crate::app::App;
        use mush_ai::types::{TokenCount, UserContent, UserMessage};

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.push_queued_message("after you finish");
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            image_protos: &mut image_protos,
        };
        let injected = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("after you finish".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        handle_agent_event(
            &mut ctx,
            &AgentEvent::FollowUpInjected { messages: injected },
            mush_runtime::Applied::Nothing,
            &model,
            false,
            &None,
        );

        assert!(!ctx.app.messages[0].queued);
    }

    #[test]
    fn an_injected_bash_done_notice_does_not_dequeue_a_typed_message() {
        use crate::app::App;
        use mush_ai::types::{TokenCount, UserContent, UserMessage};

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let done = mush_tools::kernel_jobs::BashDone {
            pid: Some(1),
            exit_code: 0,
            command: "sleep 1".into(),
        };
        app.push_bash_done(&done, None);
        app.push_queued_message("typed");
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            image_protos: &mut image_protos,
        };
        let injected = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text(done.text()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        handle_agent_event(
            &mut ctx,
            &AgentEvent::SteeringInjected { messages: injected },
            mush_runtime::Applied::Nothing,
            &model,
            false,
            &None,
        );

        assert!(ctx.app.messages[1].queued);
    }
}
