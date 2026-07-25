//! one-shot LLM calls
//!
//! some work is a single tool-free question with a text answer:
//! compaction summaries, session titles. those callers want the text or a
//! reason it never arrived, not a stream of events, and they all need the
//! same failure classification so a rate limit is not mistaken for an
//! empty answer.

use futures::StreamExt;

use crate::registry::{ApiRegistry, EventStream, LlmContext};
use crate::stream::StreamEvent;
use crate::types::{Model, StopReason, StreamOptions};

/// why a one-shot call produced no text
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OneshotError {
    /// the request never reached a stream (network, auth, http status)
    #[error("request failed: {0}")]
    Request(String),
    /// the provider accepted the request then surfaced a failure mid-stream
    #[error("stream error: {0}")]
    Stream(String),
    /// the stream completed but carried no text at all
    #[error("stream completed without text")]
    EmptyResponse,
}

impl OneshotError {
    /// whether retrying the same call could plausibly succeed. mirrors the
    /// provider-level classification, so overloaded and rate-limited
    /// mid-stream failures are worth another attempt while a refusal or an
    /// empty answer is not
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Request(_) | Self::EmptyResponse => false,
            Self::Stream(message) => crate::registry::is_retryable_stream_error_message(message),
        }
    }
}

/// drain a stream and return the assistant text it produced.
///
/// a mid-stream failure is reported rather than truncating silently, which
/// is the difference between "the model answered nothing" and "the request
/// was rate limited"
pub async fn collect_stream_text(mut stream: EventStream) -> Result<String, OneshotError> {
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta { delta, .. } => text.push_str(&delta),
            StreamEvent::Done { reason, message } => {
                if reason == StopReason::Error || message.stop_reason == StopReason::Error {
                    return Err(OneshotError::Stream(
                        message
                            .error_message
                            .unwrap_or_else(|| "stream ended with error".into()),
                    ));
                }
                break;
            }
            StreamEvent::Error { message, .. } => {
                return Err(OneshotError::Stream(
                    message
                        .error_message
                        .unwrap_or_else(|| "stream emitted error".into()),
                ));
            }
            _ => {}
        }
    }

    if text.is_empty() {
        Err(OneshotError::EmptyResponse)
    } else {
        Ok(text)
    }
}

/// make a single tool-free call and return its text
pub async fn collect_text(
    registry: &ApiRegistry,
    model: &Model,
    context: &LlmContext,
    options: &StreamOptions,
) -> Result<String, OneshotError> {
    let stream = registry
        .stream(model, context, options)
        .await
        .map_err(|e| OneshotError::Request(e.to_string()))?;
    collect_stream_text(stream).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantMessage, Timestamp};

    fn assistant(error_message: Option<&str>, stop_reason: StopReason) -> AssistantMessage {
        AssistantMessage::builder()
            .model(crate::types::ModelId::from("test-model"))
            .provider(crate::types::Provider::Anthropic)
            .api(crate::types::Api::AnthropicMessages)
            .stop_reason(stop_reason)
            .maybe_error_message(error_message.map(str::to_string))
            .timestamp_ms(Timestamp::zero())
            .build()
    }

    fn stream_of(events: Vec<StreamEvent>) -> EventStream {
        Box::pin(futures::stream::iter(events))
    }

    fn delta(text: &str) -> StreamEvent {
        StreamEvent::TextDelta {
            content_index: 0,
            delta: text.into(),
        }
    }

    #[tokio::test]
    async fn concatenates_text_deltas() {
        let events = vec![
            delta("fix the "),
            delta("scroll jump"),
            StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant(None, StopReason::Stop),
            },
        ];
        let text = collect_stream_text(stream_of(events)).await.unwrap();
        assert_eq!(text, "fix the scroll jump");
    }

    #[tokio::test]
    async fn reports_a_mid_stream_failure_instead_of_truncating() {
        // the whole point: a rate limit that lands after a partial answer
        // must not read as "the model said this much and stopped"
        let events = vec![
            delta("partial"),
            StreamEvent::Error {
                reason: StopReason::Error,
                message: assistant(Some("rate_limit_exceeded"), StopReason::Error),
            },
        ];
        let error = collect_stream_text(stream_of(events)).await.unwrap_err();
        assert_eq!(error, OneshotError::Stream("rate_limit_exceeded".into()));
        assert!(error.is_retryable(), "a rate limit is worth retrying");
    }

    #[tokio::test]
    async fn reports_an_error_stop_reason_on_done() {
        let events = vec![StreamEvent::Done {
            reason: StopReason::Error,
            message: assistant(Some("overloaded_error"), StopReason::Error),
        }];
        let error = collect_stream_text(stream_of(events)).await.unwrap_err();
        assert_eq!(error, OneshotError::Stream("overloaded_error".into()));
        assert!(error.is_retryable());
    }

    #[tokio::test]
    async fn an_answerless_stream_is_not_retryable() {
        let events = vec![StreamEvent::Done {
            reason: StopReason::Stop,
            message: assistant(None, StopReason::Stop),
        }];
        let error = collect_stream_text(stream_of(events)).await.unwrap_err();
        assert_eq!(error, OneshotError::EmptyResponse);
        assert!(
            !error.is_retryable(),
            "asking again yields the same nothing"
        );
    }
}
