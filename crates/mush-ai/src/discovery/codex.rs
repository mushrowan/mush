use std::time::SystemTime;

use semver::Version;
use serde::Deserialize;

use super::{DiscoveredModel, DiscoveryError, DiscoveryReport, ModelDiscovery};
use crate::types::{
    Api, ApiKey, InputModality, Model, ModelCost, Provider, ThinkingLevel, TokenCount,
};

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
const DEFAULT_MAX_OUTPUT: u64 = 64_000;
const MIN_DISCOVERY_CLIENT_VERSION: Version = Version::new(1, 0, 0);
const PROVIDER_NAME: &str = "openai-codex";

pub struct CodexDiscovery {
    client: crate::http::HttpClient,
    base_url: String,
    access_token: ApiKey,
    account_id: Option<String>,
    client_version: String,
}

impl CodexDiscovery {
    #[must_use]
    pub fn new(
        client: crate::http::HttpClient,
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
            client_version: discovery_client_version(&client_version.into()),
        }
    }
}

fn discovery_client_version(version: &str) -> String {
    let parsed = Version::parse(version)
        .map(|mut version| {
            version.pre = semver::Prerelease::EMPTY;
            version.build = semver::BuildMetadata::EMPTY;
            version
        })
        .unwrap_or_else(|_| MIN_DISCOVERY_CLIENT_VERSION.clone());

    std::cmp::max(parsed, MIN_DISCOVERY_CLIENT_VERSION.clone()).to_string()
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

pub fn parse_codex_models(body: &str) -> Result<Vec<DiscoveredModel>, DiscoveryError> {
    let response: CodexModelsResponse =
        serde_json::from_str(body).map_err(|e| DiscoveryError::Malformed(e.to_string()))?;
    response
        .models
        .into_iter()
        .map(|raw| {
            let entry = CodexModelEntry::deserialize(&raw)
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

    let reasoning = entry
        .supported_reasoning_levels
        .iter()
        .any(|preset| !preset.effort.eq_ignore_ascii_case("none"));

    let supported_thinking_levels = entry
        .supported_reasoning_levels
        .iter()
        .filter(|p| !p.effort.eq_ignore_ascii_case("none"))
        .map(|p| ThinkingLevel::from_effort_str(&p.effort))
        .collect();

    let default_thinking_level = entry
        .default_reasoning_level
        .as_deref()
        .map(ThinkingLevel::from_effort_str);

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
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        context_window: TokenCount::new(context),
        effective_context_window: None,
        max_output_tokens: TokenCount::new(max_output),
        supports_adaptive_thinking: false,
        supported_thinking_levels,
        default_thinking_level,
    }
}

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
    #[serde(default)]
    default_reasoning_level: Option<String>,
}

#[derive(Deserialize, Debug)]
struct CodexReasoningPreset {
    #[serde(default)]
    effort: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CodexModelExtras {
    pub description: Option<String>,
    pub default_reasoning_level: Option<String>,
    pub supported_reasoning_levels: Vec<ReasoningLevelPreset>,
    pub supports_reasoning_summaries: bool,
    pub priority: i32,
    pub visibility: Option<String>,
    pub additional_speed_tiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReasoningLevelPreset {
    pub effort: String,
    pub description: String,
}

#[must_use]
pub fn extras(raw: Option<&serde_json::Value>) -> Option<CodexModelExtras> {
    let raw = raw?;
    let parsed = CodexExtrasWire::deserialize(raw).ok()?;
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
    fn supported_thinking_levels_propagate_from_codex_extras() {
        let models = parse_codex_models(FIXTURE).unwrap();
        let gpt54 = &models[0].model;
        assert_eq!(
            gpt54.supported_thinking_levels,
            vec![
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ]
        );
    }

    #[test]
    fn default_thinking_level_propagates_from_codex_extras() {
        let models = parse_codex_models(FIXTURE).unwrap();
        let gpt54 = &models[0].model;
        assert_eq!(gpt54.default_thinking_level, Some(ThinkingLevel::Medium));
    }

    #[test]
    fn legacy_codex_entry_with_only_none_level_has_empty_supported_levels() {
        let models = parse_codex_models(FIXTURE).unwrap();
        let legacy = models
            .iter()
            .find(|m| m.model.id.as_str() == "gpt-3.5-legacy")
            .unwrap();
        assert!(legacy.model.supported_thinking_levels.is_empty());
        assert!(legacy.model.default_thinking_level.is_none());
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
        let models = parse_codex_models(FIXTURE).unwrap();
        for m in &models {
            assert_eq!(m.model.cost.input, 0.0);
            assert_eq!(m.model.cost.output, 0.0);
        }
    }

    #[test]
    fn parser_preserves_raw_entry_json() {
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
        let raw = serde_json::json!({ "slug": "tiny" });
        let extras = extras(Some(&raw)).unwrap();
        assert!(extras.description.is_none());
        assert!(extras.default_reasoning_level.is_none());
        assert!(extras.supported_reasoning_levels.is_empty());
        assert!(!extras.supports_reasoning_summaries);
        assert_eq!(extras.priority, 0);
    }

    #[test]
    fn discovery_client_version_has_floor_for_old_app_versions() {
        assert_eq!(discovery_client_version("0.1.0"), "1.0.0");
    }

    #[test]
    fn discovery_client_version_drops_prerelease_suffix() {
        assert_eq!(discovery_client_version("1.5.0-alpha.2"), "1.5.0");
    }
}
