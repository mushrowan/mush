//! api provider registry
//!
//! providers register streaming functions keyed by api type.
//! models reference which api they use, and the registry resolves
//! the right provider at call time.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::stream::StreamEvent;
use crate::types::{Api, Model, StreamOptions};

/// a boxed stream of events from an LLM provider
pub type EventStream = Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>;

/// a boxed future that returns an event stream (allows async setup)
pub type StreamResult = Pin<Box<dyn Future<Output = Result<EventStream, ProviderError>> + Send>>;

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

/// max bytes of API error body to include in the error message
const MAX_ERROR_BODY: usize = 2000;

/// truncate an error body for diagnostic display
pub fn truncate_error_body(body: &str) -> String {
    if body.len() <= MAX_ERROR_BODY {
        body.to_string()
    } else {
        format!(
            "{}... ({} bytes total)",
            &body[..MAX_ERROR_BODY],
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
pub trait ApiProvider: Send + Sync {
    fn api(&self) -> Api;

    fn stream(&self, model: &Model, context: &LlmContext, options: &StreamOptions) -> StreamResult;
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
    pub fn stream(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> Result<StreamResult, ProviderError> {
        let provider = self
            .get(model.api)
            .ok_or(ProviderError::NoProvider(model.api))?;
        Ok(provider.stream(model, context, options))
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

    #[test]
    fn stream_without_provider_returns_error() {
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
            max_output_tokens: TokenCount::new(8192),
        };
        let ctx = LlmContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        };
        let err = registry.stream(&model, &ctx, &StreamOptions::default());
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
}
