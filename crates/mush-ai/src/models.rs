use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use crate::types::{
    Api, Cost, Dollars, InputModality, Model, ModelCost, Provider, TokenCount, Usage,
};

#[derive(Debug, Clone)]
struct UserModelsCache {
    path: PathBuf,
    modified: Option<SystemTime>,
    size: Option<u64>,
    models: Vec<Model>,
}

static BUILTIN_MODELS: LazyLock<Vec<Model>> = LazyLock::new(|| {
    let mut models = Vec::new();
    models.extend(anthropic_models());
    models.extend(openrouter_models());
    models.extend(openai_models());
    models.extend(openai_codex_models());
    models
});

static USER_MODELS_CACHE: LazyLock<Mutex<Option<UserModelsCache>>> =
    LazyLock::new(|| Mutex::new(None));

#[must_use]
pub fn anthropic_models() -> Vec<Model> {
    vec![
        Model {
            id: "claude-opus-5-5".into(),
            name: "Claude Opus 5.5".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 4.0,
                output: 20.0,
                cache_read: 0.2,
                cache_write: 5.0,
            },
            context_window: TokenCount::new(1_000_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(128_000),
            supports_adaptive_thinking: true,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: Some(crate::types::ThinkingLevel::Medium),
        },
        Model {
            id: "claude-sonnet-5".into(),
            name: "Claude Sonnet 5".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 2.0,
                output: 10.0,
                cache_read: 0.2,
                cache_write: 2.5,
            },
            context_window: TokenCount::new(1_000_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(128_000),
            supports_adaptive_thinking: true,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: Some(crate::types::ThinkingLevel::High),
        },
        Model {
            id: "claude-opus-4-7".into(),
            name: "Claude Opus 4.7".into(),
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
            context_window: TokenCount::new(1_000_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(128_000),
            supports_adaptive_thinking: true,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        },
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
            context_window: TokenCount::new(1_000_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(128_000),
            supports_adaptive_thinking: true,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        },
        Model {
            id: "claude-opus-4-5".into(),
            name: "Claude Opus 4.5".into(),
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(64_000),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
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
            context_window: TokenCount::new(1_000_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(64_000),
            supports_adaptive_thinking: true,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(64_000),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        },
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(64_000),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(32768),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        },
    ]
}

#[must_use]
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(16384),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(32768),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(65536),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(65536),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        },
    ]
}

#[must_use]
pub fn openai_models() -> Vec<Model> {
    vec![Model {
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
        effective_context_window: None,
        max_output_tokens: TokenCount::new(128_000),
        supports_adaptive_thinking: false,
        supported_thinking_levels: Vec::new(),
        default_thinking_level: None,
    }]
}

#[must_use]
pub fn openai_codex_models() -> Vec<Model> {
    vec![
        Model {
            id: "gpt-5.5".into(),
            name: "GPT-5.5 (ChatGPT subscription)".into(),
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
            context_window: TokenCount::new(272_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(128_000),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        },
        Model {
            id: "gpt-5.4".into(),
            name: "GPT-5.4 (ChatGPT subscription)".into(),
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
            context_window: TokenCount::new(1_050_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(128_000),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(128_000),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(128_000),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        },
    ]
}

#[must_use]
pub fn all_models() -> Vec<Model> {
    BUILTIN_MODELS.clone()
}

#[must_use]
pub fn all_models_with_user() -> Vec<Model> {
    let mut models = all_models();
    let user = load_user_models();

    for um in user {
        if let Some(pos) = models.iter().position(|m| m.id == um.id) {
            models[pos] = um;
        } else {
            models.push(um);
        }
    }

    models
}

#[must_use]
pub fn model_overrides() -> Vec<Model> {
    load_user_models()
}

#[must_use]
pub fn find_model(provider: &Provider, id: &str) -> Option<Model> {
    all_models_with_user()
        .into_iter()
        .find(|m| &m.provider == provider && m.id.as_str() == id)
}

#[must_use]
pub fn find_model_by_id(id: &str) -> Option<Model> {
    all_models_with_user()
        .into_iter()
        .find(|m| m.id.as_str() == id)
}

fn load_user_models() -> Vec<Model> {
    let path = user_models_path();
    let metadata = std::fs::metadata(&path).ok();
    let modified = metadata
        .as_ref()
        .and_then(|metadata| metadata.modified().ok());
    let size = metadata.as_ref().map(std::fs::Metadata::len);

    if let Ok(cache) = USER_MODELS_CACHE.lock()
        && let Some(cached) = cache.as_ref()
        && cached.path == path
        && cached.modified == modified
        && cached.size == size
    {
        return cached.models.clone();
    }

    let models = std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str::<Vec<Model>>(&content).ok())
        .unwrap_or_default();

    if let Ok(mut cache) = USER_MODELS_CACHE.lock() {
        *cache = Some(UserModelsCache {
            path,
            modified,
            size,
            models: models.clone(),
        });
    }

    models
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

#[cfg(test)]
fn clear_user_models_cache() {
    if let Ok(mut cache) = USER_MODELS_CACHE.lock() {
        *cache = None;
    }
}

#[must_use]
pub fn models_for_provider(provider: &Provider) -> Vec<Model> {
    all_models()
        .into_iter()
        .filter(|m| &m.provider == provider)
        .collect()
}

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
    use std::sync::{LazyLock, Mutex};

    use super::*;

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct EnvGuard {
        saved: Option<String>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            clear_user_models_cache();
            unsafe {
                match self.saved.take() {
                    Some(value) => std::env::set_var("MUSH_CONFIG_DIR", value),
                    None => std::env::remove_var("MUSH_CONFIG_DIR"),
                }
            }
        }
    }

    fn set_models_dir(path: &std::path::Path) -> EnvGuard {
        let saved = std::env::var("MUSH_CONFIG_DIR").ok();
        clear_user_models_cache();
        unsafe {
            std::env::set_var("MUSH_CONFIG_DIR", path);
        }
        EnvGuard { saved }
    }

    #[test]
    fn anthropic_models_exist() {
        let models = anthropic_models();
        assert!(models.len() >= 3);
        assert!(models.iter().all(|m| m.provider == Provider::Anthropic));
        assert!(models.iter().all(|m| m.api == Api::AnthropicMessages));
    }

    #[test]
    fn current_anthropic_models_have_current_limits_and_pricing() {
        let models = anthropic_models();
        let by_id: std::collections::HashMap<_, _> =
            models.iter().map(|m| (m.id.as_str(), m)).collect();
        let opus = by_id["claude-opus-5-5"];
        assert_eq!(opus.context_window, TokenCount::new(1_000_000));
        assert_eq!(opus.max_output_tokens, TokenCount::new(128_000));
        assert_eq!(opus.cost.input, 4.0);
        assert_eq!(opus.cost.output, 20.0);
        assert!(opus.supports_adaptive_thinking);
        let sonnet = by_id["claude-sonnet-5"];
        assert_eq!(sonnet.context_window, TokenCount::new(1_000_000));
        assert_eq!(sonnet.max_output_tokens, TokenCount::new(128_000));
        assert_eq!(sonnet.cost.input, 2.0);
        assert_eq!(sonnet.cost.output, 10.0);
        assert!(sonnet.supports_adaptive_thinking);
    }

    #[test]
    fn anthropic_4_6_and_4_7_models_advertise_adaptive_thinking() {
        let models = anthropic_models();
        let by_id: std::collections::HashMap<_, _> =
            models.iter().map(|m| (m.id.as_str(), m)).collect();
        for id in ["claude-opus-4-6", "claude-opus-4-7", "claude-sonnet-4-6"] {
            let model = by_id
                .get(id)
                .unwrap_or_else(|| panic!("missing model: {id}"));
            assert!(
                model.supports_adaptive_thinking,
                "{id} should advertise adaptive thinking"
            );
        }
        for id in ["claude-opus-4-5", "claude-haiku-4-5"] {
            let model = by_id
                .get(id)
                .unwrap_or_else(|| panic!("missing model: {id}"));
            assert!(
                !model.supports_adaptive_thinking,
                "{id} should not advertise adaptive thinking"
            );
        }
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
    fn gpt_5_5_codex_ships_at_input_budget() {
        let gpt55 = openai_codex_models()
            .into_iter()
            .find(|m| m.id.as_str() == "gpt-5.5")
            .expect("gpt-5.5 in codex catalogue");
        assert_eq!(gpt55.context_window, TokenCount::new(272_000));
        assert_eq!(gpt55.effective_context_window, None);
        assert_eq!(gpt55.compaction_window(), TokenCount::new(272_000));
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
    fn find_new_opus_4_5_model() {
        let model = find_model_by_id("claude-opus-4-5");
        assert!(model.is_some());
        assert_eq!(model.unwrap().name, "Claude Opus 4.5");
    }

    #[test]
    fn find_new_opus_4_7_model() {
        let model = find_model_by_id("claude-opus-4-7").unwrap();
        assert_eq!(model.name, "Claude Opus 4.7");
        assert_eq!(model.context_window, TokenCount::new(1_000_000));
        assert_eq!(model.max_output_tokens, TokenCount::new(128_000));
        assert_eq!(model.cost.input, 5.0);
        assert_eq!(model.cost.output, 25.0);
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(4096),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(8192),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(4096),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(4096),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        }];
        crate::private_io::write_atomic(&path, serde_json::to_string(&models).unwrap()).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let loaded: Vec<Model> = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id.as_str(), "custom/test");
        assert_eq!(loaded[0].name, "Custom Test");
    }

    #[test]
    fn cached_user_models_reload_after_file_change() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _guard = set_models_dir(dir.path());
        let path = dir.path().join("models.json");

        let first = vec![Model {
            id: "custom/test".into(),
            name: "First".into(),
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
            effective_context_window: None,
            max_output_tokens: TokenCount::new(4096),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        }];
        crate::private_io::write_atomic(&path, serde_json::to_string(&first).unwrap()).unwrap();
        assert_eq!(find_model_by_id("custom/test").unwrap().name, "First");

        std::thread::sleep(std::time::Duration::from_millis(20));

        let second = vec![Model {
            name: "Second".into(),
            ..first[0].clone()
        }];
        crate::private_io::write_atomic(&path, serde_json::to_string(&second).unwrap()).unwrap();

        assert_eq!(find_model_by_id("custom/test").unwrap().name, "Second");
    }

    #[test]
    fn user_models_path_uses_env() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mush/models.json");
        assert!(path.to_str().unwrap().ends_with("mush/models.json"));
    }

    #[test]
    fn calculate_cost_works() {
        let model = anthropic_models().into_iter().next().unwrap();
        let usage = Usage {
            input_tokens: TokenCount::new(1_000_000),
            output_tokens: TokenCount::new(500_000),
            cache_read_tokens: TokenCount::new(100_000),
            cache_write_tokens: TokenCount::new(50_000),
        };
        let cost = calculate_cost(&model, &usage);
        assert!((cost.input.get() - 4.0).abs() < f64::EPSILON);
        assert!((cost.output.get() - 10.0).abs() < f64::EPSILON);
        assert!(cost.total() > Dollars::ZERO);
    }
}
