use futures::StreamExt;

use crate::registry::{
    ApiRegistry, EventStream, LlmContext, MAX_RETRIES, STREAM_ENDED_EARLY, retry_backoff,
    retry_delay, retry_stream_error,
};
use crate::stream::StreamEvent;
use crate::types::{Model, StopReason, StreamOptions};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OneshotError {
    #[error("request failed: {0}")]
    Request(String),
    #[error("stream error: {0}")]
    Stream(String),
    #[error("stream completed without text")]
    EmptyResponse,
}

impl OneshotError {
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Request(_) | Self::EmptyResponse => false,
            Self::Stream(message) => crate::registry::is_retryable_stream_error_message(message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retried<T> {
    pub value: T,
    pub retries: u32,
}

pub async fn collect_stream_text(mut stream: EventStream) -> Result<String, OneshotError> {
    let mut text = String::new();
    let mut finished = false;
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
                finished = true;
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

    if !finished {
        Err(OneshotError::Stream(STREAM_ENDED_EARLY.into()))
    } else if text.is_empty() {
        Err(OneshotError::EmptyResponse)
    } else {
        Ok(text)
    }
}

pub async fn collect_text(
    registry: &ApiRegistry,
    model: &Model,
    context: &LlmContext,
    options: &StreamOptions,
) -> Result<String, OneshotError> {
    collect_text_retrying(registry, model, context, options)
        .await
        .map(|text| text.value)
        .map_err(|error| error.value)
}

pub async fn collect_text_retrying(
    registry: &ApiRegistry,
    model: &Model,
    context: &LlmContext,
    options: &StreamOptions,
) -> Result<Retried<String>, Retried<OneshotError>> {
    let mut retries = 0;
    loop {
        let can_retry = retries < MAX_RETRIES;
        let (error, retryable, retry_after) = match registry.stream(model, context, options).await {
            Ok(stream) => match collect_stream_text(stream).await {
                Ok(text) => {
                    return Ok(Retried {
                        value: text,
                        retries,
                    });
                }
                Err(error) => {
                    let retryable = match &error {
                        OneshotError::Stream(message) => {
                            can_retry && retry_stream_error(message, retries)
                        }
                        OneshotError::Request(_) | OneshotError::EmptyResponse => false,
                    };
                    (error, retryable, None)
                }
            },
            Err(error) => {
                let retryable = can_retry && error.is_retryable();
                if retryable {
                    tracing::warn!(%error, attempt = retries, "retryable request error");
                }
                let retry_after = error.retry_after();
                (
                    OneshotError::Request(error.to_string()),
                    retryable,
                    retry_after,
                )
            }
        };
        if !retryable {
            return Err(Retried {
                value: error,
                retries,
            });
        }
        let Some(delay) = retry_delay(retry_backoff(retries + 1), retry_after) else {
            tracing::warn!(%error, "the provider asked to wait past the retry cap, giving up");
            return Err(Retried {
                value: error,
                retries,
            });
        };
        retries += 1;
        tracing::warn!(
            attempt = retries,
            ?delay,
            "retrying a one-shot provider call"
        );
        tokio::time::sleep(delay).await;
    }
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

    struct Scripted {
        calls: std::sync::Arc<std::sync::atomic::AtomicU32>,
        script: fn(u32) -> Result<Vec<StreamEvent>, crate::registry::ProviderError>,
    }

    #[async_trait::async_trait]
    impl crate::registry::ApiProvider for Scripted {
        fn api(&self) -> crate::types::Api {
            crate::types::Api::AnthropicMessages
        }

        async fn stream(
            &self,
            _model: &Model,
            _context: &LlmContext,
            _options: &StreamOptions,
        ) -> Result<EventStream, crate::registry::ProviderError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (self.script)(call).map(stream_of)
        }
    }

    fn test_model() -> Model {
        Model::builder()
            .id("test".into())
            .name("test".into())
            .api(crate::types::Api::AnthropicMessages)
            .provider(crate::types::Provider::Anthropic)
            .base_url("https://api.anthropic.com".into())
            .context_window(crate::types::TokenCount::new(200_000))
            .max_output_tokens(crate::types::TokenCount::new(8192))
            .build()
    }

    async fn run(
        script: fn(u32) -> Result<Vec<StreamEvent>, crate::registry::ProviderError>,
    ) -> (u32, Result<Retried<String>, Retried<OneshotError>>) {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(Scripted {
            calls: calls.clone(),
            script,
        }));
        let context = LlmContext {
            system_prompt: None,
            messages: Vec::new(),
            tools: Vec::new(),
        };
        let result = collect_text_retrying(
            &registry,
            &test_model(),
            &context,
            &StreamOptions::default(),
        )
        .await;
        (calls.load(std::sync::atomic::Ordering::SeqCst), result)
    }

    fn answered() -> Vec<StreamEvent> {
        vec![
            delta("a title"),
            StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant(None, StopReason::Stop),
            },
        ]
    }

    fn failed(error: &str) -> Vec<StreamEvent> {
        vec![
            delta("half"),
            StreamEvent::Error {
                reason: StopReason::Error,
                message: assistant(Some(error), StopReason::Error),
            },
        ]
    }

    #[tokio::test]
    async fn a_stream_that_stops_without_done_is_an_early_end() {
        let error = collect_stream_text(stream_of(vec![delta("cut o")]))
            .await
            .unwrap_err();
        assert_eq!(error, OneshotError::Stream(STREAM_ENDED_EARLY.into()));
        assert!(error.is_retryable());
    }

    #[tokio::test(start_paused = true)]
    async fn a_mid_stream_drop_is_retried_without_keeping_the_partial() {
        let (calls, result) = run(|call| {
            Ok(if call == 0 {
                failed("error decoding response body: Connection reset by peer (os error 104)")
            } else {
                answered()
            })
        })
        .await;
        assert_eq!(calls, 2);
        assert_eq!(
            result.unwrap(),
            Retried {
                value: "a title".into(),
                retries: 1
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_and_an_early_end_are_retried() {
        let (calls, result) = run(|call| {
            Ok(match call {
                0 => {
                    failed("invalid_request: cyber_policy: flagged for possible cybersecurity risk")
                }
                1 => vec![delta("cut o")],
                _ => answered(),
            })
        })
        .await;
        assert_eq!(calls, 3);
        assert_eq!(result.unwrap().retries, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_retryable_request_error_is_retried() {
        let (calls, result) = run(|call| {
            if call == 0 {
                Err(crate::registry::ProviderError::ApiError {
                    api: "test",
                    status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    content_type: "text/plain".into(),
                    body: "unavailable".into(),
                    retry_after: None,
                })
            } else {
                Ok(answered())
            }
        })
        .await;
        assert_eq!(calls, 2);
        assert_eq!(result.unwrap().value, "a title");
    }

    #[tokio::test(start_paused = true)]
    async fn a_permanent_error_is_not_retried() {
        let (calls, result) = run(|_| Ok(failed("authentication_error: bad key"))).await;
        assert_eq!(calls, 1);
        assert_eq!(
            result.unwrap_err(),
            Retried {
                value: OneshotError::Stream("authentication_error: bad key".into()),
                retries: 0
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retries_stop_after_the_limit() {
        let (calls, result) = run(|_| Ok(failed("Overloaded"))).await;
        assert_eq!(calls, MAX_RETRIES + 1);
        assert_eq!(result.unwrap_err().retries, MAX_RETRIES);
    }
}
