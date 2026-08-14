use mush_ai::Model;
use mush_ai::oneshot;
use mush_ai::registry::{ApiRegistry, LlmContext};
use mush_ai::types::*;

const TITLE_PROMPT: &str = "\
Generate a very short title (max 6 words) for this conversation. \
The title should capture the main topic or task. \
Reply with ONLY the title, no quotes, no punctuation at the end, all lowercase.";

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

    let title = match oneshot::collect_text(registry, model, &context, options).await {
        Ok(text) => text,
        Err(error) => {
            tracing::debug!(%error, "session title generation failed");
            return None;
        }
    };

    let title = title
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_end_matches('.')
        .trim()
        .to_lowercase();

    if title.is_empty() { None } else { Some(title) }
}
