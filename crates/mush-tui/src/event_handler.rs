//! agent event handling
//!
//! maps AgentEvents to App state mutations, managing conversation
//! history, session tree, and image protocol state

use crossterm::terminal::enable_raw_mode;

use mush_agent::{AgentEvent, summarise_tool_args};
use mush_ai::models;
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;
use mush_session::ConversationState;

use crate::app::App;

/// mutable state shared across the event loop
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
            let cost = models::calculate_cost(model, &message.usage);
            app.finish_streaming(Some(message.usage), Some(cost.total()));
            if message.usage.cache_read_tokens > TokenCount::ZERO
                || message.usage.cache_write_tokens > TokenCount::ZERO
            {
                app.refresh_cache_timer();
            }
            if debug_cache && message.usage.cache_read_tokens > TokenCount::ZERO {
                app.push_system_message(format!(
                    "cache read detected: {} tokens",
                    message.usage.cache_read_tokens
                ));
            }
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
            // re-apply raw mode after external tool execution in case the child
            // process modified terminal settings (e.g. via /dev/tty)
            if tool_name.as_str() == "bash" {
                let _ = enable_raw_mode();
            }
            let output_text = result.content.iter().find_map(|p| match p {
                ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            });

            if tool_name.as_str().eq_ignore_ascii_case("batch") {
                app.end_batch_tool(tool_call_id, output_text);
            } else {
                // extract image data from tool result (base64 → raw bytes)
                let image_data = result.content.iter().find_map(|p| match p {
                    ToolResultContentPart::Image(img) => {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD
                            .decode(&img.data)
                            .ok()
                    }
                    _ => None,
                });
                // create image protocol for inline rendering
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
            // clear previous turn's tool panels (results are already inline in messages)
            app.active_tools.clear();
            if !app.is_streaming {
                app.start_streaming();
            }
        }
        AgentEvent::SteeringInjected { count } => {
            // mark queued display messages as no longer pending
            for msg in app.messages.iter_mut().rev().take(*count) {
                msg.queued = false;
            }
        }
        AgentEvent::FollowUpInjected { .. } => {}
        AgentEvent::ContextTransformed {
            before_count,
            after_count,
        } => {
            app.status = Some(format!(
                "compacted: {before_count} → {after_count} messages"
            ));
        }
        AgentEvent::MaxTurnsReached { max_turns } => {
            app.is_streaming = false;
            app.status = Some(format!("hit max turns limit ({max_turns})"));
        }
        AgentEvent::Error { error } => {
            app.is_streaming = false;
            tracing::error!(%error, "agent error");
            app.status = Some(format!("error: {error}"));
        }
        AgentEvent::AgentEnd => {
            app.is_streaming = false;
            app.active_tools.clear();
        }
        _ => {}
    }
}

/// inject a relevance hint into the last user message
pub fn inject_hint(
    msgs: &mut [Message],
    enricher: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> bool {
    if let Some((pos, content, timestamp_ms)) = hinted_user_message(msgs, enricher) {
        msgs[pos] = Message::User(UserMessage {
            content: UserContent::Text(content),
            timestamp_ms,
        });
        true
    } else {
        false
    }
}

pub fn would_inject_hint(
    msgs: &[Message],
    enricher: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> bool {
    hinted_user_message(msgs, enricher).is_some()
}

fn hinted_user_message(
    msgs: &[Message],
    enricher: &(dyn Fn(&str) -> Option<String> + Send + Sync),
) -> Option<(usize, String, Timestamp)> {
    let pos = msgs.iter().rposition(|m| matches!(m, Message::User(_)))?;
    let Message::User(user_msg) = &msgs[pos] else {
        return None;
    };
    let text = user_msg.text();
    let hint = enricher(&text)?;
    Some((pos, format!("{hint}\n\n{text}"), user_msg.timestamp_ms))
}

/// resolve API key and account ID for a model
pub async fn resolve_auth_for_model(
    model: &Model,
    provider_api_keys: &std::collections::HashMap<String, String>,
) -> (Option<ApiKey>, Option<String>) {
    if let Some(key) = mush_ai::env::env_api_key(&model.provider) {
        return (Some(key), None);
    }

    let provider_name = model.provider.to_string();
    if let Some(key) = provider_api_keys.get(&provider_name) {
        return (ApiKey::new(key.clone()), None);
    }

    match &model.provider {
        Provider::Anthropic => {
            let token = mush_ai::oauth::get_oauth_token("anthropic")
                .await
                .ok()
                .flatten()
                .and_then(ApiKey::new);
            (token, None)
        }
        Provider::Custom(name) if name == "openai-codex" => {
            let token = mush_ai::oauth::get_oauth_token("openai-codex")
                .await
                .ok()
                .flatten()
                .and_then(ApiKey::new);
            let account_id = oauth_account_id("openai-codex");
            (token, account_id)
        }
        _ => (None, None),
    }
}

fn oauth_account_id(provider_id: &str) -> Option<String> {
    mush_ai::oauth::load_credentials().ok().and_then(|store| {
        store
            .providers
            .get(provider_id)
            .and_then(|c| c.account_id.clone())
    })
}

/// compact messages when approaching context limit.
/// `context_tokens` is the actual API-reported input token count from the last call.
///
/// uses escalating strategy:
/// compact messages when approaching context limit
///
/// delegates escalation (masking then LLM summarisation) to
/// `mush_session::compact::auto_compact`, then runs post-compaction
/// hooks if configured.
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

    // post-compaction hooks (needs mush-agent types, so handled here)
    if let Some(hooks) = lifecycle_hooks
        && !hooks.post_compaction.is_empty()
    {
        inject_post_compaction_hooks(hooks, cwd, &mut messages).await;
    }

    messages
}

/// run post-compaction hooks and inject output into context
async fn inject_post_compaction_hooks(
    hooks: &mush_agent::LifecycleHooks,
    cwd: Option<&std::path::Path>,
    messages: &mut Vec<Message>,
) {
    let results = hooks.run_post_compaction(cwd).await;
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

/// max chars to keep in masked tool result text
const MASK_TRUNCATE_LEN: usize = 200;

/// max messages to carry into a forked pane
const FORK_CONTEXT_LIMIT: usize = 30;
/// number of recent tool results to preserve in forked context
const FORK_MASK_RECENT: usize = 3;

/// strip tool outputs to reduce context size while keeping cache prefixes stable
///
/// this applies a deterministic transform to every tool result, so adding new
/// messages does not rewrite older masked content on later turns
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

/// truncate text parts in a tool result, keeping a prefix + size note
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
        // images are dropped entirely from old results
        if matches!(part, ToolResultContentPart::Image(_)) {
            *part = ToolResultContentPart::Text(TextContent {
                text: "[image omitted]".into(),
            });
            changed = true;
        }
    }
    changed
}

/// slim down a conversation for a forked pane.
///
/// keeps the first user message (initial context/instructions) and the
/// most recent messages, aggressively masking tool outputs. this gives
/// forked agents focused context without the full history
pub fn slim_for_fork(messages: &[Message]) -> Vec<Message> {
    if messages.len() <= FORK_CONTEXT_LIMIT {
        let mut result = messages.to_vec();
        mask_observations_with_limit(&mut result, FORK_MASK_RECENT);
        return result;
    }

    let mut result = Vec::with_capacity(FORK_CONTEXT_LIMIT + 2);

    // keep first user message (often contains initial instructions)
    if let Some(first_user) = messages.iter().find(|m| matches!(m, Message::User(_))) {
        result.push(first_user.clone());
    }

    // inject a note about trimmed context
    result.push(Message::User(UserMessage {
        content: UserContent::Text(format!(
            "[context trimmed: {} earlier messages omitted for focus]",
            messages.len() - FORK_CONTEXT_LIMIT
        )),
        timestamp_ms: Timestamp::now(),
    }));

    // keep the most recent messages
    let recent_start = messages.len().saturating_sub(FORK_CONTEXT_LIMIT);
    // avoid duplicating the first user message if it falls in the recent window
    for msg in &messages[recent_start..] {
        if result.len() == 1 && msg == &result[0] {
            continue;
        }
        result.push(msg.clone());
    }

    mask_observations_with_limit(&mut result, FORK_MASK_RECENT);
    result
}

/// mask tool results with a configurable recent-preserve count
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
        // 3 long results
        for _ in 0..3 {
            msgs.push(make_tool_result(&long_text));
        }
        // interspersed user messages
        msgs.push(make_user("question"));
        // 6 short results
        for i in 0..6 {
            msgs.push(make_tool_result(&format!("recent {i}")));
        }

        mask_observations(&mut msgs);

        // long outputs should be truncated
        for msg in &msgs[..3] {
            let text = tool_text(msg);
            assert!(text.contains("truncated"));
            assert!(text.contains("500 chars total"));
            assert!(text.len() < 500);
        }
        // last 6 tool results should be untouched
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

        // short outputs should remain unchanged
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
        // add more messages to ensure neighbouring entries don't matter
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
        // short conversation should be kept as-is
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn slim_for_fork_trims_long_conversation() {
        let mut msgs: Vec<Message> = Vec::new();
        msgs.push(make_user("initial instructions"));
        // 40 tool results with long text
        for i in 0..40 {
            msgs.push(make_tool_result(&format!("{}: {}", i, "x".repeat(500))));
        }
        msgs.push(make_user("latest question"));

        let result = slim_for_fork(&msgs);

        // should be trimmed: first user + trim note + last 30
        assert!(result.len() <= FORK_CONTEXT_LIMIT + 3);
        // first message should be preserved
        match &result[0] {
            Message::User(u) => assert!(u.text().contains("initial instructions")),
            _ => panic!("first message should be user"),
        }
        // second should be the trim note
        match &result[1] {
            Message::User(u) => assert!(u.text().contains("context trimmed")),
            _ => panic!("second message should be trim note"),
        }
    }

    #[test]
    fn slim_for_fork_masks_aggressively() {
        let mut msgs: Vec<Message> = Vec::new();
        // 10 tool results (under FORK_CONTEXT_LIMIT but enough to trigger masking)
        for _ in 0..10 {
            msgs.push(make_tool_result(&"y".repeat(500)));
        }

        let result = slim_for_fork(&msgs);

        // only last FORK_MASK_RECENT (3) should have full output
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
}
