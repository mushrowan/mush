pub mod anthropic;
pub(crate) mod bench_support;
pub mod openai;
pub mod openai_responses;
pub mod sse;

use crate::registry::ApiRegistry;

/// register all built-in api providers
pub fn register_builtins(registry: &mut ApiRegistry, client: reqwest::Client) {
    registry.register(Box::new(anthropic::AnthropicProvider {
        client: client.clone(),
    }));
    registry.register(Box::new(openai::OpenaiCompletionsProvider {
        client: client.clone(),
    }));
    registry.register(Box::new(openai_responses::OpenaiResponsesProvider {
        client,
    }));
}
