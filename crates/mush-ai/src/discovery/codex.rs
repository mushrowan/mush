//! codex (chatgpt subscription) `/models` discovery
//!
//! codex's backend exposes a richer schema than the public `/v1/models`
//! endpoint - same as anthropic plus reasoning levels, verbosity modes,
//! truncation policy, and a hand-curated `display_name`/`description`
//! pair. mush picks the cross-cutting fields to populate [`Model`] and
//! preserves the original entry on `DiscoveredModel::raw` so a typed
//! accessor (added in a follow-up commit) can extract codex-specific
//! extras lazily.
//!
//! request:
//! ```text
//! GET https://chatgpt.com/backend-api/codex/models?client_version=<v>
//! Authorization: Bearer <chatgpt access token>
//! chatgpt-account-id: <account id from JWT>
//! ```
//!
//! response (trimmed):
//! ```json
//! {
//!   "models": [
//!     {
//!       "slug": "gpt-5",
//!       "display_name": "GPT-5",
//!       "description": "...",
//!       "context_window": 1050000,
//!       "input_modalities": ["text", "image"],
//!       "supported_reasoning_levels": [...],
//!       "default_reasoning_level": "medium",
//!       "supports_reasoning_summaries": true
//!     }
//!   ]
//! }
//! ```

use std::time::SystemTime;

use serde::Deserialize;

use super::{DiscoveredModel, DiscoveryError, DiscoveryReport, ModelDiscovery};
use crate::types::{Api, ApiKey, InputModality, Model, ModelCost, Provider, TokenCount};

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
const DEFAULT_MAX_OUTPUT: u64 = 64_000;
const PROVIDER_NAME: &str = "openai-codex";

/// codex discovery client
pub struct CodexDiscovery {
    client: reqwest::Client,
    base_url: String,
    /// chatgpt session access token
    access_token: ApiKey,
    /// chatgpt-account-id header. when `None` we try to extract it from the
    /// JWT payload at request time (codex's own convention)
    account_id: Option<String>,
    /// `client_version` query param. mush uses its own crate version.
    client_version: String,
}

impl CodexDiscovery {
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        base_url: impl Into<String>,
        access_token: ApiKey,
        account_id: Option<String>,
        client_version: impl Into<String>,
    ) -> Self {
        Self {
            client,
            base_url: base_url.into(),
            access_token,
            account_id,
            client_version: client_version.into(),
        }
    }
}

impl ModelDiscovery for CodexDiscovery {
    fn provider(&self) -> Provider {
        Provider::Custom(PROVIDER_NAME.into())
    }

    async fn fetch(&self) -> Result<DiscoveryReport, DiscoveryError> {
        let base = if self.base_url.is_empty() {
            DEFAULT_BASE_URL
        } else {
            self.base_url.trim_end_matches('/')
        };
        let url = format!(
            "{base}/models?client_version={ver}",
            ver = self.client_version
        );

        let mut req = self.client.get(&url).header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", self.access_token.expose()),
        );
        let account_id = self
            .account_id
            .clone()
            .or_else(|| extract_account_id(self.access_token.expose()));
        if let Some(account_id) = account_id {
            req = req.header("chatgpt-account-id", account_id);
        }
        req = req
            .header(reqwest::header::USER_AGENT, "mush")
            .header("originator", "mush");

        let body = super::execute_request(req).await?;
        let models = parse_codex_models(&body)?;
        Ok(DiscoveryReport {
            provider: Provider::Custom(PROVIDER_NAME.into()),
            fetched_at: SystemTime::now(),
            models,
        })
    }
}

/// parse the raw response body into [`DiscoveredModel`] entries.
pub fn parse_codex_models(body: &str) -> Result<Vec<DiscoveredModel>, DiscoveryError> {
    let response: CodexModelsResponse =
        serde_json::from_str(body).map_err(|e| DiscoveryError::Malformed(e.to_string()))?;
    response
        .models
        .into_iter()
        .map(|raw| {
            let entry: CodexModelEntry = serde_json::from_value(raw.clone())
                .map_err(|e| DiscoveryError::Malformed(e.to_string()))?;
            Ok(DiscoveredModel {
                model: entry_to_model(entry),
                raw: Some(raw),
            })
        })
        .collect()
}

fn entry_to_model(entry: CodexModelEntry) -> Model {
    let mut input = vec![InputModality::Text];
    if entry
        .input_modalities
        .iter()
        .any(|m| m.eq_ignore_ascii_case("image"))
    {
        input.push(InputModality::Image);
    }

    let context = entry
        .context_window
        .or(entry.max_context_window)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW as i64)
        .max(0) as u64;
    let max_output = entry
        .max_output_tokens
        .unwrap_or(DEFAULT_MAX_OUTPUT as i64)
        .max(0) as u64;

    // a model is reasoning-capable if codex advertises any non-`none` level
    let reasoning = entry
        .supported_reasoning_levels
        .iter()
        .any(|preset| !preset.effort.eq_ignore_ascii_case("none"));

    let display = entry
        .display_name
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| entry.slug.clone());

    Model {
        id: entry.slug.into(),
        name: display,
        api: Api::OpenaiResponses,
        provider: Provider::Custom(PROVIDER_NAME.into()),
        base_url: "https://chatgpt.com/backend-api".into(),
        reasoning,
        input,
        // chatgpt subscription has no per-token billing; cost stays zero
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        context_window: TokenCount::new(context),
        max_output_tokens: TokenCount::new(max_output),
        supports_adaptive_thinking: false,
    }
}

/// extract the `chatgpt_account_id` claim from a chatgpt-issued JWT.
/// duplicated from `crate::providers::openai_responses` to keep the
/// discovery module self-contained; consolidate if a third caller appears.
fn extract_account_id(token: &str) -> Option<String> {
    use base64::Engine;
    let payload_b64 = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload_b64))
        .ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&decoded).ok()?;

    payload
        .get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            payload
                .get("https://api.openai.com/auth")
                .and_then(|v| v.get("chatgpt_account_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}

#[derive(Deserialize, Debug)]
struct CodexModelsResponse {
    #[serde(default)]
    models: Vec<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct CodexModelEntry {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    context_window: Option<i64>,
    #[serde(default)]
    max_context_window: Option<i64>,
    #[serde(default)]
    max_output_tokens: Option<i64>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<CodexReasoningPreset>,
}

#[derive(Deserialize, Debug)]
struct CodexReasoningPreset {
    #[serde(default)]
    effort: String,
}

/// option-C payoff: typed view onto codex's enriched fields, parsed
/// lazily from `DiscoveredEntry::raw`. mush's [`Model`] only carries
/// the cross-cutting fields; everything codex-specific lives here so
/// consumers can opt in field-by-field without bloating the core type.
///
/// fields mirror codex's `ModelInfo` for the slice mush actually
/// consumes today (description, reasoning levels, priority, visibility)
/// plus an `additional_speed_tiers` hook for picker badges. unknown
/// upstream fields stay in `raw` for future code paths.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CodexModelExtras {
    /// human-readable blurb codex pairs with each model
    pub description: Option<String>,
    /// codex's recommended starting reasoning level for this model.
    /// kept as a string (not mush's `ThinkingLevel` enum) so unknown
    /// upstream values don't break parsing
    pub default_reasoning_level: Option<String>,
    /// every reasoning level the model accepts. ordering matches the
    /// upstream array, which codex authors curate from cheapest to
    /// most expensive
    pub supported_reasoning_levels: Vec<ReasoningLevelPreset>,
    /// whether the model emits a reasoning-summary block alongside its
    /// final answer (mush could expose this in the thinking ui)
    pub supports_reasoning_summaries: bool,
    /// codex's intra-catalogue priority (higher = preferred). picker
    /// could honour this for "default" sort order
    pub priority: i32,
    /// `default`, `internal`, `experimental`, …  picker can hide
    /// non-`default` entries unless the user opts in
    pub visibility: Option<String>,
    /// `fast`, … – picker badge candidates
    pub additional_speed_tiers: Vec<String>,
}

/// one row of `CodexModelExtras::supported_reasoning_levels`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReasoningLevelPreset {
    pub effort: String,
    pub description: String,
}

/// extract the typed extras view from a raw upstream entry. returns
/// `None` when no raw blob is supplied (the entry was cached before
/// raw-preservation landed) or when the JSON doesn't deserialise.
///
/// callers typically pass `entry.raw.as_ref()` for cache entries or
/// `merged.raw.as_ref()` for entries from the merge step.
#[must_use]
pub fn extras(raw: Option<&serde_json::Value>) -> Option<CodexModelExtras> {
    let raw = raw?;
    let parsed: CodexExtrasWire = serde_json::from_value(raw.clone()).ok()?;
    Some(CodexModelExtras {
        description: parsed.description.filter(|s| !s.is_empty()),
        default_reasoning_level: parsed.default_reasoning_level.filter(|s| !s.is_empty()),
        supported_reasoning_levels: parsed
            .supported_reasoning_levels
            .into_iter()
            .map(|p| ReasoningLevelPreset {
                effort: p.effort,
                description: p.description,
            })
            .collect(),
        supports_reasoning_summaries: parsed.supports_reasoning_summaries,
        priority: parsed.priority,
        visibility: parsed.visibility.filter(|s| !s.is_empty()),
        additional_speed_tiers: parsed.additional_speed_tiers,
    })
}

#[derive(Deserialize, Debug, Default)]
struct CodexExtrasWire {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<CodexExtrasReasoningPreset>,
    #[serde(default)]
    supports_reasoning_summaries: bool,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    additional_speed_tiers: Vec<String>,
}

#[derive(Deserialize, Debug, Default)]
struct CodexExtrasReasoningPreset {
    #[serde(default)]
    effort: String,
    #[serde(default)]
    description: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// representative codex `/models` payload covering reasoning levels,
    /// modalities, and the missing-display-name fallback path
    const FIXTURE: &str = r#"{
      "models": [
        {
          "slug": "gpt-5.4",
          "display_name": "GPT-5.4",
          "description": "Top-tier reasoning model",
          "context_window": 1050000,
          "max_context_window": 1050000,
          "max_output_tokens": 128000,
          "input_modalities": ["text", "image"],
          "supported_reasoning_levels": [
            {"effort": "minimal", "description": "fast"},
            {"effort": "low",     "description": "balanced"},
            {"effort": "medium",  "description": "default"},
            {"effort": "high",    "description": "deep"}
          ],
          "default_reasoning_level": "medium",
          "supports_reasoning_summaries": true,
          "priority": 100,
          "visibility": "default"
        },
        {
          "slug": "gpt-3.5-legacy",
          "display_name": "",
          "context_window": 16000,
          "input_modalities": ["text"],
          "supported_reasoning_levels": [{"effort": "none", "description": "no reasoning"}]
        }
      ]
    }"#;

    #[test]
    fn parses_full_fixture() {
        let models = parse_codex_models(FIXTURE).unwrap();
        assert_eq!(models.len(), 2);

        let gpt54 = &models[0].model;
        assert_eq!(gpt54.id.as_str(), "gpt-5.4");
        assert_eq!(gpt54.name, "GPT-5.4");
        assert_eq!(gpt54.api, Api::OpenaiResponses);
        assert_eq!(gpt54.provider, Provider::Custom(PROVIDER_NAME.into()));
        assert_eq!(gpt54.context_window, TokenCount::new(1_050_000));
        assert_eq!(gpt54.max_output_tokens, TokenCount::new(128_000));
        assert!(gpt54.reasoning);
        assert!(gpt54.input.contains(&InputModality::Image));
    }

    #[test]
    fn empty_display_name_falls_back_to_slug() {
        let models = parse_codex_models(FIXTURE).unwrap();
        let legacy = models
            .iter()
            .find(|m| m.model.id.as_str() == "gpt-3.5-legacy")
            .unwrap();
        assert_eq!(legacy.model.name, "gpt-3.5-legacy");
    }

    #[test]
    fn only_none_reasoning_levels_marks_non_reasoning() {
        let models = parse_codex_models(FIXTURE).unwrap();
        let legacy = models
            .iter()
            .find(|m| m.model.id.as_str() == "gpt-3.5-legacy")
            .unwrap();
        assert!(!legacy.model.reasoning);
    }

    #[test]
    fn cost_stays_zero_for_codex() {
        // chatgpt subscription has no per-token billing exposed in /models
        let models = parse_codex_models(FIXTURE).unwrap();
        for m in &models {
            assert_eq!(m.model.cost.input, 0.0);
            assert_eq!(m.model.cost.output, 0.0);
        }
    }

    #[test]
    fn parser_preserves_raw_entry_json() {
        // codex's rich fields (supported_reasoning_levels, visibility,
        // priority, …) are kept verbatim for a typed accessor to read later
        let models = parse_codex_models(FIXTURE).unwrap();
        let raw = models[0].raw.as_ref().expect("raw populated");
        assert_eq!(raw["slug"], "gpt-5.4");
        assert_eq!(raw["default_reasoning_level"], "medium");
        assert_eq!(
            raw["supported_reasoning_levels"].as_array().unwrap().len(),
            4
        );
        assert_eq!(raw["priority"], 100);
    }

    #[test]
    fn malformed_json_returns_error() {
        let err = parse_codex_models("not json").unwrap_err();
        assert!(matches!(err, DiscoveryError::Malformed(_)));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // forward-compat: codex regularly adds new fields. unknown ones must
        // round-trip through `raw` without breaking the typed parse
        let body = r#"{
          "models": [{
            "slug": "test",
            "future_field": "whatever",
            "experimental_supported_tools": ["new-tool"]
          }]
        }"#;
        let models = parse_codex_models(body).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(
            models[0].raw.as_ref().unwrap()["experimental_supported_tools"][0],
            "new-tool"
        );
    }

    #[test]
    fn extras_extracts_codex_specific_fields_from_raw() {
        let raw = serde_json::json!({
            "slug": "gpt-5.4",
            "display_name": "GPT-5.4",
            "description": "Top-tier reasoning",
            "default_reasoning_level": "medium",
            "supported_reasoning_levels": [
                {"effort": "minimal", "description": "fast"},
                {"effort": "medium",  "description": "default"},
                {"effort": "high",    "description": "deep"}
            ],
            "supports_reasoning_summaries": true,
            "priority": 100,
            "visibility": "default",
            "additional_speed_tiers": ["fast"]
        });

        let extras = extras(Some(&raw)).expect("raw payload parses");
        assert_eq!(extras.description.as_deref(), Some("Top-tier reasoning"));
        assert_eq!(extras.default_reasoning_level.as_deref(), Some("medium"));
        assert_eq!(extras.supported_reasoning_levels.len(), 3);
        assert_eq!(extras.supported_reasoning_levels[0].effort, "minimal");
        assert!(extras.supports_reasoning_summaries);
        assert_eq!(extras.priority, 100);
        assert_eq!(extras.visibility.as_deref(), Some("default"));
        assert_eq!(extras.additional_speed_tiers, vec!["fast".to_string()]);
    }

    #[test]
    fn extras_returns_none_when_raw_missing() {
        assert!(extras(None).is_none());
    }

    #[test]
    fn extras_tolerates_missing_fields() {
        // bare-minimum entry with only `slug` — every optional field stays None / default
        let raw = serde_json::json!({ "slug": "tiny" });
        let extras = extras(Some(&raw)).unwrap();
        assert!(extras.description.is_none());
        assert!(extras.default_reasoning_level.is_none());
        assert!(extras.supported_reasoning_levels.is_empty());
        assert!(!extras.supports_reasoning_summaries);
        assert_eq!(extras.priority, 0);
    }
}
