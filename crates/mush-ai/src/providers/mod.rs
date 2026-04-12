pub mod anthropic;
pub(crate) mod bench_support;
pub mod openai;
pub mod openai_responses;
pub mod sse;

use crate::registry::ApiRegistry;
use crate::types::{Message, ToolResultTrimming};

/// max chars for tool results in older turns
const TRIM_TOOL_OUTPUT_CHARS: usize = 1500;
/// number of recent user messages whose tool results are kept at full size
const RECENT_TURNS_TO_KEEP: usize = 3;

/// find the message index at which "recent" turns begin (sliding window boundary)
pub(crate) fn recent_boundary(messages: &[Message]) -> usize {
    let mut user_count = 0;
    for (i, msg) in messages.iter().enumerate().rev() {
        if matches!(msg, Message::User(_)) {
            user_count += 1;
            if user_count >= RECENT_TURNS_TO_KEEP {
                return i;
            }
        }
    }
    0
}

/// trim a tool result string, keeping head and tail previews
pub(crate) fn trim_tool_output(text: &str) -> String {
    if text.len() <= TRIM_TOOL_OUTPUT_CHARS {
        return text.to_string();
    }
    let preview_end = text.floor_char_boundary(TRIM_TOOL_OUTPUT_CHARS / 2);
    let tail_start = text.len().saturating_sub(TRIM_TOOL_OUTPUT_CHARS / 4);
    let tail_start = text.ceil_char_boundary(tail_start);
    let trimmed = text.len() - preview_end - (text.len() - tail_start);
    format!(
        "{}\n\n[... {} chars trimmed from old tool result ...]\n\n{}",
        &text[..preview_end],
        trimmed,
        &text[tail_start..]
    )
}

/// conditionally trim a tool result based on whether it's in an old turn
/// and the active trimming strategy
pub(crate) fn maybe_trim_tool_output(
    text: &str,
    is_old_turn: bool,
    trimming: ToolResultTrimming,
) -> String {
    if is_old_turn && trimming == ToolResultTrimming::SlidingWindow {
        trim_tool_output(text)
    } else {
        text.to_string()
    }
}

/// register all built-in api providers
pub fn register_builtins(registry: &mut ApiRegistry, client: reqwest::Client) {
    registry.register(Box::new(anthropic::AnthropicProvider {
        client: client.clone(),
    }));
    registry.register(Box::new(openai::OpenaiCompletionsProvider {
        client: client.clone(),
    }));
    registry.register(Box::new(openai_responses::OpenaiResponsesProvider {
        client,
    }));
}
