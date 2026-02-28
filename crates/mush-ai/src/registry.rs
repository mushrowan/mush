//! api provider registry
//!
//! providers register streaming functions keyed by api type.
//! models reference which api they use, and the registry resolves
//! the right provider at call time.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use crate::types::{Api, Model, StreamOptions};
use crate::stream::StreamEvent;

/// a boxed stream of events from an LLM provider
pub type EventStream = Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>;

/// a boxed future that returns an event stream (allows async setup)
pub type StreamResult = Pin<Box<dyn Future<Output = Result<EventStream, ProviderError>> + Send>>;

/// errors from provider operations
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ProviderError {
    #[error("no provider registered for api: {0:?}")]
    #[diagnostic(help("register a provider for this api type before streaming"))]
    NoProvider(Api),

    #[error("missing api key for provider: {0}")]
    #[diagnostic(help("set the appropriate env var or pass an api key in options"))]
    MissingApiKey(String),

    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("{0}")]
    Other(String),
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

    fn stream(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> StreamResult;
}

/// registry holding all available api providers
pub struct ApiRegistry {
    providers: HashMap<Api, Box<dyn ApiProvider>>,
}

impl ApiRegistry {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn register(&mut self, provider: Box<dyn ApiProvider>) {
        let api = provider.api();
        self.providers.insert(api, provider);
    }

    pub fn get(&self, api: Api) -> Option<&dyn ApiProvider> {
        self.providers.get(&api).map(|p| p.as_ref())
    }

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

impl Default for ApiRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            context_window: 200_000,
            max_output_tokens: 8192,
        };
        let ctx = LlmContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        };
        let err = registry.stream(&model, &ctx, &StreamOptions::default());
        assert!(err.is_err());
    }
}
