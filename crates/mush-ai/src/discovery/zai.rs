use std::time::SystemTime;

use serde::Deserialize;

use super::{DiscoveredModel, DiscoveryError, DiscoveryReport};
use crate::models::ZAI_CODING_PLAN_BASE_URL;
use crate::types::{Api, InputModality, Model, ModelCost, Provider, ThinkingLevel, TokenCount};

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const MODELS_DEV_PROVIDER: &str = "zai-coding-plan";

pub struct ZaiDiscovery {
    client: crate::http::HttpClient,
    url: String,
}

impl ZaiDiscovery {
    #[must_use]
    pub fn new(client: crate::http::HttpClient, url: impl Into<String>) -> Self {
        Self {
            client,
            url: url.into(),
        }
    }

    pub async fn fetch(&self) -> Result<DiscoveryReport, DiscoveryError> {
        let url = if self.url.is_empty() {
            MODELS_DEV_URL
        } else {
            &self.url
        };
        let body = super::execute_request(self.client.get(url)).await?;
        Ok(DiscoveryReport {
            provider: Provider::Custom("zai".into()),
            fetched_at: SystemTime::now(),
            models: parse_models_dev(&body)?,
        })
    }
}

pub fn parse_models_dev(body: &str) -> Result<Vec<DiscoveredModel>, DiscoveryError> {
    let mut catalogue: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(body).map_err(|e| DiscoveryError::Malformed(e.to_string()))?;
    let models = catalogue
        .remove(MODELS_DEV_PROVIDER)
        .and_then(|provider| provider.get("models").cloned())
        .and_then(|models| match models {
            serde_json::Value::Object(models) => Some(models),
            _ => None,
        })
        .ok_or_else(|| {
            DiscoveryError::Malformed(format!("models.dev has no {MODELS_DEV_PROVIDER} models"))
        })?;
    let mut discovered = Vec::new();
    for (id, raw) in models {
        let entry = match Entry::deserialize(&raw) {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%id, %error, "skipping unreadable models.dev glm entry");
                continue;
            }
        };
        if !entry.tool_call || entry.status.as_deref() == Some("deprecated") {
            continue;
        }
        discovered.push(DiscoveredModel {
            model: entry_to_model(&id, entry),
            raw: Some(raw),
        });
    }
    Ok(discovered)
}

fn entry_to_model(id: &str, entry: Entry) -> Model {
    let input = if entry.modalities.input.iter().any(|m| m == "image") {
        vec![InputModality::Text, InputModality::Image]
    } else {
        vec![InputModality::Text]
    };
    let supported_thinking_levels = if entry.reasoning {
        thinking_levels(&entry.reasoning_options)
    } else {
        Vec::new()
    };
    Model {
        id: id.into(),
        name: format!(
            "{} (GLM Coding Plan)",
            entry.name.unwrap_or_else(|| id.to_string())
        ),
        api: Api::OpenaiCompletions,
        provider: Provider::Custom("zai".into()),
        base_url: ZAI_CODING_PLAN_BASE_URL.into(),
        reasoning: entry.reasoning,
        input,
        cost: ModelCost {
            input: entry.cost.input,
            output: entry.cost.output,
            cache_read: entry.cost.cache_read,
            cache_write: entry.cost.cache_write,
        },
        context_window: TokenCount::new(entry.limit.context),
        effective_context_window: None,
        max_output_tokens: TokenCount::new(entry.limit.output),
        supports_adaptive_thinking: false,
        supported_thinking_levels,
        default_thinking_level: None,
    }
}

fn thinking_levels(options: &[ReasoningOption]) -> Vec<ThinkingLevel> {
    let mut levels = Vec::new();
    for option in options {
        match option.kind.as_str() {
            "toggle" => levels.extend([ThinkingLevel::Off, ThinkingLevel::High]),
            "effort" => levels.extend(option.values.iter().filter_map(
                |value| match value.as_str() {
                    "none" => Some(ThinkingLevel::Off),
                    "minimal" => Some(ThinkingLevel::Minimal),
                    "low" => Some(ThinkingLevel::Low),
                    "medium" => Some(ThinkingLevel::Medium),
                    "high" => Some(ThinkingLevel::High),
                    "xhigh" | "max" => Some(ThinkingLevel::Xhigh),
                    _ => None,
                },
            )),
            _ => {}
        }
    }
    if levels.is_empty() {
        levels.extend([ThinkingLevel::Off, ThinkingLevel::High]);
    }
    ThinkingLevel::LADDER
        .into_iter()
        .filter(|rung| levels.contains(rung))
        .collect()
}

#[derive(Deserialize)]
struct Entry {
    name: Option<String>,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    reasoning_options: Vec<ReasoningOption>,
    #[serde(default)]
    tool_call: bool,
    status: Option<String>,
    #[serde(default)]
    modalities: Modalities,
    limit: Limit,
    #[serde(default)]
    cost: Cost,
}

#[derive(Deserialize)]
struct ReasoningOption {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    values: Vec<String>,
}

#[derive(Deserialize, Default)]
struct Modalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Deserialize)]
struct Limit {
    context: u64,
    output: u64,
}

#[derive(Deserialize, Default)]
struct Cost {
    #[serde(default)]
    input: f64,
    #[serde(default)]
    output: f64,
    #[serde(default)]
    cache_read: f64,
    #[serde(default)]
    cache_write: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "openai": { "models": { "gpt-5": { "name": "GPT-5", "tool_call": true, "limit": { "context": 1, "output": 1 } } } },
      "zai-coding-plan": {
        "api": "https://api.z.ai/api/coding/paas/v4",
        "models": {
          "glm-5.3-flash": {
            "name": "GLM-5.3-Flash", "reasoning": true, "tool_call": true,
            "reasoning_options": [{ "type": "effort", "values": ["low", "high", "max"] }],
            "modalities": { "input": ["text", "image", "video", "pdf"] },
            "limit": { "context": 1000000, "output": 131072 },
            "cost": { "input": 0, "output": 0 }
          },
          "glm-4.7": {
            "name": "GLM-4.7", "reasoning": true, "tool_call": true,
            "reasoning_options": [{ "type": "toggle" }],
            "modalities": { "input": ["text"] },
            "limit": { "context": 204800, "output": 131072 }
          },
          "glm-4.5v": {
            "name": "GLM-4.5V", "reasoning": true, "tool_call": false,
            "limit": { "context": 64000, "output": 16384 }
          },
          "glm-4.5": {
            "name": "GLM-4.5", "reasoning": true, "tool_call": true, "status": "deprecated",
            "limit": { "context": 128000, "output": 96000 }
          },
          "glm-6-preview": { "name": "GLM-6", "tool_call": true }
        }
      }
    }"#;

    fn by_id(id: &str) -> Model {
        parse_models_dev(FIXTURE)
            .unwrap()
            .into_iter()
            .find(|m| m.model.id.as_str() == id)
            .unwrap()
            .model
    }

    #[test]
    fn only_tool_calling_live_coding_plan_models_are_kept() {
        let mut ids: Vec<String> = parse_models_dev(FIXTURE)
            .unwrap()
            .into_iter()
            .map(|m| m.model.id.as_str().to_string())
            .collect();
        ids.sort();
        assert_eq!(ids, ["glm-4.7", "glm-5.3-flash"]);
    }

    #[test]
    fn models_land_on_the_coding_plan_endpoint_with_their_limits() {
        let flash = by_id("glm-5.3-flash");
        assert_eq!(flash.provider, Provider::Custom("zai".into()));
        assert_eq!(flash.api, Api::OpenaiCompletions);
        assert_eq!(flash.base_url.as_str(), ZAI_CODING_PLAN_BASE_URL);
        assert_eq!(flash.context_window, TokenCount::new(1_000_000));
        assert_eq!(flash.max_output_tokens, TokenCount::new(131_072));
        assert!(flash.input.contains(&InputModality::Image));
        assert_eq!(flash.name, "GLM-5.3-Flash (GLM Coding Plan)");
    }

    #[test]
    fn reasoning_options_become_thinking_levels() {
        assert_eq!(
            by_id("glm-5.3-flash").supported_thinking_levels,
            [
                ThinkingLevel::Low,
                ThinkingLevel::High,
                ThinkingLevel::Xhigh
            ]
        );
        assert_eq!(
            by_id("glm-4.7").supported_thinking_levels,
            [ThinkingLevel::Off, ThinkingLevel::High]
        );
    }

    #[test]
    fn a_catalogue_without_the_coding_plan_is_malformed() {
        assert!(matches!(
            parse_models_dev(r#"{"openai": {"models": {}}}"#),
            Err(DiscoveryError::Malformed(_))
        ));
    }
}
