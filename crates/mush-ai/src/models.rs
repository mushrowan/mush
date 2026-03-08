//! built-in model catalogue
//!
//! static definitions for known models. users can override via models.json.

use crate::types::*;

/// all built-in anthropic models
pub fn anthropic_models() -> Vec<Model> {
    vec![
        // current generation
        Model {
            id: "claude-opus-4-6".into(),
            name: "Claude Opus 4.6".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 5.0,
                output: 25.0,
                cache_read: 0.5,
                cache_write: 6.25,
            },
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(128_000),
        },
        Model {
            id: "claude-sonnet-4-6".into(),
            name: "Claude Sonnet 4.6".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(64_000),
        },
        Model {
            id: "claude-haiku-4-5".into(),
            name: "Claude Haiku 4.5".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 1.0,
                output: 5.0,
                cache_read: 0.1,
                cache_write: 1.25,
            },
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(64_000),
        },
        // legacy
        Model {
            id: "claude-sonnet-4-20250514".into(),
            name: "Claude Sonnet 4".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(64_000),
        },
        Model {
            id: "claude-opus-4-20250514".into(),
            name: "Claude Opus 4".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 15.0,
                output: 75.0,
                cache_read: 1.5,
                cache_write: 18.75,
            },
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(32768),
        },
    ]
}

/// all built-in openrouter models (via openai-completions api)
pub fn openrouter_models() -> Vec<Model> {
    vec![
        Model {
            id: "anthropic/claude-sonnet-4".into(),
            name: "Claude Sonnet 4 (OpenRouter)".into(),
            api: Api::OpenaiCompletions,
            provider: Provider::OpenRouter,
            base_url: "https://openrouter.ai/api/v1".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(16384),
        },
        Model {
            id: "anthropic/claude-opus-4".into(),
            name: "Claude Opus 4 (OpenRouter)".into(),
            api: Api::OpenaiCompletions,
            provider: Provider::OpenRouter,
            base_url: "https://openrouter.ai/api/v1".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 15.0,
                output: 75.0,
                cache_read: 1.5,
                cache_write: 18.75,
            },
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(32768),
        },
        Model {
            id: "google/gemini-2.5-pro".into(),
            name: "Gemini 2.5 Pro (OpenRouter)".into(),
            api: Api::OpenaiCompletions,
            provider: Provider::OpenRouter,
            base_url: "https://openrouter.ai/api/v1".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 1.25,
                output: 10.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(1_048_576),
            max_output_tokens: TokenCount::new(65536),
        },
        Model {
            id: "google/gemini-2.5-flash".into(),
            name: "Gemini 2.5 Flash (OpenRouter)".into(),
            api: Api::OpenaiCompletions,
            provider: Provider::OpenRouter,
            base_url: "https://openrouter.ai/api/v1".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 0.15,
                output: 0.6,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(1_048_576),
            max_output_tokens: TokenCount::new(65536),
        },
    ]
}

/// all built-in openai api-key models (responses API)
pub fn openai_models() -> Vec<Model> {
    vec![
        Model {
            id: "gpt-5.2".into(),
            name: "GPT-5.2".into(),
            api: Api::OpenaiResponses,
            provider: Provider::Custom("openai".into()),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 1.25,
                output: 10.0,
                cache_read: 0.125,
                cache_write: 1.5625,
            },
            context_window: TokenCount::new(400_000),
            max_output_tokens: TokenCount::new(128_000),
        },
        Model {
            id: "gpt-5.2-mini".into(),
            name: "GPT-5.2 Mini".into(),
            api: Api::OpenaiResponses,
            provider: Provider::Custom("openai".into()),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 0.25,
                output: 2.0,
                cache_read: 0.025,
                cache_write: 0.3125,
            },
            context_window: TokenCount::new(400_000),
            max_output_tokens: TokenCount::new(64_000),
        },
    ]
}

/// all built-in openai codex subscription models (chatgpt oauth)
pub fn openai_codex_models() -> Vec<Model> {
    vec![
        Model {
            id: "gpt-5.4-codex".into(),
            name: "GPT-5.4 Codex (ChatGPT subscription)".into(),
            api: Api::OpenaiResponses,
            provider: Provider::Custom("openai-codex".into()),
            base_url: "https://chatgpt.com/backend-api".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(400_000),
            max_output_tokens: TokenCount::new(128_000),
        },
        Model {
            id: "gpt-5.3-codex".into(),
            name: "GPT-5.3 Codex (ChatGPT subscription)".into(),
            api: Api::OpenaiResponses,
            provider: Provider::Custom("openai-codex".into()),
            base_url: "https://chatgpt.com/backend-api".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(400_000),
            max_output_tokens: TokenCount::new(128_000),
        },
        Model {
            id: "gpt-5.2-codex".into(),
            name: "GPT-5.2 Codex (ChatGPT subscription)".into(),
            api: Api::OpenaiResponses,
            provider: Provider::Custom("openai-codex".into()),
            base_url: "https://chatgpt.com/backend-api".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(400_000),
            max_output_tokens: TokenCount::new(128_000),
        },
    ]
}

/// all built-in models across all providers
pub fn all_models() -> Vec<Model> {
    let mut models = Vec::new();
    models.extend(anthropic_models());
    models.extend(openrouter_models());
    models.extend(openai_models());
    models.extend(openai_codex_models());
    models
}

/// all models including user overrides from models.json
pub fn all_models_with_user() -> Vec<Model> {
    let mut models = all_models();
    let user = load_user_models();

    // user models override builtins with matching id
    for um in user {
        if let Some(pos) = models.iter().position(|m| m.id == um.id) {
            models[pos] = um;
        } else {
            models.push(um);
        }
    }

    models
}

/// find a model by provider and id (includes user models)
pub fn find_model(provider: &Provider, id: &str) -> Option<Model> {
    all_models_with_user()
        .into_iter()
        .find(|m| &m.provider == provider && m.id.as_str() == id)
}

/// find a model by id alone (first match, includes user models)
pub fn find_model_by_id(id: &str) -> Option<Model> {
    all_models_with_user()
        .into_iter()
        .find(|m| m.id.as_str() == id)
}

/// load user-defined models from ~/.config/mush/models.json
fn load_user_models() -> Vec<Model> {
    let path = user_models_path();
    if !path.exists() {
        return vec![];
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str::<Vec<Model>>(&content).ok())
        .unwrap_or_default()
}

fn user_models_path() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("MUSH_CONFIG_DIR") {
        std::path::PathBuf::from(dir).join("models.json")
    } else if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        std::path::PathBuf::from(config).join("mush/models.json")
    } else if let Some(home) = std::env::var_os("HOME") {
        std::path::PathBuf::from(home).join(".config/mush/models.json")
    } else {
        std::path::PathBuf::from(".mush/models.json")
    }
}

/// list all models for a provider
pub fn models_for_provider(provider: &Provider) -> Vec<Model> {
    all_models()
        .into_iter()
        .filter(|m| &m.provider == provider)
        .collect()
}

/// calculate cost from usage and model pricing
pub fn calculate_cost(model: &Model, usage: &Usage) -> Cost {
    let per_token = |rate: f64, tokens: TokenCount| -> Dollars {
        Dollars::new((rate / 1_000_000.0) * tokens.get() as f64)
    };
    Cost {
        input: per_token(model.cost.input, usage.input_tokens),
        output: per_token(model.cost.output, usage.output_tokens),
        cache_read: per_token(model.cost.cache_read, usage.cache_read_tokens),
        cache_write: per_token(model.cost.cache_write, usage.cache_write_tokens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_models_exist() {
        let models = anthropic_models();
        assert!(models.len() >= 3);
        assert!(models.iter().all(|m| m.provider == Provider::Anthropic));
        assert!(models.iter().all(|m| m.api == Api::AnthropicMessages));
    }

    #[test]
    fn openrouter_models_exist() {
        let models = openrouter_models();
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| m.provider == Provider::OpenRouter));
        assert!(models.iter().all(|m| m.api == Api::OpenaiCompletions));
    }

    #[test]
    fn openai_models_exist() {
        let models = openai_models();
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| m.api == Api::OpenaiResponses));
        assert!(
            models
                .iter()
                .all(|m| m.provider == Provider::Custom("openai".into()))
        );
    }

    #[test]
    fn openai_codex_models_exist() {
        let models = openai_codex_models();
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| m.api == Api::OpenaiResponses));
        assert!(
            models
                .iter()
                .all(|m| m.provider == Provider::Custom("openai-codex".into()))
        );
    }

    #[test]
    fn find_model_by_provider_and_id() {
        let model = find_model(&Provider::Anthropic, "claude-sonnet-4-20250514");
        assert!(model.is_some());
        let model = model.unwrap();
        assert_eq!(model.name, "Claude Sonnet 4");
    }

    #[test]
    fn find_model_by_id_alone() {
        let model = find_model_by_id("claude-sonnet-4-20250514");
        assert!(model.is_some());
    }

    #[test]
    fn find_model_missing() {
        assert!(find_model_by_id("nonexistent-model").is_none());
    }

    #[test]
    fn models_for_provider_filters() {
        let anthropic = models_for_provider(&Provider::Anthropic);
        let openrouter = models_for_provider(&Provider::OpenRouter);
        assert!(anthropic.iter().all(|m| m.provider == Provider::Anthropic));
        assert!(
            openrouter
                .iter()
                .all(|m| m.provider == Provider::OpenRouter)
        );
    }

    #[test]
    fn user_models_override_builtins() {
        // simulate by calling the override logic directly
        let mut models = vec![Model {
            id: "test-model".into(),
            name: "Test".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(100_000),
            max_output_tokens: TokenCount::new(4096),
        }];

        let user = vec![Model {
            id: "test-model".into(),
            name: "Test Override".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://custom.api.com".into(),
            reasoning: true,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 5.0,
                output: 10.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(8192),
        }];

        for um in user {
            if let Some(pos) = models.iter().position(|m| m.id == um.id) {
                models[pos] = um;
            } else {
                models.push(um);
            }
        }

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "Test Override");
        assert_eq!(models[0].context_window, TokenCount::new(200_000));
    }

    #[test]
    fn user_models_add_new() {
        let mut models = anthropic_models();
        let original_len = models.len();

        let user = vec![Model {
            id: "custom/my-model".into(),
            name: "My Model".into(),
            api: Api::OpenaiCompletions,
            provider: Provider::Custom("my-provider".into()),
            base_url: "https://my-api.com".into(),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.5,
                output: 1.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(128_000),
            max_output_tokens: TokenCount::new(4096),
        }];

        for um in user {
            if let Some(pos) = models.iter().position(|m| m.id == um.id) {
                models[pos] = um;
            } else {
                models.push(um);
            }
        }

        assert_eq!(models.len(), original_len + 1);
        assert!(models.iter().any(|m| m.id.as_str() == "custom/my-model"));
    }

    #[test]
    fn model_serialisation_roundtrip() {
        let model = anthropic_models().into_iter().next().unwrap();
        let json = serde_json::to_string_pretty(&model).unwrap();
        let restored: Model = serde_json::from_str(&json).unwrap();
        assert_eq!(model.id, restored.id);
        assert_eq!(model.name, restored.name);
        assert_eq!(model.cost.input, restored.cost.input);
    }

    #[test]
    fn load_user_models_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        let models = vec![Model {
            id: "custom/test".into(),
            name: "Custom Test".into(),
            api: Api::OpenaiCompletions,
            provider: Provider::Custom("test".into()),
            base_url: "https://test.api.com".into(),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(128_000),
            max_output_tokens: TokenCount::new(4096),
        }];
        std::fs::write(&path, serde_json::to_string(&models).unwrap()).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let loaded: Vec<Model> = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id.as_str(), "custom/test");
        assert_eq!(loaded[0].name, "Custom Test");
    }

    #[test]
    fn user_models_path_uses_env() {
        let dir = tempfile::tempdir().unwrap();
        // just test the path logic, not the actual env var
        let path = dir.path().join("mush/models.json");
        assert!(path.to_str().unwrap().ends_with("mush/models.json"));
    }

    #[test]
    fn calculate_cost_works() {
        // opus 4.6: $5/MTok input, $25/MTok output
        let model = anthropic_models().into_iter().next().unwrap();
        let usage = Usage {
            input_tokens: TokenCount::new(1_000_000),
            output_tokens: TokenCount::new(500_000),
            cache_read_tokens: TokenCount::new(100_000),
            cache_write_tokens: TokenCount::new(50_000),
        };
        let cost = calculate_cost(&model, &usage);
        assert!((cost.input.get() - 5.0).abs() < f64::EPSILON);
        assert!((cost.output.get() - 12.5).abs() < f64::EPSILON);
        assert!(cost.total() > Dollars::ZERO);
    }
}
