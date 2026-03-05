pub mod anthropic;
pub mod openai;
pub mod openai_responses;
pub mod sse;

use crate::registry::ApiRegistry;

/// register all built-in api providers
pub fn register_builtins(registry: &mut ApiRegistry) {
    registry.register(Box::new(anthropic::AnthropicProvider));
    registry.register(Box::new(openai::OpenaiCompletionsProvider));
    registry.register(Box::new(openai_responses::OpenaiResponsesProvider));
}
