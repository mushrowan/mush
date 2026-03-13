//! collect a streaming provider response into a single AssistantMessage

use futures::StreamExt;
use mush_ai::registry::{ApiProvider, LlmContext, ProviderError};
use mush_ai::stream::StreamEvent;
use mush_ai::types::{AssistantMessage, Model, StreamOptions};

/// run a provider stream to completion, returning the final message
pub async fn collect_response(
    provider: &dyn ApiProvider,
    model: &Model,
    context: &LlmContext,
    options: &StreamOptions,
) -> Result<AssistantMessage, ProviderError> {
    let stream = provider.stream(model, context, options).await?;
    tokio::pin!(stream);

    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::Done { message, .. } => return Ok(message),
            StreamEvent::Error { message, .. } => {
                return Err(ProviderError::Other(
                    message
                        .error_message
                        .unwrap_or_else(|| "stream error".into()),
                ));
            }
            _ => {} // skip intermediate events
        }
    }
    Err(ProviderError::Other("stream ended without Done".into()))
}
