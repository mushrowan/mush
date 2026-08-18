use mush_agent::{AgentEvent, summarise_tool_args};
use mush_ai::ApiKey;
use mush_ai::models;
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;
use mush_session::ConversationState;

use crate::app::App;

pub struct EventCtx<'a> {
    pub app: &'a mut App,
    pub conversation: &'a mut ConversationState,
    pub image_protos: &'a mut std::collections::HashMap<
        (usize, usize),
        ratatui_image::protocol::StatefulProtocol,
    >,
}

pub fn handle_agent_event(
    ctx: &mut EventCtx<'_>,
    event: &AgentEvent,
    model: &Model,
    debug_cache: bool,
    image_picker: &Option<ratatui_image::picker::Picker>,
) {
    let EventCtx {
        app,
        conversation,
        image_protos,
    } = ctx;
    match event {
        AgentEvent::StreamEvent { event } => match event {
            StreamEvent::TextDelta { delta, .. } => app.push_text_delta(delta),
            StreamEvent::ThinkingDelta { delta, .. } => app.push_thinking_delta(delta),
            StreamEvent::ToolCallDelta { delta, .. } => app.push_tool_args_delta(delta),
            _ => {}
        },
        AgentEvent::MessageEnd { message } => {
            apply_message_end(app, message, model, debug_cache);
            let msg = Message::Assistant(message.clone());
            conversation.append_message(msg);
        }
        AgentEvent::ToolExecStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            if tool_name.as_str().eq_ignore_ascii_case("batch") {
                let summary = summarise_tool_args("batch", args);
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
                app.start_batch_tool(tool_call_id, &summary, &sub_calls);
            } else {
                let summary = summarise_tool_args(tool_name.as_str(), args);
                app.start_tool(tool_call_id, tool_name.as_str(), &summary);
            }
        }
        AgentEvent::ToolExecEnd {
            tool_call_id,
            tool_name,
            result,
        } => {
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
            }
            let msg = Message::ToolResult(ToolResultMessage {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                content: result.content.clone(),
                outcome: result.outcome,
                timestamp_ms: Timestamp::now(),
            });
            conversation.append_message(msg);
        }
        AgentEvent::TurnStart { .. } => {
            app.active_tools.clear();
            if !app.stream.active {
                app.start_streaming();
            }
        }
        AgentEvent::SteeringInjected { messages } => {
            for mut msg in app.messages.iter_mut().rev().take(messages.len()) {
                msg.queued = false;
            }
            for msg in messages {
                conversation.append_message(msg.clone());
            }
        }
        AgentEvent::FollowUpInjected { messages } => {
            for msg in messages {
                conversation.append_message(msg.clone());
            }
        }
        AgentEvent::ContextTransformed {
            before_count,
            after_count,
        } => {
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

pub async fn resolve_auth_for_model(
    model: &Model,
    provider_api_keys: &std::collections::HashMap<String, mush_ai::ApiKey>,
) -> (Option<ApiKey>, Option<String>) {
    let resolved = mush_ai::auth::resolve_auth(
        &model.provider,
        provider_api_keys,
        mush_ai::credentials::default_store().as_ref(),
    )
    .await;
    (resolved.api_key, resolved.account_id)
}

pub async fn resolve_auth_into_options(
    model: &Model,
    mut options: StreamOptions,
    provider_api_keys: &std::collections::HashMap<String, mush_ai::ApiKey>,
) -> StreamOptions {
    let (api_key, account_id) = resolve_auth_for_model(model, provider_api_keys).await;
    options.api_key = api_key;
    options.account_id = account_id;
    options
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    fn custom_model(provider: &str) -> Model {
        Model {
            id: format!("{provider}/model").into(),
            name: "test model".into(),
            api: Api::OpenaiResponses,
            provider: Provider::Custom(provider.into()),
            base_url: "https://example.invalid".into(),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(128_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(4096),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        }
    }

    #[tokio::test]
    async fn resolve_auth_into_options_replaces_stale_key_for_compaction() {
        let provider = "compaction-auth-test-provider";
        let model = custom_model(provider);
        let fresh_key = ApiKey::new("fresh-compaction-key").unwrap();
        let mut provider_api_keys = std::collections::HashMap::new();
        provider_api_keys.insert(provider.into(), fresh_key.clone());
        let stale_options = StreamOptions {
            api_key: Some(ApiKey::new("stale-startup-key").unwrap()),
            account_id: Some("stale-account".into()),
            ..StreamOptions::default()
        };

        let options = resolve_auth_into_options(&model, stale_options, &provider_api_keys).await;

        assert_eq!(options.api_key, Some(fresh_key));
        assert_eq!(options.account_id, None);
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn auto_compact(
    messages: Vec<Message>,
    context_tokens: TokenCount,
    context_window: TokenCount,
    registry: &mush_ai::registry::ApiRegistry,
    model: &Model,
    options: &StreamOptions,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
) -> Vec<Message> {
    let result = mush_session::compact::auto_compact(
        messages,
        context_tokens,
        context_window,
        registry,
        model,
        options,
    )
    .await;

    if result.masked_count > 0 {
        tracing::info!(
            masked = result.masked_count,
            tokens_saved = result.mask_tokens_saved,
            "observation masking applied"
        );
    }
    if result.summarised_count > 0 {
        tracing::info!(
            summarised = result.summarised_count,
            "LLM compaction applied"
        );
    }

    let mut messages = result.messages;

    if let Some(hooks) = lifecycle_hooks
        && !hooks
            .for_point(mush_agent::HookPoint::PostCompaction)
            .is_empty()
    {
        inject_post_compaction_hooks(hooks, cwd, &mut messages).await;
    }

    messages
}

async fn inject_post_compaction_hooks(
    hooks: &mush_agent::LifecycleHooks,
    cwd: Option<&std::path::Path>,
    messages: &mut Vec<Message>,
) {
    let results = hooks
        .run_all(mush_agent::HookPoint::PostCompaction, cwd)
        .await;
    let output: String = results
        .iter()
        .filter(|r| !r.output.is_empty())
        .map(|r| r.output.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    if !output.is_empty() {
        tracing::info!("post-compaction hook output injected into context");
        messages.push(Message::User(UserMessage {
            content: UserContent::Text(format!("[post-compaction hook output]\n{output}")),
            timestamp_ms: Timestamp::now(),
        }));
    }

    for r in &results {
        if !r.success {
            tracing::warn!(command = %r.command, "post-compaction hook failed: {}", r.output);
        }
    }
}

const MASK_TRUNCATE_LEN: usize = 200;

const FORK_CONTEXT_LIMIT: usize = 30;
const FORK_MASK_RECENT: usize = 3;

#[cfg(test)]
fn mask_observations(messages: &mut [Message]) -> bool {
    let mut changed = false;
    for msg in messages.iter_mut() {
        if let Message::ToolResult(tr) = msg {
            changed |= mask_tool_result_content(&mut tr.content);
        }
    }
    changed
}

fn mask_tool_result_content(content: &mut [ToolResultContentPart]) -> bool {
    let mut changed = false;
    for part in content.iter_mut() {
        if let ToolResultContentPart::Text(t) = part {
            let original_len = t.text.len();
            if original_len > MASK_TRUNCATE_LEN {
                let truncated: String = t.text.chars().take(MASK_TRUNCATE_LEN).collect();
                t.text = format!("{truncated}\n\n[... truncated, {original_len} chars total]");
                changed = true;
            }
        }
        if matches!(part, ToolResultContentPart::Image(_)) {
            *part = ToolResultContentPart::Text(TextContent {
                text: "[image omitted]".into(),
            });
            changed = true;
        }
    }
    changed
}

pub fn slim_for_fork(messages: &[Message]) -> Vec<Message> {
    if messages.len() <= FORK_CONTEXT_LIMIT {
        let mut result = messages.to_vec();
        mask_observations_with_limit(&mut result, FORK_MASK_RECENT);
        return result;
    }

    let mut result = Vec::with_capacity(FORK_CONTEXT_LIMIT + 2);

    if let Some(first_user) = messages.iter().find(|m| matches!(m, Message::User(_))) {
        result.push(first_user.clone());
    }

    result.push(Message::User(UserMessage {
        content: UserContent::Text(format!(
            "[context trimmed: {} earlier messages omitted for focus]",
            messages.len() - FORK_CONTEXT_LIMIT
        )),
        timestamp_ms: Timestamp::now(),
    }));

    let recent_start = messages.len().saturating_sub(FORK_CONTEXT_LIMIT);
    for msg in &messages[recent_start..] {
        if result.len() == 1 && msg == &result[0] {
            continue;
        }
        result.push(msg.clone());
    }

    mask_observations_with_limit(&mut result, FORK_MASK_RECENT);
    result
}

fn mask_observations_with_limit(messages: &mut [Message], preserve_recent: usize) {
    let tool_positions: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, Message::ToolResult(_)))
        .map(|(i, _)| i)
        .collect();

    if tool_positions.len() <= preserve_recent {
        return;
    }

    let to_mask = tool_positions.len() - preserve_recent;
    for &pos in &tool_positions[..to_mask] {
        if let Message::ToolResult(tr) = &mut messages[pos] {
            let _ = mask_tool_result_content(&mut tr.content);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool_result(text: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "tc_1".into(),
            tool_name: "read".into(),
            content: vec![ToolResultContentPart::Text(TextContent {
                text: text.into(),
            })],
            outcome: ToolOutcome::Success,
            timestamp_ms: Timestamp::now(),
        })
    }

    fn make_user(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp_ms: Timestamp::now(),
        })
    }

    fn tool_text(msg: &Message) -> &str {
        match msg {
            Message::ToolResult(tr) => match &tr.content[0] {
                ToolResultContentPart::Text(t) => &t.text,
                _ => panic!("expected text"),
            },
            _ => panic!("expected tool result"),
        }
    }

    #[test]
    fn mask_is_deterministic_across_turns() {
        let long = "x".repeat(500);
        let mut base = vec![
            make_user("q"),
            make_tool_result(&long),
            make_tool_result("short"),
        ];

        let mut once = base.clone();
        mask_observations(&mut once);

        base.push(make_tool_result("new output"));
        let mut with_new = base.clone();
        mask_observations(&mut with_new);

        assert_eq!(&with_new[..once.len()], once.as_slice());
    }

    #[test]
    fn mask_truncates_long_results() {
        let long_text = "x".repeat(500);
        let mut msgs: Vec<Message> = Vec::new();
        for _ in 0..3 {
            msgs.push(make_tool_result(&long_text));
        }
        msgs.push(make_user("question"));
        for i in 0..6 {
            msgs.push(make_tool_result(&format!("recent {i}")));
        }

        mask_observations(&mut msgs);

        for msg in &msgs[..3] {
            let text = tool_text(msg);
            assert!(text.contains("truncated"));
            assert!(text.contains("500 chars total"));
            assert!(text.len() < 500);
        }
        for (i, msg) in msgs[4..].iter().enumerate() {
            assert_eq!(tool_text(msg), format!("recent {i}"));
        }
    }

    #[test]
    fn mask_leaves_short_text_alone() {
        let mut msgs: Vec<Message> = Vec::new();
        for _ in 0..8 {
            msgs.push(make_tool_result("short"));
        }

        mask_observations(&mut msgs);

        for msg in &msgs {
            assert_eq!(tool_text(msg), "short");
        }
    }

    #[test]
    fn mask_replaces_images() {
        let mut msgs = vec![Message::ToolResult(ToolResultMessage {
            tool_call_id: "tc_img".into(),
            tool_name: "read".into(),
            content: vec![ToolResultContentPart::Image(ImageContent {
                data: "base64data".into(),
                mime_type: ImageMimeType::Png,
            })],
            outcome: ToolOutcome::Success,
            timestamp_ms: Timestamp::now(),
        })];
        for _ in 0..6 {
            msgs.push(make_tool_result("recent"));
        }

        mask_observations(&mut msgs);

        assert_eq!(tool_text(&msgs[0]), "[image omitted]");
    }

    #[test]
    fn slim_for_fork_short_conversation() {
        let msgs: Vec<Message> = vec![make_user("hello"), make_tool_result("output")];
        let result = slim_for_fork(&msgs);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn slim_for_fork_trims_long_conversation() {
        let mut msgs: Vec<Message> = Vec::new();
        msgs.push(make_user("initial instructions"));
        for i in 0..40 {
            msgs.push(make_tool_result(&format!("{}: {}", i, "x".repeat(500))));
        }
        msgs.push(make_user("latest question"));

        let result = slim_for_fork(&msgs);

        assert!(result.len() <= FORK_CONTEXT_LIMIT + 3);
        match &result[0] {
            Message::User(u) => assert!(u.text().contains("initial instructions")),
            _ => panic!("first message should be user"),
        }
        match &result[1] {
            Message::User(u) => assert!(u.text().contains("context trimmed")),
            _ => panic!("second message should be trim note"),
        }
    }

    #[test]
    fn slim_for_fork_masks_aggressively() {
        let mut msgs: Vec<Message> = Vec::new();
        for _ in 0..10 {
            msgs.push(make_tool_result(&"y".repeat(500)));
        }

        let result = slim_for_fork(&msgs);

        let full_outputs = result
            .iter()
            .filter(|m| {
                if let Message::ToolResult(tr) = m {
                    tr.content.iter().any(|p| match p {
                        ToolResultContentPart::Text(t) => !t.text.contains("truncated"),
                        _ => false,
                    })
                } else {
                    false
                }
            })
            .count();
        assert_eq!(full_outputs, FORK_MASK_RECENT);
    }

    #[test]
    fn agent_error_becomes_system_message() {
        use crate::app::{App, MessageRole};
        use mush_ai::types::TokenCount;

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let mut conversation = ConversationState::new();
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            conversation: &mut conversation,
            image_protos: &mut image_protos,
        };

        let event = AgentEvent::Error {
            error: "anthropic returned 400 Bad Request".into(),
        };
        handle_agent_event(&mut ctx, &event, &model, false, &None);

        assert!(
            ctx.app.status.is_none(),
            "error should not be in status bar"
        );
        let last = ctx.app.messages.last().expect("should have a message");
        assert_eq!(last.role, MessageRole::System);
        assert!(last.content.contains("400 Bad Request"));
    }

    #[test]
    fn steering_injection_persists_messages_to_conversation_tree() {
        use crate::app::App;
        use mush_ai::types::{TokenCount, UserContent, UserMessage};

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let mut conversation = ConversationState::new();
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            conversation: &mut conversation,
            image_protos: &mut image_protos,
        };

        let injected = vec![Message::User(UserMessage {
            content: UserContent::Text("steered mid-stream".into()),
            timestamp_ms: Timestamp::zero(),
        })];

        let event = AgentEvent::SteeringInjected {
            messages: injected.clone(),
        };
        handle_agent_event(&mut ctx, &event, &model, false, &None);

        let context = ctx.conversation.context();
        assert_eq!(context.len(), 1, "steering message should land in tree");
        assert!(
            matches!(&context[0], Message::User(u) if u.text() == "steered mid-stream"),
            "tree should contain the steered text, got: {:?}",
            context
        );
    }

    #[test]
    fn follow_up_injection_persists_messages_to_conversation_tree() {
        use crate::app::App;
        use mush_ai::types::{TokenCount, UserContent, UserMessage};

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let mut conversation = ConversationState::new();
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            conversation: &mut conversation,
            image_protos: &mut image_protos,
        };

        let injected = vec![Message::User(UserMessage {
            content: UserContent::Text("follow up".into()),
            timestamp_ms: Timestamp::zero(),
        })];

        let event = AgentEvent::FollowUpInjected {
            messages: injected.clone(),
        };
        handle_agent_event(&mut ctx, &event, &model, false, &None);

        let context = ctx.conversation.context();
        assert_eq!(context.len(), 1, "follow-up message should land in tree");
        assert!(
            matches!(&context[0], Message::User(u) if u.text() == "follow up"),
            "tree should contain the follow-up text, got: {:?}",
            context
        );
    }

    #[test]
    fn context_transformed_surfaces_in_message_log() {
        use crate::app::{App, MessageRole};
        use mush_ai::types::TokenCount;

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let mut conversation = ConversationState::new();
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            conversation: &mut conversation,
            image_protos: &mut image_protos,
        };

        let event = AgentEvent::ContextTransformed {
            before_count: 42,
            after_count: 7,
        };
        handle_agent_event(&mut ctx, &event, &model, false, &None);

        let last = ctx
            .app
            .messages
            .last()
            .expect("compaction should surface in message log");
        assert_eq!(last.role, MessageRole::System);
        assert!(
            last.content.contains("compacted")
                && last.content.contains("42")
                && last.content.contains('7'),
            "expected counts in log message, got: {:?}",
            last.content
        );
    }

    #[test]
    fn max_turns_becomes_system_message() {
        use crate::app::{App, MessageRole};
        use mush_ai::types::TokenCount;

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let mut conversation = ConversationState::new();
        let mut image_protos = std::collections::HashMap::new();
        let model = mush_ai::models::all_models().first().unwrap().clone();
        let mut ctx = EventCtx {
            app: &mut app,
            conversation: &mut conversation,
            image_protos: &mut image_protos,
        };

        let event = AgentEvent::MaxTurnsReached { max_turns: 10 };
        handle_agent_event(&mut ctx, &event, &model, false, &None);

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

        let mut conversation = ConversationState::new();
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
            conversation: &mut conversation,
            image_protos: &mut image_protos,
        };
        handle_agent_event(
            &mut ctx,
            &AgentEvent::MessageEnd { message: errored },
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
}
