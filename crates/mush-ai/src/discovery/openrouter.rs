//! openrouter /api/v1/models discovery
//!
//! openrouter exposes a richer schema than first-party providers - including
//! pricing - so this is the only provider where discovery alone gives us
//! a fully-populated [`ModelCost`].
//!
//! example response shape (trimmed):
//!
//! ```json
//! {
//!   "data": [{
//!     "id": "anthropic/claude-sonnet-4",
//!     "name": "Anthropic: Claude Sonnet 4",
//!     "context_length": 1000000,
//!     "architecture": {
//!       "input_modalities": ["image", "text"],
//!       "output_modalities": ["text"]
//!     },
//!     "pricing": {
//!       "prompt": "0.000003",
//!       "completion": "0.000015",
//!       "input_cache_read": "0.0000003",
//!       "input_cache_write": "0.00000375"
//!     },
//!     "top_provider": {
//!       "context_length": 1000000,
//!       "max_completion_tokens": 64000
//!     },
//!     "supported_parameters": ["reasoning", "tools", ...]
//!   }]
//! }
//! ```
//!
//! pricing fields are dollars-per-token strings. mush stores cost as
//! dollars-per-million-tokens, so we multiply by `1_000_000`.

use std::time::SystemTime;

use serde::Deserialize;

use super::{DiscoveredModel, DiscoveryError, DiscoveryReport, ModelDiscovery};
use crate::types::{Api, ApiKey, InputModality, Model, ModelCost, Provider, TokenCount};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;
const DEFAULT_MAX_OUTPUT: u64 = 16_384;

/// openrouter discovery client
pub struct OpenRouterDiscovery {
    client: reqwest::Client,
    base_url: String,
    /// optional - openrouter `/models` works unauthenticated, but auth
    /// gives access to private/preview models tied to the account
    api_key: Option<ApiKey>,
}

impl OpenRouterDiscovery {
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        base_url: impl Into<String>,
        api_key: Option<ApiKey>,
    ) -> Self {
        Self {
            client,
            base_url: base_url.into(),
            api_key,
        }
    }
}

impl ModelDiscovery for OpenRouterDiscovery {
    fn provider(&self) -> Provider {
        Provider::OpenRouter
    }

    async fn fetch(&self) -> Result<DiscoveryReport, DiscoveryError> {
        let base = if self.base_url.is_empty() {
            DEFAULT_BASE_URL
        } else {
            &self.base_url
        };
        let url = format!("{base}/models");

        let mut req = self.client.get(&url);
        if let Some(key) = &self.api_key {
            req = req.header("authorization", format!("Bearer {}", key.expose()));
        }

        let body = super::execute_request(req).await?;
        let models = parse_openrouter_models(&body)?;
        Ok(DiscoveryReport {
            provider: Provider::OpenRouter,
            fetched_at: SystemTime::now(),
            models,
        })
    }
}

/// parse the raw response body into [`DiscoveredModel`] entries.
pub fn parse_openrouter_models(body: &str) -> Result<Vec<DiscoveredModel>, DiscoveryError> {
    let response: OpenRouterModelsResponse =
        serde_json::from_str(body).map_err(|e| DiscoveryError::Malformed(e.to_string()))?;
    response
        .data
        .into_iter()
        .map(|raw| {
            let entry: OpenRouterModelEntry = serde_json::from_value(raw.clone())
                .map_err(|e| DiscoveryError::Malformed(e.to_string()))?;
            Ok(DiscoveredModel {
                model: entry_to_model(entry),
                raw: Some(raw),
            })
        })
        .collect()
}

fn entry_to_model(entry: OpenRouterModelEntry) -> Model {
    let arch = entry.architecture.unwrap_or_default();
    let mut input = vec![InputModality::Text];
    if arch.input_modalities.iter().any(|m| m == "image") {
        input.push(InputModality::Image);
    }

    let supported = entry.supported_parameters.unwrap_or_default();
    let reasoning = supported
        .iter()
        .any(|p| p == "reasoning" || p == "include_reasoning");

    let context_window = entry
        .top_provider
        .as_ref()
        .and_then(|tp| tp.context_length)
        .or(entry.context_length)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let max_output = entry
        .top_provider
        .as_ref()
        .and_then(|tp| tp.max_completion_tokens)
        .unwrap_or(DEFAULT_MAX_OUTPUT);

    let cost = entry.pricing.map(pricing_to_cost).unwrap_or(ModelCost {
        input: 0.0,
        output: 0.0,
        cache_read: 0.0,
        cache_write: 0.0,
    });

    let name = entry
        .name
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| entry.id.clone());

    Model {
        id: entry.id.into(),
        name,
        api: Api::OpenaiCompletions,
        provider: Provider::OpenRouter,
        base_url: DEFAULT_BASE_URL.into(),
        reasoning,
        input,
        cost,
        context_window: TokenCount::new(context_window),
        max_output_tokens: TokenCount::new(max_output),
        supports_adaptive_thinking: false,
    }
}

/// convert openrouter's `dollars-per-token` strings to mush's `dollars-per-million-tokens` floats.
/// missing or unparseable fields default to zero (free / unknown).
fn pricing_to_cost(pricing: OpenRouterPricing) -> ModelCost {
    let parse = |s: Option<String>| -> f64 {
        s.and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0) * 1_000_000.0
    };
    ModelCost {
        input: parse(pricing.prompt),
        output: parse(pricing.completion),
        cache_read: parse(pricing.input_cache_read),
        cache_write: parse(pricing.input_cache_write),
    }
}

#[derive(Deserialize, Debug)]
struct OpenRouterModelsResponse {
    data: Vec<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct OpenRouterModelEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    architecture: Option<OpenRouterArchitecture>,
    #[serde(default)]
    pricing: Option<OpenRouterPricing>,
    #[serde(default)]
    top_provider: Option<OpenRouterTopProvider>,
    #[serde(default)]
    supported_parameters: Option<Vec<String>>,
}

#[derive(Deserialize, Debug, Default)]
struct OpenRouterArchitecture {
    #[serde(default)]
    input_modalities: Vec<String>,
}

#[derive(Deserialize, Debug, Default)]
struct OpenRouterPricing {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
    #[serde(default)]
    input_cache_read: Option<String>,
    #[serde(default)]
    input_cache_write: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct OpenRouterTopProvider {
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_completion_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// real openrouter response excerpt covering pricing, modalities,
    /// supported_parameters, top_provider, and the ones-with-missing-pieces
    /// case (free model with no pricing field).
    const FIXTURE: &str = r#"{
      "data": [
        {
          "id": "anthropic/claude-sonnet-4",
          "name": "Anthropic: Claude Sonnet 4",
          "context_length": 1000000,
          "architecture": {
            "input_modalities": ["image", "text"],
            "output_modalities": ["text"]
          },
          "pricing": {
            "prompt": "0.000003",
            "completion": "0.000015",
            "input_cache_read": "0.0000003",
            "input_cache_write": "0.00000375"
          },
          "top_provider": {
            "context_length": 1000000,
            "max_completion_tokens": 64000
          },
          "supported_parameters": ["reasoning", "tools", "temperature"]
        },
        {
          "id": "openrouter/auto",
          "name": "",
          "context_length": 200000,
          "architecture": {
            "input_modalities": ["text"],
            "output_modalities": ["text"]
          },
          "supported_parameters": ["temperature"]
        }
      ]
    }"#;

    #[test]
    fn parses_full_fixture() {
        let models = parse_openrouter_models(FIXTURE).unwrap();
        assert_eq!(models.len(), 2);

        let sonnet = &models[0].model;
        assert_eq!(sonnet.id.as_str(), "anthropic/claude-sonnet-4");
        assert_eq!(sonnet.name, "Anthropic: Claude Sonnet 4");
        assert_eq!(sonnet.api, Api::OpenaiCompletions);
        assert_eq!(sonnet.provider, Provider::OpenRouter);
        assert_eq!(sonnet.context_window, TokenCount::new(1_000_000));
        assert_eq!(sonnet.max_output_tokens, TokenCount::new(64_000));
        assert!(sonnet.reasoning);
        assert!(sonnet.input.contains(&InputModality::Image));
    }

    #[test]
    fn pricing_converted_to_per_million_tokens() {
        let models = parse_openrouter_models(FIXTURE).unwrap();
        let sonnet = &models[0].model;
        // 0.000003 dollars-per-token = 3.0 dollars-per-million
        assert!((sonnet.cost.input - 3.0).abs() < 0.001);
        assert!((sonnet.cost.output - 15.0).abs() < 0.001);
        assert!((sonnet.cost.cache_read - 0.3).abs() < 0.001);
        assert!((sonnet.cost.cache_write - 3.75).abs() < 0.001);
    }

    #[test]
    fn missing_pricing_yields_zero_cost() {
        let models = parse_openrouter_models(FIXTURE).unwrap();
        let auto = models
            .iter()
            .find(|m| m.model.id.as_str() == "openrouter/auto")
            .unwrap();
        assert_eq!(auto.model.cost.input, 0.0);
        assert_eq!(auto.model.cost.output, 0.0);
    }

    #[test]
    fn empty_name_falls_back_to_id() {
        let models = parse_openrouter_models(FIXTURE).unwrap();
        let auto = models
            .iter()
            .find(|m| m.model.id.as_str() == "openrouter/auto")
            .unwrap();
        assert_eq!(auto.model.name, "openrouter/auto");
    }

    #[test]
    fn text_only_models_dont_advertise_image_input() {
        let models = parse_openrouter_models(FIXTURE).unwrap();
        let auto = models
            .iter()
            .find(|m| m.model.id.as_str() == "openrouter/auto")
            .unwrap();
        assert!(!auto.model.input.contains(&InputModality::Image));
    }

    #[test]
    fn reasoning_supported_parameter_marks_model_reasoning() {
        let models = parse_openrouter_models(FIXTURE).unwrap();
        let sonnet = &models[0].model;
        let auto = models
            .iter()
            .find(|m| m.model.id.as_str() == "openrouter/auto")
            .unwrap();
        assert!(sonnet.reasoning);
        assert!(!auto.model.reasoning);
    }

    #[test]
    fn malformed_pricing_string_falls_back_to_zero() {
        let body = r#"{
          "data": [{
            "id": "x",
            "pricing": {"prompt": "not-a-number", "completion": "1.0"}
          }]
        }"#;
        let models = parse_openrouter_models(body).unwrap();
        assert_eq!(models[0].model.cost.input, 0.0);
        assert!((models[0].model.cost.output - 1_000_000.0).abs() < 0.001);
    }

    #[test]
    fn parser_preserves_raw_entry_json() {
        // pricing is in the raw blob too; consumers can re-derive
        // anything they want without going back to the upstream
        let models = parse_openrouter_models(FIXTURE).unwrap();
        let raw = models[0].raw.as_ref().expect("raw must be populated");
        assert_eq!(raw["id"], "anthropic/claude-sonnet-4");
        assert_eq!(raw["pricing"]["prompt"], "0.000003");
    }

    #[test]
    fn malformed_json_returns_error() {
        let err = parse_openrouter_models("not json").unwrap_err();
        assert!(matches!(err, DiscoveryError::Malformed(_)));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let body = r#"{
          "data": [{
            "id": "test",
            "future_field": "whatever",
            "architecture": {"input_modalities": ["text"], "extra": 1}
          }]
        }"#;
        let models = parse_openrouter_models(body).unwrap();
        assert_eq!(models.len(), 1);
    }
}
