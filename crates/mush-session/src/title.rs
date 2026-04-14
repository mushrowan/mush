//! LLM-based session title generation

use futures::StreamExt;
use mush_ai::Model;
use mush_ai::registry::{ApiRegistry, LlmContext};
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;

const TITLE_PROMPT: &str = "\
Generate a very short title (max 6 words) for this conversation. \
The title should capture the main topic or task. \
Reply with ONLY the title, no quotes, no punctuation at the end, all lowercase.";

/// generate a title from the first few messages using an LLM
pub async fn generate_title(
    mut messages: Vec<Message>,
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
) -> Option<String> {
    if messages.is_empty() {
        return None;
    }

    messages.truncate(4);

    let context = LlmContext {
        system_prompt: Some(TITLE_PROMPT.to_string()),
        messages,
        tools: vec![],
    };

    let mut stream = registry.stream(model, &context, options).await.ok()?;

    let mut title = String::new();
    while let Some(event) = stream.next().await {
        if let StreamEvent::TextDelta { delta, .. } = event {
            title.push_str(&delta);
        }
    }

    let title = title.trim().to_string();
    if title.is_empty() {
        return None;
    }

    // clean up: remove quotes, trailing punctuation
    let title = title
        .trim_matches('"')
        .trim_matches('\'')
        .trim_end_matches('.')
        .trim()
        .to_lowercase();

    if title.is_empty() { None } else { Some(title) }
}
