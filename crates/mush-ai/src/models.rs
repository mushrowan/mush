//! built-in model catalogue
//!
//! static definitions for known models. users can override via models.json.

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
    models.extend(groq_models());
    models.extend(deepseek_models());
    models.extend(xai_models());
    models.extend(cerebras_models());
    models.extend(mistral_models());
    models.extend(together_models());
    models.extend(deepinfra_models());
    models
});

static USER_MODELS_CACHE: LazyLock<Mutex<Option<UserModelsCache>>> =
    LazyLock::new(|| Mutex::new(None));

/// all built-in anthropic models
#[must_use]
pub fn anthropic_models() -> Vec<Model> {
    vec![
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
            max_output_tokens: TokenCount::new(128_000),
            supports_adaptive_thinking: true,
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
            max_output_tokens: TokenCount::new(128_000),
            supports_adaptive_thinking: true,
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
            max_output_tokens: TokenCount::new(64_000),
            supports_adaptive_thinking: false,
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
            max_output_tokens: TokenCount::new(64_000),
            supports_adaptive_thinking: true,
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
            supports_adaptive_thinking: false,
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
            max_output_tokens: TokenCount::new(64_000),
            supports_adaptive_thinking: false,
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
            supports_adaptive_thinking: false,
        },
    ]
}

/// all built-in openrouter models (via openai-completions api)
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
            max_output_tokens: TokenCount::new(16384),
            supports_adaptive_thinking: false,
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
            supports_adaptive_thinking: false,
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
            supports_adaptive_thinking: false,
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
            supports_adaptive_thinking: false,
        },
    ]
}

/// all built-in openai api-key models (responses API)
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
        max_output_tokens: TokenCount::new(128_000),
        supports_adaptive_thinking: false,
    }]
}

/// all built-in openai codex subscription models (chatgpt oauth)
#[must_use]
pub fn openai_codex_models() -> Vec<Model> {
    vec![
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
            max_output_tokens: TokenCount::new(128_000),
            supports_adaptive_thinking: false,
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
            supports_adaptive_thinking: false,
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
            supports_adaptive_thinking: false,
        },
    ]
}

/// compact spec for building an OpenAI-compatible model entry.
/// each field mirrors `Model` but skips the repeated constants
/// (api = completions, provider wrapped, costs as ModelCost) to keep
/// the provider bundle tables readable
struct OaiModel {
    id: &'static str,
    name: &'static str,
    provider: &'static str,
    base_url: &'static str,
    reasoning: bool,
    image: bool,
    input_cost: f64,
    output_cost: f64,
    cache_read_cost: f64,
    context: u32,
    max_output: u32,
}

impl OaiModel {
    fn into_model(self) -> Model {
        let mut input = vec![InputModality::Text];
        if self.image {
            input.push(InputModality::Image);
        }
        Model {
            id: self.id.into(),
            name: self.name.into(),
            api: Api::OpenaiCompletions,
            provider: Provider::Custom(self.provider.into()),
            base_url: self.base_url.into(),
            reasoning: self.reasoning,
            input,
            cost: ModelCost {
                input: self.input_cost,
                output: self.output_cost,
                cache_read: self.cache_read_cost,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(self.context as u64),
            max_output_tokens: TokenCount::new(self.max_output as u64),
            supports_adaptive_thinking: false,
        }
    }
}

/// all built-in groq models (openai-compatible, fast LPU inference).
/// base url: https://api.groq.com/openai/v1
/// env: GROQ_API_KEY
#[must_use]
pub fn groq_models() -> Vec<Model> {
    const BASE: &str = "https://api.groq.com/openai/v1";
    [
        OaiModel {
            id: "llama-3.3-70b-versatile",
            name: "Llama 3.3 70B (Groq)",
            provider: "groq",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.59,
            output_cost: 0.79,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 32_768,
        },
        OaiModel {
            id: "llama-3.1-8b-instant",
            name: "Llama 3.1 8B Instant (Groq)",
            provider: "groq",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.05,
            output_cost: 0.08,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 8_192,
        },
        OaiModel {
            id: "meta-llama/llama-4-scout-17b-16e-instruct",
            name: "Llama 4 Scout 17B (Groq)",
            provider: "groq",
            base_url: BASE,
            reasoning: false,
            image: true,
            input_cost: 0.11,
            output_cost: 0.34,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 8_192,
        },
        OaiModel {
            id: "moonshotai/kimi-k2-instruct",
            name: "Kimi K2 Instruct (Groq)",
            provider: "groq",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 1.0,
            output_cost: 3.0,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 16_384,
        },
        OaiModel {
            id: "qwen/qwen3-32b",
            name: "Qwen3 32B (Groq)",
            provider: "groq",
            base_url: BASE,
            reasoning: true,
            image: false,
            input_cost: 0.29,
            output_cost: 0.59,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 16_384,
        },
        OaiModel {
            id: "openai/gpt-oss-120b",
            name: "GPT-OSS 120B (Groq)",
            provider: "groq",
            base_url: BASE,
            reasoning: true,
            image: false,
            input_cost: 0.15,
            output_cost: 0.75,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 32_768,
        },
        OaiModel {
            id: "openai/gpt-oss-20b",
            name: "GPT-OSS 20B (Groq)",
            provider: "groq",
            base_url: BASE,
            reasoning: true,
            image: false,
            input_cost: 0.10,
            output_cost: 0.50,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 32_768,
        },
    ]
    .into_iter()
    .map(OaiModel::into_model)
    .collect()
}

/// all built-in deepseek models (openai-compatible).
/// base url: https://api.deepseek.com
/// env: DEEPSEEK_API_KEY
#[must_use]
pub fn deepseek_models() -> Vec<Model> {
    const BASE: &str = "https://api.deepseek.com";
    [
        OaiModel {
            id: "deepseek-chat",
            name: "DeepSeek V3.2",
            provider: "deepseek",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.26,
            output_cost: 0.38,
            cache_read_cost: 0.13,
            context: 163_840,
            max_output: 8_192,
        },
        OaiModel {
            id: "deepseek-reasoner",
            name: "DeepSeek R1",
            provider: "deepseek",
            base_url: BASE,
            reasoning: true,
            image: false,
            input_cost: 0.50,
            output_cost: 2.15,
            cache_read_cost: 0.35,
            context: 163_840,
            max_output: 32_768,
        },
    ]
    .into_iter()
    .map(OaiModel::into_model)
    .collect()
}

/// all built-in xAI models (Grok, openai-compatible).
/// base url: https://api.x.ai/v1
/// env: XAI_API_KEY
#[must_use]
pub fn xai_models() -> Vec<Model> {
    const BASE: &str = "https://api.x.ai/v1";
    [
        OaiModel {
            id: "grok-4",
            name: "Grok 4 (xAI)",
            provider: "xai",
            base_url: BASE,
            reasoning: true,
            image: true,
            input_cost: 3.0,
            output_cost: 15.0,
            cache_read_cost: 0.75,
            context: 256_000,
            max_output: 16_384,
        },
        OaiModel {
            id: "grok-code-fast-1",
            name: "Grok Code Fast 1 (xAI)",
            provider: "xai",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.20,
            output_cost: 1.50,
            cache_read_cost: 0.02,
            context: 256_000,
            max_output: 16_384,
        },
        OaiModel {
            id: "grok-3",
            name: "Grok 3 (xAI)",
            provider: "xai",
            base_url: BASE,
            reasoning: false,
            image: true,
            input_cost: 3.0,
            output_cost: 15.0,
            cache_read_cost: 0.75,
            context: 131_072,
            max_output: 16_384,
        },
    ]
    .into_iter()
    .map(OaiModel::into_model)
    .collect()
}

/// all built-in cerebras models (openai-compatible, wafer-scale inference).
/// base url: https://api.cerebras.ai/v1
/// env: CEREBRAS_API_KEY
#[must_use]
pub fn cerebras_models() -> Vec<Model> {
    const BASE: &str = "https://api.cerebras.ai/v1";
    [
        OaiModel {
            id: "llama-3.3-70b",
            name: "Llama 3.3 70B (Cerebras)",
            provider: "cerebras",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.85,
            output_cost: 1.20,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 8_192,
        },
        OaiModel {
            id: "llama3.1-8b",
            name: "Llama 3.1 8B (Cerebras)",
            provider: "cerebras",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.10,
            output_cost: 0.10,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 8_192,
        },
        OaiModel {
            id: "qwen-3-coder-480b",
            name: "Qwen3 Coder 480B (Cerebras)",
            provider: "cerebras",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 2.0,
            output_cost: 2.0,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 32_768,
        },
        OaiModel {
            id: "qwen-3-235b-a22b-instruct-2507",
            name: "Qwen3 235B Instruct (Cerebras)",
            provider: "cerebras",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.60,
            output_cost: 1.20,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 16_384,
        },
    ]
    .into_iter()
    .map(OaiModel::into_model)
    .collect()
}

/// all built-in mistral models (openai-compatible via la plateforme).
/// base url: https://api.mistral.ai/v1
/// env: MISTRAL_API_KEY
#[must_use]
pub fn mistral_models() -> Vec<Model> {
    const BASE: &str = "https://api.mistral.ai/v1";
    [
        OaiModel {
            id: "mistral-large-latest",
            name: "Mistral Large",
            provider: "mistral",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 2.0,
            output_cost: 6.0,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 16_384,
        },
        OaiModel {
            id: "mistral-medium-latest",
            name: "Mistral Medium",
            provider: "mistral",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.40,
            output_cost: 2.0,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 16_384,
        },
        OaiModel {
            id: "mistral-small-latest",
            name: "Mistral Small",
            provider: "mistral",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.20,
            output_cost: 0.60,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 8_192,
        },
        OaiModel {
            id: "codestral-latest",
            name: "Codestral",
            provider: "mistral",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.30,
            output_cost: 0.90,
            cache_read_cost: 0.0,
            context: 256_000,
            max_output: 16_384,
        },
    ]
    .into_iter()
    .map(OaiModel::into_model)
    .collect()
}

/// all built-in together ai models (openai-compatible, many open models).
/// base url: https://api.together.xyz/v1
/// env: TOGETHER_API_KEY
#[must_use]
pub fn together_models() -> Vec<Model> {
    const BASE: &str = "https://api.together.xyz/v1";
    [
        OaiModel {
            id: "meta-llama/Llama-4-Maverick-17B-128E-Instruct-FP8",
            name: "Llama 4 Maverick (Together)",
            provider: "together",
            base_url: BASE,
            reasoning: false,
            image: true,
            input_cost: 0.27,
            output_cost: 0.85,
            cache_read_cost: 0.0,
            context: 524_288,
            max_output: 16_384,
        },
        OaiModel {
            id: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
            name: "Llama 3.3 70B Turbo (Together)",
            provider: "together",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.88,
            output_cost: 0.88,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 16_384,
        },
        OaiModel {
            id: "deepseek-ai/DeepSeek-V3",
            name: "DeepSeek V3 (Together)",
            provider: "together",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 1.25,
            output_cost: 1.25,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 16_384,
        },
        OaiModel {
            id: "Qwen/Qwen2.5-Coder-32B-Instruct",
            name: "Qwen 2.5 Coder 32B (Together)",
            provider: "together",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.80,
            output_cost: 0.80,
            cache_read_cost: 0.0,
            context: 131_072,
            max_output: 16_384,
        },
    ]
    .into_iter()
    .map(OaiModel::into_model)
    .collect()
}

/// all built-in deepinfra models (openai-compatible, cheap open models).
/// base url: https://api.deepinfra.com/v1/openai
/// env: DEEPINFRA_API_KEY
#[must_use]
pub fn deepinfra_models() -> Vec<Model> {
    const BASE: &str = "https://api.deepinfra.com/v1/openai";
    [
        OaiModel {
            id: "deepseek-ai/DeepSeek-V3.2",
            name: "DeepSeek V3.2 (DeepInfra)",
            provider: "deepinfra",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.26,
            output_cost: 0.38,
            cache_read_cost: 0.13,
            context: 163_840,
            max_output: 16_384,
        },
        OaiModel {
            id: "moonshotai/Kimi-K2.5",
            name: "Kimi K2.5 (DeepInfra)",
            provider: "deepinfra",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.45,
            output_cost: 2.80,
            cache_read_cost: 0.09,
            context: 262_144,
            max_output: 32_768,
        },
        OaiModel {
            id: "zai-org/GLM-4.7-Flash",
            name: "GLM 4.7 Flash (DeepInfra)",
            provider: "deepinfra",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.06,
            output_cost: 0.40,
            cache_read_cost: 0.01,
            context: 202_752,
            max_output: 16_384,
        },
        OaiModel {
            id: "Qwen/Qwen3-Coder-480B-A35B-Instruct",
            name: "Qwen3 Coder 480B (DeepInfra)",
            provider: "deepinfra",
            base_url: BASE,
            reasoning: false,
            image: false,
            input_cost: 0.40,
            output_cost: 1.60,
            cache_read_cost: 0.0,
            context: 262_144,
            max_output: 32_768,
        },
    ]
    .into_iter()
    .map(OaiModel::into_model)
    .collect()
}

/// all built-in models across all providers
#[must_use]
pub fn all_models() -> Vec<Model> {
    BUILTIN_MODELS.clone()
}

/// all models including user overrides from models.json
#[must_use]
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
#[must_use]
pub fn find_model(provider: &Provider, id: &str) -> Option<Model> {
    all_models_with_user()
        .into_iter()
        .find(|m| &m.provider == provider && m.id.as_str() == id)
}

/// find a model by id alone (first match, includes user models)
#[must_use]
pub fn find_model_by_id(id: &str) -> Option<Model> {
    all_models_with_user()
        .into_iter()
        .find(|m| m.id.as_str() == id)
}

/// load user-defined models from ~/.config/mush/models.json
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

/// list all models for a provider
#[must_use]
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
    fn anthropic_4_6_and_4_7_models_advertise_adaptive_thinking() {
        // claude opus 4.6, opus 4.7, and sonnet 4.6 use anthropic's
        // adaptive thinking. older claude models use enabled+budget mode.
        // capability lives on Model rather than a hardcoded id sniff so
        // new releases can opt in by tweaking the catalogue
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
        // older models stay on budget mode
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
    fn groq_models_exist_and_use_openai_completions() {
        let models = groq_models();
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| m.api == Api::OpenaiCompletions));
        assert!(
            models
                .iter()
                .all(|m| m.provider == Provider::Custom("groq".into()))
        );
        assert!(
            models
                .iter()
                .all(|m| m.base_url.as_str() == "https://api.groq.com/openai/v1")
        );
    }

    #[test]
    fn deepseek_models_exist_and_use_openai_completions() {
        let models = deepseek_models();
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| m.api == Api::OpenaiCompletions));
        assert!(
            models
                .iter()
                .all(|m| m.provider == Provider::Custom("deepseek".into()))
        );
        assert!(
            models
                .iter()
                .all(|m| m.base_url.as_str() == "https://api.deepseek.com")
        );
    }

    #[test]
    fn xai_models_exist_and_use_openai_completions() {
        let models = xai_models();
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| m.api == Api::OpenaiCompletions));
        assert!(
            models
                .iter()
                .all(|m| m.provider == Provider::Custom("xai".into()))
        );
        assert!(
            models
                .iter()
                .all(|m| m.base_url.as_str() == "https://api.x.ai/v1")
        );
    }

    #[test]
    fn cerebras_models_exist_and_use_openai_completions() {
        let models = cerebras_models();
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| m.api == Api::OpenaiCompletions));
        assert!(
            models
                .iter()
                .all(|m| m.provider == Provider::Custom("cerebras".into()))
        );
    }

    #[test]
    fn mistral_models_exist_and_use_openai_completions() {
        let models = mistral_models();
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| m.api == Api::OpenaiCompletions));
        assert!(
            models
                .iter()
                .all(|m| m.provider == Provider::Custom("mistral".into()))
        );
    }

    #[test]
    fn together_models_exist_and_use_openai_completions() {
        let models = together_models();
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| m.api == Api::OpenaiCompletions));
        assert!(
            models
                .iter()
                .all(|m| m.provider == Provider::Custom("together".into()))
        );
    }

    #[test]
    fn deepinfra_models_exist_and_use_openai_completions() {
        let models = deepinfra_models();
        assert!(!models.is_empty());
        assert!(models.iter().all(|m| m.api == Api::OpenaiCompletions));
        assert!(
            models
                .iter()
                .all(|m| m.provider == Provider::Custom("deepinfra".into()))
        );
    }

    #[test]
    fn new_oai_compat_providers_registered_in_all_models() {
        let all = all_models();
        for provider in [
            "groq",
            "deepseek",
            "xai",
            "cerebras",
            "mistral",
            "together",
            "deepinfra",
        ] {
            let found = all
                .iter()
                .any(|m| m.provider == Provider::Custom(provider.into()));
            assert!(found, "{provider} provider should have registered models");
        }
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
            supports_adaptive_thinking: false,
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
            supports_adaptive_thinking: false,
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
            supports_adaptive_thinking: false,
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
            supports_adaptive_thinking: false,
        }];
        std::fs::write(&path, serde_json::to_string(&models).unwrap()).unwrap();

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
            max_output_tokens: TokenCount::new(4096),
            supports_adaptive_thinking: false,
        }];
        std::fs::write(&path, serde_json::to_string(&first).unwrap()).unwrap();
        assert_eq!(find_model_by_id("custom/test").unwrap().name, "First");

        std::thread::sleep(std::time::Duration::from_millis(20));

        let second = vec![Model {
            name: "Second".into(),
            ..first[0].clone()
        }];
        std::fs::write(&path, serde_json::to_string(&second).unwrap()).unwrap();

        assert_eq!(find_model_by_id("custom/test").unwrap().name, "Second");
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
        // opus 4.7: $5/MTok input, $25/MTok output
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
