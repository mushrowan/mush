//! openai /v1/models discovery
//!
//! openai's public listing endpoint is the most minimal of the three:
//!
//! ```json
//! {
//!   "object": "list",
//!   "data": [
//!     {"id": "gpt-5", "created": 1700000000, "object": "model", "owned_by": "openai"},
//!     {"id": "text-embedding-3-large", ...}
//!   ]
//! }
//! ```
//!
//! we get only `id`, `created`, and `owned_by`. no context window, no
//! pricing, no reasoning hints. mush filters down to chat-capable
//! reasoning models (gpt-*, o-series, codex) and relies on the static
//! catalogue merge step (later commit) to backfill cost and limits.
//!
//! this provider corresponds to api-key access at `api.openai.com`.
//! the chatgpt-subscription path (codex) uses a different backend with
//! a richer response schema and lives in a separate module (todo).

use std::time::SystemTime;

use serde::Deserialize;

use super::{DiscoveredModel, DiscoveryError, DiscoveryReport, ModelDiscovery};
use crate::types::{Api, ApiKey, InputModality, Model, ModelCost, Provider, TokenCount};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// rough fallback - real values come from the static catalogue merge step
const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;
const DEFAULT_MAX_OUTPUT: u64 = 16_384;

/// openai api-key discovery client
pub struct OpenAiDiscovery {
    client: reqwest::Client,
    base_url: String,
    api_key: ApiKey,
}

impl OpenAiDiscovery {
    #[must_use]
    pub fn new(client: reqwest::Client, base_url: impl Into<String>, api_key: ApiKey) -> Self {
        Self {
            client,
            base_url: base_url.into(),
            api_key,
        }
    }
}

impl ModelDiscovery for OpenAiDiscovery {
    fn provider(&self) -> Provider {
        Provider::Custom("openai".into())
    }

    async fn fetch(&self) -> Result<DiscoveryReport, DiscoveryError> {
        let base = if self.base_url.is_empty() {
            DEFAULT_BASE_URL
        } else {
            &self.base_url
        };
        let url = format!("{base}/models");

        let req = self
            .client
            .get(&url)
            .header("authorization", format!("Bearer {}", self.api_key.expose()));
        let body = super::execute_request(req).await?;
        let models = parse_openai_models(&body)?;
        Ok(DiscoveryReport {
            provider: Provider::Custom("openai".into()),
            fetched_at: SystemTime::now(),
            models,
        })
    }
}

/// parse the raw response body into [`DiscoveredModel`] entries, filtering to
/// chat-capable models. embeddings, image, audio, and moderation models
/// are dropped because they're not usable from a coding-assistant TUI.
pub fn parse_openai_models(body: &str) -> Result<Vec<DiscoveredModel>, DiscoveryError> {
    let response: OpenAiModelsResponse =
        serde_json::from_str(body).map_err(|e| DiscoveryError::Malformed(e.to_string()))?;
    response
        .data
        .into_iter()
        .filter(|raw| {
            raw.get("id")
                .and_then(|v| v.as_str())
                .map(is_chat_capable)
                .unwrap_or(false)
        })
        .map(|raw| {
            let entry: OpenAiModelEntry = serde_json::from_value(raw.clone())
                .map_err(|e| DiscoveryError::Malformed(e.to_string()))?;
            Ok(DiscoveredModel {
                model: entry_to_model(entry),
                raw: Some(raw),
            })
        })
        .collect()
}

/// chat-capable model id heuristic.
///
/// - include: `gpt-`, `o1`, `o3`, `o4`, `chatgpt-`, `codex-`
/// - exclude: embeddings/image/audio/moderation models even if id begins with one of the above
fn is_chat_capable(id: &str) -> bool {
    let blocked = [
        "embedding",
        "embed-",
        "dall-e",
        "whisper",
        "tts-",
        "text-moderation",
        "omni-moderation",
        "image-",
        "audio-",
    ];
    if blocked.iter().any(|p| id.contains(p)) {
        return false;
    }
    let allowed_prefixes = ["gpt-", "o1", "o3", "o4", "chatgpt-", "codex-"];
    allowed_prefixes.iter().any(|p| id.starts_with(p))
}

fn entry_to_model(entry: OpenAiModelEntry) -> Model {
    let reasoning = entry.id.starts_with("o1")
        || entry.id.starts_with("o3")
        || entry.id.starts_with("o4")
        || entry.id.contains("codex");

    Model {
        id: entry.id.clone().into(),
        name: entry.id,
        api: Api::OpenaiResponses,
        provider: Provider::Custom("openai".into()),
        base_url: DEFAULT_BASE_URL.into(),
        reasoning,
        input: vec![InputModality::Text, InputModality::Image],
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        context_window: TokenCount::new(DEFAULT_CONTEXT_WINDOW),
        max_output_tokens: TokenCount::new(DEFAULT_MAX_OUTPUT),
        supports_adaptive_thinking: false,
    }
}

#[derive(Deserialize, Debug)]
struct OpenAiModelsResponse {
    data: Vec<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct OpenAiModelEntry {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// real-world-ish response covering chat models, reasoning models,
    /// and the embedding/audio/image entries we want to filter out.
    const FIXTURE: &str = r#"{
      "object": "list",
      "data": [
        {"id": "gpt-5",                  "object": "model", "created": 1730000000, "owned_by": "openai"},
        {"id": "gpt-5-mini",             "object": "model", "created": 1730000001, "owned_by": "openai"},
        {"id": "gpt-4o",                 "object": "model", "created": 1715000000, "owned_by": "openai"},
        {"id": "o3",                     "object": "model", "created": 1720000000, "owned_by": "openai"},
        {"id": "o4-mini",                "object": "model", "created": 1722000000, "owned_by": "openai"},
        {"id": "codex-mini-latest",      "object": "model", "created": 1740000000, "owned_by": "openai"},
        {"id": "text-embedding-3-large", "object": "model", "created": 1700000000, "owned_by": "openai"},
        {"id": "dall-e-3",               "object": "model", "created": 1700000000, "owned_by": "openai"},
        {"id": "whisper-1",              "object": "model", "created": 1700000000, "owned_by": "openai"},
        {"id": "tts-1",                  "object": "model", "created": 1700000000, "owned_by": "openai"},
        {"id": "omni-moderation-latest", "object": "model", "created": 1700000000, "owned_by": "openai"},
        {"id": "babbage-002",            "object": "model", "created": 1700000000, "owned_by": "openai"}
      ]
    }"#;

    #[test]
    fn filters_to_chat_capable_models() {
        let models = parse_openai_models(FIXTURE).unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.model.id.as_str()).collect();
        assert!(ids.contains(&"gpt-5"));
        assert!(ids.contains(&"gpt-5-mini"));
        assert!(ids.contains(&"gpt-4o"));
        assert!(ids.contains(&"o3"));
        assert!(ids.contains(&"o4-mini"));
        assert!(ids.contains(&"codex-mini-latest"));
    }

    #[test]
    fn excludes_embeddings_and_audio_and_image_models() {
        let models = parse_openai_models(FIXTURE).unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.model.id.as_str()).collect();
        assert!(!ids.contains(&"text-embedding-3-large"));
        assert!(!ids.contains(&"dall-e-3"));
        assert!(!ids.contains(&"whisper-1"));
        assert!(!ids.contains(&"tts-1"));
        assert!(!ids.contains(&"omni-moderation-latest"));
        assert!(!ids.contains(&"babbage-002"));
    }

    #[test]
    fn o_series_and_codex_models_marked_reasoning() {
        let models = parse_openai_models(FIXTURE).unwrap();
        let by_id: std::collections::HashMap<_, _> = models
            .iter()
            .map(|m| (m.model.id.as_str(), &m.model))
            .collect();
        assert!(by_id["o3"].reasoning);
        assert!(by_id["o4-mini"].reasoning);
        assert!(by_id["codex-mini-latest"].reasoning);
    }

    #[test]
    fn gpt5_and_gpt4o_default_to_non_reasoning() {
        // mush's reasoning catalogue is overlaid by the static merge step,
        // which knows gpt-5 reasons. discovery on its own can't tell from
        // an id alone whether plain `gpt-5` reasons - so we stay conservative
        let models = parse_openai_models(FIXTURE).unwrap();
        let by_id: std::collections::HashMap<_, _> = models
            .iter()
            .map(|m| (m.model.id.as_str(), &m.model))
            .collect();
        assert!(!by_id["gpt-5"].reasoning);
        assert!(!by_id["gpt-4o"].reasoning);
    }

    #[test]
    fn malformed_json_returns_error() {
        let err = parse_openai_models("not json").unwrap_err();
        assert!(matches!(err, DiscoveryError::Malformed(_)));
    }

    #[test]
    fn provider_id_is_custom_openai() {
        let models = parse_openai_models(FIXTURE).unwrap();
        for m in &models {
            assert_eq!(m.model.provider, Provider::Custom("openai".into()));
            assert_eq!(m.model.api, Api::OpenaiResponses);
        }
    }

    #[test]
    fn parser_preserves_raw_entry_json() {
        let models = parse_openai_models(FIXTURE).unwrap();
        let gpt5 = models
            .iter()
            .find(|m| m.model.id.as_str() == "gpt-5")
            .unwrap();
        let raw = gpt5.raw.as_ref().expect("raw must be populated");
        assert_eq!(raw["id"], "gpt-5");
        assert_eq!(raw["owned_by"], "openai");
    }
}
