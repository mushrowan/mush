//! agent event handling
//!
//! maps AgentEvents to App state mutations, managing conversation
//! history, session tree, and image protocol state

use crossterm::terminal::enable_raw_mode;

use mush_agent::{AgentEvent, summarise_tool_args};
use mush_ai::models;
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;
use mush_session::tree::SessionTree;

use crate::app::App;

/// mutable state shared across the event loop
pub struct EventCtx<'a> {
    pub app: &'a mut App,
    pub conversation: &'a mut Vec<Message>,
    pub session_tree: &'a mut SessionTree,
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
        session_tree,
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
            if message.usage.cache_read_tokens > 0 || message.usage.cache_write_tokens > 0 {
                app.refresh_cache_timer();
            }
            if debug_cache && message.usage.cache_read_tokens > 0 {
                app.push_system_message(format!(
                    "cache read detected: {} tokens",
                    message.usage.cache_read_tokens
                ));
            }
            let msg = Message::Assistant(message.clone());
            session_tree.append_message(msg.clone());
            conversation.push(msg);
        }
        AgentEvent::ToolExecStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            let summary = summarise_tool_args(tool_name.as_str(), args);
            app.start_tool(tool_call_id.as_str(), tool_name.as_str(), &summary);
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
                tool_call_id.as_str(),
                tool_name.as_str(),
                result.outcome,
                output_text,
                image_data,
            );
            let msg = Message::ToolResult(ToolResultMessage {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                content: result.content.clone(),
                outcome: result.outcome,
                timestamp_ms: Timestamp::now(),
            });
            session_tree.append_message(msg.clone());
            conversation.push(msg);
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
) {
    let Some(pos) = msgs.iter().rposition(|m| matches!(m, Message::User(_))) else {
        return;
    };

    let Message::User(ref user_msg) = msgs[pos] else {
        return;
    };

    let text = user_msg.text();

    if let Some(hint) = enricher(&text) {
        msgs[pos] = Message::User(UserMessage {
            content: UserContent::Text(format!("{hint}\n\n{text}")),
            timestamp_ms: user_msg.timestamp_ms,
        });
    }
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
/// `context_tokens` is the actual API-reported input token count from the last call
pub async fn auto_compact(
    messages: Vec<Message>,
    context_tokens: u64,
    context_window: u64,
    registry: &mush_ai::registry::ApiRegistry,
    model: &Model,
    options: &StreamOptions,
) -> Vec<Message> {
    use mush_session::compact;

    // 95% of context window, using real token counts from the API
    let threshold = context_window * 95 / 100;
    if context_tokens < threshold || messages.len() <= 10 {
        return messages;
    }

    compact::llm_compact(messages, registry, model, options, Some(10))
        .await
        .messages
}

/// max chars to keep in masked tool result text
const MASK_TRUNCATE_LEN: usize = 200;
/// number of most-recent tool results to keep unmasked
const MASK_PRESERVE_RECENT: usize = 6;

/// max messages to carry into a forked pane
const FORK_CONTEXT_LIMIT: usize = 30;
/// number of recent tool results to preserve in forked context
const FORK_MASK_RECENT: usize = 3;

/// strip old tool outputs to reduce context size.
///
/// keeps the most recent `MASK_PRESERVE_RECENT` tool results intact.
/// older tool results have their text content truncated with a summary
/// of the original length. tool call names and structure are preserved
/// so the model retains action history without the bulk of output.
pub fn mask_observations(messages: &mut [Message]) {
    mask_observations_with_limit(messages, MASK_PRESERVE_RECENT);
}

/// truncate text parts in a tool result, keeping a prefix + size note
fn mask_tool_result_content(content: &mut [ToolResultContentPart]) {
    for part in content.iter_mut() {
        if let ToolResultContentPart::Text(t) = part {
            let original_len = t.text.len();
            if original_len > MASK_TRUNCATE_LEN {
                let truncated: String = t.text.chars().take(MASK_TRUNCATE_LEN).collect();
                t.text =
                    format!("{truncated}\n\n[... truncated, {original_len} chars total]");
            }
        }
        // images are dropped entirely from old results
        if matches!(part, ToolResultContentPart::Image(_)) {
            *part = ToolResultContentPart::Text(TextContent {
                text: "[image omitted]".into(),
            });
        }
    }
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
            mask_tool_result_content(&mut tr.content);
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
    fn mask_preserves_recent_results() {
        let mut msgs: Vec<Message> = (0..6)
            .map(|i| make_tool_result(&format!("output {i}")))
            .collect();

        mask_observations(&mut msgs);

        // all 6 should be preserved (at threshold)
        for (i, msg) in msgs.iter().enumerate() {
            assert_eq!(tool_text(msg), format!("output {i}"));
        }
    }

    #[test]
    fn mask_truncates_old_results() {
        let long_text = "x".repeat(500);
        let mut msgs: Vec<Message> = Vec::new();
        // 3 old results with long text
        for _ in 0..3 {
            msgs.push(make_tool_result(&long_text));
        }
        // interspersed user messages
        msgs.push(make_user("question"));
        // 6 recent results (should be preserved)
        for i in 0..6 {
            msgs.push(make_tool_result(&format!("recent {i}")));
        }

        mask_observations(&mut msgs);

        // first 3 should be truncated
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

        // first 2 are masked but text is short, so no truncation
        for msg in &msgs[..2] {
            assert_eq!(tool_text(msg), "short");
        }
    }

    #[test]
    fn mask_replaces_images() {
        let mut msgs = vec![
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_img".into(),
                tool_name: "read".into(),
                content: vec![ToolResultContentPart::Image(ImageContent {
                    data: "base64data".into(),
                    mime_type: ImageMimeType::Png,
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::now(),
            }),
        ];
        // add 6 more to push the first one past the threshold
        for _ in 0..6 {
            msgs.push(make_tool_result("recent"));
        }

        mask_observations(&mut msgs);

        assert_eq!(tool_text(&msgs[0]), "[image omitted]");
    }

    #[test]
    fn slim_for_fork_short_conversation() {
        let msgs: Vec<Message> = vec![
            make_user("hello"),
            make_tool_result("output"),
        ];
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
