//! api provider registry
//!
//! providers register streaming functions keyed by api type.
//! models reference which api they use, and the registry resolves
//! the right provider at call time.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use crate::stream::StreamEvent;
use crate::types::{Api, Model, StreamOptions};

/// a boxed stream of events from an LLM provider
pub type EventStream = Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>;

/// errors from provider operations
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum ProviderError {
    #[error("no provider registered for api: {0:?}")]
    #[diagnostic(help("register a provider for this api type before streaming"))]
    NoProvider(Api),

    #[error("missing api key for provider: {0}")]
    #[diagnostic(help("set the appropriate env var or pass an api key in options"))]
    MissingApiKey(crate::types::Provider),

    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("request middleware failed: {0}")]
    Middleware(String),

    #[error("invalid header value: {0}")]
    InvalidHeader(#[from] reqwest::header::InvalidHeaderValue),

    #[error("{api} returned {status} ({content_type}): {body}")]
    #[diagnostic(help("check your api key and model id"))]
    ApiError {
        api: &'static str,
        status: reqwest::StatusCode,
        content_type: String,
        body: String,
    },

    #[error("{0}")]
    Other(String),
}

impl From<reqwest_middleware::Error> for ProviderError {
    fn from(err: reqwest_middleware::Error) -> Self {
        match err {
            reqwest_middleware::Error::Reqwest(e) => Self::Request(e),
            reqwest_middleware::Error::Middleware(e) => Self::Middleware(e.to_string()),
        }
    }
}

/// max bytes of API error body to include in the error message
const MAX_ERROR_BODY: usize = 2000;

/// truncate an error body for diagnostic display
pub fn truncate_error_body(body: &str) -> String {
    if body.len() <= MAX_ERROR_BODY {
        body.to_string()
    } else {
        format!(
            "{}... ({} bytes total)",
            &body[..body.floor_char_boundary(MAX_ERROR_BODY)],
            body.len()
        )
    }
}

impl ProviderError {
    /// whether this error is transient and worth retrying
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Request(e) => {
                // network errors, timeouts, connection resets
                e.is_timeout() || e.is_connect() || e.is_request()
            }
            Self::ApiError { status, .. } => {
                // rate limit, server errors, overloaded
                status.as_u16() == 429 || status.as_u16() >= 500
            }
            _ => false,
        }
    }
}

/// whether an `AssistantMessage::error_message` from a stream that ended
/// with `StopReason::Error` (no http error, but the provider surfaced a
/// transient failure mid-stream) is worth retrying. covers anthropic's
/// `overloaded_error` SSE event, openai's `rate_limit_exceeded`, and
/// the generic upstream-overload patterns providers use when the
/// connection accepted (200) before the failure surfaced. matched
/// case-insensitively as a substring scan because each provider
/// formats the surfaced message slightly differently
#[must_use]
pub fn is_retryable_stream_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "overloaded",
        "rate_limit",
        "rate limit",
        "internal_server_error",
        "internal server error",
        "service_unavailable",
        "service unavailable",
        "timeout",
        "temporarily unavailable",
    ]
    .iter()
    .any(|pat| lower.contains(pat))
}

/// context passed to providers for each LLM call
pub struct LlmContext {
    pub system_prompt: Option<String>,
    pub messages: Vec<crate::types::Message>,
    pub tools: Vec<ToolDefinition>,
}

/// minimal tool definition for the provider layer
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// trait that LLM api providers implement
#[async_trait::async_trait]
pub trait ApiProvider: Send + Sync {
    fn api(&self) -> Api;

    async fn stream(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> Result<EventStream, ProviderError>;
}

/// registry holding all available api providers
#[derive(Default, Clone)]
pub struct ApiRegistry {
    providers: HashMap<Api, Arc<dyn ApiProvider>>,
}

impl ApiRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Box<dyn ApiProvider>) {
        let api = provider.api();
        self.providers.insert(api, Arc::from(provider));
    }

    pub fn get(&self, api: Api) -> Option<&dyn ApiProvider> {
        self.providers.get(&api).map(|p| p.as_ref())
    }

    #[tracing::instrument(name = "llm_stream", skip_all, fields(model = %model.id, api = ?model.api))]
    pub async fn stream(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> Result<EventStream, ProviderError> {
        let provider = self
            .get(model.api)
            .ok_or(ProviderError::NoProvider(model.api))?;
        provider.stream(model, context, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TokenCount;

    #[test]
    fn empty_registry_returns_none() {
        let registry = ApiRegistry::new();
        assert!(registry.get(Api::AnthropicMessages).is_none());
    }

    #[tokio::test]
    async fn stream_without_provider_returns_error() {
        let registry = ApiRegistry::new();
        let model = crate::types::Model {
            id: "test".into(),
            name: "test".into(),
            api: Api::AnthropicMessages,
            provider: crate::types::Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: false,
            input: vec![crate::types::InputModality::Text],
            cost: crate::types::ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(200_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(8192),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        };
        let ctx = LlmContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        };
        let err = registry
            .stream(&model, &ctx, &StreamOptions::default())
            .await;
        assert!(err.is_err());
    }

    #[test]
    fn retryable_429_rate_limit() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            content_type: "application/json".into(),
            body: "rate limited".into(),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn retryable_500_server_error() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            content_type: "application/json".into(),
            body: "internal error".into(),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn retryable_502_bad_gateway() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::BAD_GATEWAY,
            content_type: "text/html".into(),
            body: "bad gateway".into(),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn retryable_503_service_unavailable() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
            content_type: "text/plain".into(),
            body: "unavailable".into(),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn not_retryable_400_bad_request() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::BAD_REQUEST,
            content_type: "application/json".into(),
            body: "bad request".into(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn not_retryable_401_unauthorized() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::UNAUTHORIZED,
            content_type: "application/json".into(),
            body: "unauthorized".into(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn not_retryable_403_forbidden() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::FORBIDDEN,
            content_type: "application/json".into(),
            body: "forbidden".into(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn truncate_error_body_short() {
        let body = "short error";
        assert_eq!(truncate_error_body(body), "short error");
    }

    #[test]
    fn truncate_error_body_long() {
        let body = "x".repeat(5000);
        let truncated = truncate_error_body(&body);
        assert!(truncated.len() < body.len());
        assert!(truncated.contains("5000 bytes total"));
    }

    #[test]
    fn truncate_error_body_cuts_a_multibyte_body_on_a_char_boundary() {
        let body = "…".repeat(MAX_ERROR_BODY);
        let truncated = truncate_error_body(&body);
        assert!(truncated.starts_with('…'));
        assert!(truncated.contains(&format!("{} bytes total", body.len())));
    }

    #[test]
    fn not_retryable_no_provider() {
        let err = ProviderError::NoProvider(Api::AnthropicMessages);
        assert!(!err.is_retryable());
    }

    #[test]
    fn not_retryable_missing_key() {
        let err = ProviderError::MissingApiKey(crate::types::Provider::Anthropic);
        assert!(!err.is_retryable());
    }

    #[test]
    fn not_retryable_other() {
        let err = ProviderError::Other("some error".into());
        assert!(!err.is_retryable());
    }

    #[test]
    fn retryable_stream_error_messages() {
        // anthropic mid-stream overloaded error
        assert!(is_retryable_stream_error_message("Overloaded"));
        // openai-style snake_case
        assert!(is_retryable_stream_error_message(
            "rate_limit_exceeded: please slow down"
        ));
        // human-readable rate limit
        assert!(is_retryable_stream_error_message("rate limit reached"));
        // 5xx surfaced as text
        assert!(is_retryable_stream_error_message(
            "internal_server_error from upstream"
        ));
        assert!(is_retryable_stream_error_message("Service Unavailable"));
        assert!(is_retryable_stream_error_message("Request timeout"));
        // case-insensitive
        assert!(is_retryable_stream_error_message("OVERLOADED"));
    }

    #[test]
    fn non_retryable_stream_error_messages() {
        assert!(!is_retryable_stream_error_message(
            "invalid_request_error: missing field"
        ));
        assert!(!is_retryable_stream_error_message(
            "authentication_error: bad key"
        ));
        assert!(!is_retryable_stream_error_message(
            "permission_denied for this model"
        ));
        assert!(!is_retryable_stream_error_message(""));
    }
}
