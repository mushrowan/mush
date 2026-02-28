//! built-in model catalogue
//!
//! static definitions for known models. users can override via models.json.

use crate::types::*;

/// all built-in anthropic models
pub fn anthropic_models() -> Vec<Model> {
    vec![
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
            context_window: 200_000,
            max_output_tokens: 16384,
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
            context_window: 200_000,
            max_output_tokens: 32768,
        },
        Model {
            id: "claude-haiku-3-5-20241022".into(),
            name: "Claude 3.5 Haiku".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: false,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 0.8,
                output: 4.0,
                cache_read: 0.08,
                cache_write: 1.0,
            },
            context_window: 200_000,
            max_output_tokens: 8192,
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
            context_window: 200_000,
            max_output_tokens: 16384,
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
            context_window: 200_000,
            max_output_tokens: 32768,
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
            context_window: 1_048_576,
            max_output_tokens: 65536,
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
            context_window: 1_048_576,
            max_output_tokens: 65536,
        },
    ]
}

/// all built-in models across all providers
pub fn all_models() -> Vec<Model> {
    let mut models = Vec::new();
    models.extend(anthropic_models());
    models.extend(openrouter_models());
    models
}

/// find a model by provider and id
pub fn find_model(provider: &Provider, id: &str) -> Option<Model> {
    all_models()
        .into_iter()
        .find(|m| &m.provider == provider && m.id == id)
}

/// find a model by id alone (first match across providers)
pub fn find_model_by_id(id: &str) -> Option<Model> {
    all_models().into_iter().find(|m| m.id == id)
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
    Cost {
        input: (model.cost.input / 1_000_000.0) * usage.input_tokens as f64,
        output: (model.cost.output / 1_000_000.0) * usage.output_tokens as f64,
        cache_read: (model.cost.cache_read / 1_000_000.0) * usage.cache_read_tokens as f64,
        cache_write: (model.cost.cache_write / 1_000_000.0) * usage.cache_write_tokens as f64,
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
        assert!(openrouter.iter().all(|m| m.provider == Provider::OpenRouter));
    }

    #[test]
    fn calculate_cost_works() {
        let model = anthropic_models().into_iter().next().unwrap();
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_read_tokens: 100_000,
            cache_write_tokens: 50_000,
        };
        let cost = calculate_cost(&model, &usage);
        assert!((cost.input - 3.0).abs() < f64::EPSILON);
        assert!((cost.output - 7.5).abs() < f64::EPSILON);
        assert!(cost.total() > 0.0);
    }
}
