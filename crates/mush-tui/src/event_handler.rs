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

/// compact messages when approaching context limit
pub async fn auto_compact(
    messages: Vec<Message>,
    context_window: usize,
    registry: &mush_ai::registry::ApiRegistry,
    model: &Model,
    options: &StreamOptions,
) -> Vec<Message> {
    use mush_session::compact;

    if !compact::needs_compaction(&messages, context_window) {
        return messages;
    }

    compact::llm_compact(messages, registry, model, options, Some(10))
        .await
        .messages
}
