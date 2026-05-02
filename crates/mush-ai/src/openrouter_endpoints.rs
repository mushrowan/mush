//! openrouter `/models/{author}/{slug}/endpoints` lookup
//!
//! returns the per-provider endpoints serving a given model, with
//! supported_parameters telling us which providers handle explicit
//! cache control. drives the optional `/endpoints` slash command and
//! could later seed cache-timer state for openrouter-routed turns
//! before /generation reconciles.

use openrouter_rs::api::models::{Endpoint, EndpointData};

use crate::types::TokenCount;

/// distilled endpoint info for one provider serving a model. only
/// the fields mush actually consumes; if a future caller needs more,
/// extend rather than expose the SDK type
#[derive(Debug, Clone, PartialEq)]
pub struct ModelEndpoint {
    /// upstream provider that serves this endpoint, e.g. "Anthropic",
    /// "Amazon Bedrock", "Google Vertex". matches the case the SDK
    /// returns; downstream lookup canonicalises before comparison
    pub provider_name: String,
    /// max prompt+completion tokens the endpoint accepts
    pub context_length: TokenCount,
    /// whether the endpoint accepts the `cache_control` request
    /// parameter (anthropic-style explicit prompt caching)
    pub supports_cache_control: bool,
    /// raw supported_parameters list as returned by openrouter, kept
    /// for diagnostics and future feature detection without re-fetching
    pub supported_parameters: Vec<String>,
}

/// distilled endpoints response for a model. carries the model's
/// canonical id and name plus its serving endpoints
#[derive(Debug, Clone, PartialEq)]
pub struct ModelEndpoints {
    pub id: String,
    pub name: String,
    pub endpoints: Vec<ModelEndpoint>,
}

impl ModelEndpoints {
    /// the unique provider name when exactly one endpoint accepts
    /// explicit cache_control. used to seed the cache timer for
    /// openrouter routes whose upstream is unambiguous up-front;
    /// returns `None` for zero or multiple cache-capable endpoints
    /// (we want /generation reconciliation to make the call instead)
    #[must_use]
    pub fn unique_caching_provider(&self) -> Option<&str> {
        let mut iter = self
            .endpoints
            .iter()
            .filter(|e| e.supports_cache_control)
            .map(|e| e.provider_name.as_str());
        let first = iter.next()?;
        if iter.next().is_some() {
            None
        } else {
            Some(first)
        }
    }
}

/// translate the SDK's [`EndpointData`] into mush's distilled view.
/// kept separate from the network call so it's easy to fixture-test
pub fn convert_endpoints(data: &EndpointData) -> ModelEndpoints {
    ModelEndpoints {
        id: data.id.clone(),
        name: data.name.clone(),
        endpoints: data.endpoints.iter().map(convert_endpoint).collect(),
    }
}

fn convert_endpoint(e: &Endpoint) -> ModelEndpoint {
    let supports_cache_control = e
        .supported_parameters
        .iter()
        .any(|p| p.eq_ignore_ascii_case("cache_control"));
    ModelEndpoint {
        provider_name: e.provider_name.clone(),
        context_length: TokenCount::new(e.context_length.max(0.0) as u64),
        supports_cache_control,
        supported_parameters: e.supported_parameters.clone(),
    }
}

/// fetch the per-provider endpoint list for a model. `model_id` is
/// the openrouter slug, `"author/slug"` (e.g. `"anthropic/claude-sonnet-4.5"`).
/// surfaces parse failure as `InvalidModelId` so callers can fall back
/// gracefully without retrying
pub async fn fetch_endpoints(
    api_key: &str,
    model_id: &str,
) -> Result<ModelEndpoints, EndpointsLookupError> {
    use openrouter_rs::api::models::list_model_endpoints;
    const BASE_URL: &str = "https://openrouter.ai/api/v1";

    let (author, slug) = split_model_id(model_id)?;
    let data = list_model_endpoints(BASE_URL, api_key, author, slug)
        .await
        .map_err(|e| EndpointsLookupError::Sdk(e.to_string()))?;
    Ok(convert_endpoints(&data))
}

/// split `"author/slug"` into its two parts. openrouter slugs always
/// have exactly one `/`; anything else means we got a non-openrouter
/// model id and should bail rather than guess
fn split_model_id(id: &str) -> Result<(&str, &str), EndpointsLookupError> {
    let (author, rest) = id
        .split_once('/')
        .ok_or_else(|| EndpointsLookupError::InvalidModelId(id.to_owned()))?;
    if rest.contains('/') || author.is_empty() || rest.is_empty() {
        return Err(EndpointsLookupError::InvalidModelId(id.to_owned()));
    }
    Ok((author, rest))
}

#[derive(Debug, thiserror::Error)]
pub enum EndpointsLookupError {
    #[error("openrouter endpoints lookup failed: {0}")]
    Sdk(String),
    #[error("model id `{0}` is not in the openrouter `author/slug` form")]
    InvalidModelId(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// representative `/models/{author}/{slug}/endpoints` response.
    /// covers a model served by three providers: anthropic and vertex
    /// support `cache_control`, fireworks does not. mush should pick
    /// none of them as a unique caching provider since two qualify
    const FIXTURE_MULTI_CACHE: &str = r#"{
        "id": "anthropic/claude-sonnet-4.5",
        "name": "Claude Sonnet 4.5",
        "created": 1700000000.0,
        "description": "test",
        "architecture": {
            "input_modalities": ["text"],
            "output_modalities": ["text"],
            "tokenizer": "Claude",
            "instruct_type": null
        },
        "endpoints": [
            {
                "name": "Anthropic",
                "context_length": 200000.0,
                "pricing": {"prompt": "0.000003", "completion": "0.000015"},
                "provider_name": "Anthropic",
                "supported_parameters": ["temperature", "max_tokens", "cache_control", "tools"]
            },
            {
                "name": "Google Vertex",
                "context_length": 200000.0,
                "pricing": {"prompt": "0.000003", "completion": "0.000015"},
                "provider_name": "Google Vertex",
                "supported_parameters": ["temperature", "max_tokens", "cache_control"]
            },
            {
                "name": "Fireworks",
                "context_length": 128000.0,
                "pricing": {"prompt": "0.000002", "completion": "0.000010"},
                "provider_name": "Fireworks",
                "supported_parameters": ["temperature", "max_tokens"]
            }
        ]
    }"#;

    /// single caching provider variant: only anthropic supports
    /// `cache_control`. mush should pick anthropic as the unique
    /// caching provider so the cache timer seeds correctly without
    /// waiting for /generation
    const FIXTURE_SINGLE_CACHE: &str = r#"{
        "id": "anthropic/claude-haiku-4.5",
        "name": "Claude Haiku 4.5",
        "created": 1700000000.0,
        "description": "test",
        "architecture": {
            "input_modalities": ["text"],
            "output_modalities": ["text"],
            "tokenizer": "Claude",
            "instruct_type": null
        },
        "endpoints": [
            {
                "name": "Anthropic",
                "context_length": 200000.0,
                "pricing": {"prompt": "0.000001", "completion": "0.000005"},
                "provider_name": "Anthropic",
                "supported_parameters": ["temperature", "max_tokens", "cache_control"]
            },
            {
                "name": "Fireworks",
                "context_length": 128000.0,
                "pricing": {"prompt": "0.000001", "completion": "0.000005"},
                "provider_name": "Fireworks",
                "supported_parameters": ["temperature", "max_tokens"]
            }
        ]
    }"#;

    #[test]
    fn convert_endpoints_extracts_provider_names_and_cache_support() {
        let data: EndpointData = serde_json::from_str(FIXTURE_MULTI_CACHE).expect("parses");
        let endpoints = convert_endpoints(&data);
        assert_eq!(endpoints.id, "anthropic/claude-sonnet-4.5");
        assert_eq!(endpoints.endpoints.len(), 3);
        let names: Vec<&str> = endpoints
            .endpoints
            .iter()
            .map(|e| e.provider_name.as_str())
            .collect();
        assert_eq!(names, vec!["Anthropic", "Google Vertex", "Fireworks"]);
    }

    #[test]
    fn supports_cache_control_detected_via_supported_parameters() {
        let data: EndpointData = serde_json::from_str(FIXTURE_MULTI_CACHE).expect("parses");
        let endpoints = convert_endpoints(&data);
        let by_provider: std::collections::HashMap<&str, bool> = endpoints
            .endpoints
            .iter()
            .map(|e| (e.provider_name.as_str(), e.supports_cache_control))
            .collect();
        assert_eq!(by_provider.get("Anthropic"), Some(&true));
        assert_eq!(by_provider.get("Google Vertex"), Some(&true));
        assert_eq!(by_provider.get("Fireworks"), Some(&false));
    }

    #[test]
    fn unique_caching_provider_returns_none_when_multiple_qualify() {
        // anthropic and vertex both support cache_control - we don't
        // want to guess which of the two will serve the next turn,
        // so leave it to /generation reconciliation
        let data: EndpointData = serde_json::from_str(FIXTURE_MULTI_CACHE).expect("parses");
        let endpoints = convert_endpoints(&data);
        assert_eq!(endpoints.unique_caching_provider(), None);
    }

    #[test]
    fn unique_caching_provider_returns_the_sole_qualifier() {
        // single cache-capable endpoint means openrouter's sticky
        // routing will deterministically land here for a cached turn,
        // so we can seed up-front
        let data: EndpointData = serde_json::from_str(FIXTURE_SINGLE_CACHE).expect("parses");
        let endpoints = convert_endpoints(&data);
        assert_eq!(endpoints.unique_caching_provider(), Some("Anthropic"));
    }

    #[test]
    fn split_model_id_accepts_canonical_author_slug_form() {
        assert_eq!(
            split_model_id("anthropic/claude-sonnet-4.5").unwrap(),
            ("anthropic", "claude-sonnet-4.5")
        );
    }

    #[test]
    fn split_model_id_rejects_malformed_inputs() {
        // bare slug without author
        assert!(matches!(
            split_model_id("claude-sonnet-4.5"),
            Err(EndpointsLookupError::InvalidModelId(_))
        ));
        // too many segments
        assert!(matches!(
            split_model_id("anthropic/claude/sonnet"),
            Err(EndpointsLookupError::InvalidModelId(_))
        ));
        // empty halves
        assert!(matches!(
            split_model_id("/claude"),
            Err(EndpointsLookupError::InvalidModelId(_))
        ));
        assert!(matches!(
            split_model_id("anthropic/"),
            Err(EndpointsLookupError::InvalidModelId(_))
        ));
    }
}
