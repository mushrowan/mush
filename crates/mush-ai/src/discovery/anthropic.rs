//! anthropic /v1/models discovery
//!
//! parses the response shape documented at
//! <https://docs.anthropic.com/en/api/models-list>:
//!
//! ```json
//! {
//!   "data": [
//!     {
//!       "type": "model",
//!       "id": "claude-opus-4-7",
//!       "display_name": "Claude Opus 4.7",
//!       "created_at": "2025-...",
//!       "max_input_tokens": 1000000,
//!       "max_tokens": 128000,
//!       "capabilities": {
//!         "thinking": {
//!           "types": {
//!             "adaptive": {"supported": true},
//!             "enabled":  {"supported": true}
//!           }
//!         },
//!         "image_input": {"supported": true}
//!       }
//!     }
//!   ],
//!   "has_more": false
//! }
//! ```
//!
//! anthropic doesn't return pricing, so [`ModelCost`] is filled with zeros.
//! the merge step (later commit) backfills costs from the static catalogue
//! when the model id matches a known one.

use std::time::SystemTime;

use serde::Deserialize;

use super::{DiscoveredModel, DiscoveryError, DiscoveryReport, ModelDiscovery};
use crate::types::{Api, ApiKey, InputModality, Model, ModelCost, Provider, TokenCount};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
const DEFAULT_MAX_OUTPUT: u64 = 64_000;

/// anthropic discovery client
pub struct AnthropicDiscovery {
    client: reqwest::Client,
    base_url: String,
    api_key: ApiKey,
}

impl AnthropicDiscovery {
    #[must_use]
    pub fn new(client: reqwest::Client, base_url: impl Into<String>, api_key: ApiKey) -> Self {
        Self {
            client,
            base_url: base_url.into(),
            api_key,
        }
    }
}

impl ModelDiscovery for AnthropicDiscovery {
    fn provider(&self) -> Provider {
        Provider::Anthropic
    }

    async fn fetch(&self) -> Result<DiscoveryReport, DiscoveryError> {
        let base = if self.base_url.is_empty() {
            DEFAULT_BASE_URL
        } else {
            &self.base_url
        };
        let url = format!("{base}/v1/models?limit=1000");

        let mut req = self
            .client
            .get(&url)
            .header("anthropic-version", API_VERSION);
        let key = self.api_key.expose();
        if self.api_key.is_oauth_token() {
            req = req.header("authorization", format!("Bearer {key}"));
        } else {
            req = req.header("x-api-key", key);
        }

        let body = super::execute_request(req).await?;
        let models = parse_anthropic_models(&body)?;
        Ok(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: SystemTime::now(),
            models,
        })
    }
}

/// parse the raw response body into [`DiscoveredModel`] entries.
///
/// pure function, no I/O — the entry point used by tests and by [`AnthropicDiscovery::fetch`].
/// each entry's verbatim JSON is preserved on `DiscoveredModel::raw` so future code can
/// access fields we don't promote to [`Model`] (e.g. `capabilities.pdf_input`).
pub fn parse_anthropic_models(body: &str) -> Result<Vec<DiscoveredModel>, DiscoveryError> {
    let response: AnthropicModelsResponse =
        serde_json::from_str(body).map_err(|e| DiscoveryError::Malformed(e.to_string()))?;
    response
        .data
        .into_iter()
        .map(|raw| {
            let entry: AnthropicModelEntry = serde_json::from_value(raw.clone())
                .map_err(|e| DiscoveryError::Malformed(e.to_string()))?;
            Ok(DiscoveredModel {
                model: entry_to_model(entry),
                raw: Some(raw),
            })
        })
        .collect()
}

fn entry_to_model(entry: AnthropicModelEntry) -> Model {
    let caps = entry.capabilities.unwrap_or_default();
    let thinking = caps.thinking.unwrap_or_default();
    let supports_adaptive_thinking = thinking.types.adaptive.unwrap_or_default().supported;
    let reasoning = supports_adaptive_thinking
        || thinking.types.enabled.unwrap_or_default().supported
        || thinking.types.budget.unwrap_or_default().supported;

    let mut input = vec![InputModality::Text];
    if caps.image_input.unwrap_or_default().supported {
        input.push(InputModality::Image);
    }

    let display_name = entry
        .display_name
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| entry.id.clone());

    Model {
        id: entry.id.into(),
        name: display_name,
        api: Api::AnthropicMessages,
        provider: Provider::Anthropic,
        base_url: DEFAULT_BASE_URL.into(),
        reasoning,
        input,
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        context_window: TokenCount::new(entry.max_input_tokens.unwrap_or(DEFAULT_CONTEXT_WINDOW)),
        max_output_tokens: TokenCount::new(entry.max_tokens.unwrap_or(DEFAULT_MAX_OUTPUT)),
        supports_adaptive_thinking,
        supported_thinking_levels: Vec::new(),
        default_thinking_level: None,
    }
}

#[derive(Deserialize, Debug)]
struct AnthropicModelsResponse {
    data: Vec<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct AnthropicModelEntry {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    max_input_tokens: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    capabilities: Option<AnthropicCapabilities>,
}

#[derive(Deserialize, Debug, Default)]
struct AnthropicCapabilities {
    #[serde(default)]
    thinking: Option<AnthropicThinkingCaps>,
    #[serde(default)]
    image_input: Option<SupportedFlag>,
}

#[derive(Deserialize, Debug, Default)]
struct AnthropicThinkingCaps {
    #[serde(default)]
    types: AnthropicThinkingTypes,
}

#[derive(Deserialize, Debug, Default)]
struct AnthropicThinkingTypes {
    #[serde(default)]
    adaptive: Option<SupportedFlag>,
    #[serde(default)]
    enabled: Option<SupportedFlag>,
    #[serde(default)]
    budget: Option<SupportedFlag>,
}

#[derive(Deserialize, Debug, Default, Clone, Copy)]
struct SupportedFlag {
    #[serde(default)]
    supported: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// canonical fixture that exercises every field we extract.
    /// id, display_name, max_input_tokens, max_tokens, capabilities.thinking.adaptive,
    /// capabilities.image_input. plus an entry without capabilities to test defaults.
    const FULL_FIXTURE: &str = r#"{
      "data": [
        {
          "type": "model",
          "id": "claude-opus-4-7",
          "display_name": "Claude Opus 4.7",
          "created_at": "2025-09-01T00:00:00Z",
          "max_input_tokens": 1000000,
          "max_tokens": 128000,
          "capabilities": {
            "thinking": {
              "types": {
                "adaptive": {"supported": true},
                "enabled":  {"supported": true}
              }
            },
            "image_input": {"supported": true}
          }
        },
        {
          "type": "model",
          "id": "claude-haiku-old",
          "display_name": "",
          "created_at": "2024-01-01T00:00:00Z"
        }
      ],
      "first_id": "claude-opus-4-7",
      "last_id": "claude-haiku-old",
      "has_more": false
    }"#;

    #[test]
    fn parses_full_fixture() {
        let models = parse_anthropic_models(FULL_FIXTURE).unwrap();
        assert_eq!(models.len(), 2);

        let opus = &models[0].model;
        assert_eq!(opus.id.as_str(), "claude-opus-4-7");
        assert_eq!(opus.name, "Claude Opus 4.7");
        assert_eq!(opus.api, Api::AnthropicMessages);
        assert_eq!(opus.provider, Provider::Anthropic);
        assert_eq!(opus.context_window, TokenCount::new(1_000_000));
        assert_eq!(opus.max_output_tokens, TokenCount::new(128_000));
        assert!(opus.reasoning);
        assert!(opus.supports_adaptive_thinking);
        assert!(opus.input.contains(&InputModality::Image));
    }

    #[test]
    fn empty_display_name_falls_back_to_id() {
        let models = parse_anthropic_models(FULL_FIXTURE).unwrap();
        let haiku = models
            .iter()
            .find(|m| m.model.id.as_str() == "claude-haiku-old")
            .unwrap();
        assert_eq!(haiku.model.name, "claude-haiku-old");
    }

    #[test]
    fn missing_capabilities_yields_safe_defaults() {
        let models = parse_anthropic_models(FULL_FIXTURE).unwrap();
        let haiku = models
            .iter()
            .find(|m| m.model.id.as_str() == "claude-haiku-old")
            .unwrap();
        assert!(!haiku.model.reasoning);
        assert!(!haiku.model.supports_adaptive_thinking);
        assert_eq!(haiku.model.input, vec![InputModality::Text]);
        assert_eq!(
            haiku.model.context_window,
            TokenCount::new(DEFAULT_CONTEXT_WINDOW)
        );
        assert_eq!(
            haiku.model.max_output_tokens,
            TokenCount::new(DEFAULT_MAX_OUTPUT)
        );
    }

    #[test]
    fn cost_defaults_to_zero_for_anthropic() {
        // anthropic doesn't return pricing in /v1/models; static merge fills it later
        let models = parse_anthropic_models(FULL_FIXTURE).unwrap();
        for m in &models {
            assert_eq!(m.model.cost.input, 0.0);
            assert_eq!(m.model.cost.output, 0.0);
            assert_eq!(m.model.cost.cache_read, 0.0);
            assert_eq!(m.model.cost.cache_write, 0.0);
        }
    }

    #[test]
    fn malformed_json_returns_error() {
        let err = parse_anthropic_models("not json").unwrap_err();
        assert!(matches!(err, DiscoveryError::Malformed(_)));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // forward-compat: anthropic adds new fields, we shouldn't break
        let body = r#"{
          "data": [{
            "id": "claude-test",
            "future_field": "whatever",
            "capabilities": {"unknown": {"supported": true}}
          }],
          "has_more": false
        }"#;
        let models = parse_anthropic_models(body).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model.id.as_str(), "claude-test");
    }

    #[test]
    fn enabled_only_thinking_marks_reasoning_but_not_adaptive() {
        // older models advertise enabled thinking only (budget mode)
        let body = r#"{
          "data": [{
            "id": "claude-old",
            "capabilities": {
              "thinking": {
                "types": { "enabled": {"supported": true} }
              }
            }
          }],
          "has_more": false
        }"#;
        let models = parse_anthropic_models(body).unwrap();
        assert!(models[0].model.reasoning);
        assert!(!models[0].model.supports_adaptive_thinking);
    }

    #[test]
    fn parser_preserves_raw_entry_json() {
        // option C: every discovered model carries the raw upstream JSON
        // so future code can extract richer per-provider fields without
        // a cache migration. anthropic's `capabilities.pdf_input` is the
        // canary case here - we don't promote it to Model, but it's
        // round-tripped intact.
        let body = r#"{
          "data": [{
            "id": "claude-test",
            "display_name": "Test",
            "capabilities": {
              "pdf_input": {"supported": true},
              "thinking": {"types": {"adaptive": {"supported": true}}}
            }
          }],
          "has_more": false
        }"#;
        let models = parse_anthropic_models(body).unwrap();
        let raw = models[0].raw.as_ref().expect("raw must be populated");
        assert_eq!(raw["id"], "claude-test");
        assert_eq!(raw["capabilities"]["pdf_input"]["supported"], true);
    }
}
