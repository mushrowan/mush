pub mod anthropic;
pub mod openai;

use crate::registry::ApiRegistry;

/// register all built-in api providers
pub fn register_builtins(registry: &mut ApiRegistry) {
    registry.register(Box::new(anthropic::AnthropicProvider));
    registry.register(Box::new(openai::OpenaiCompletionsProvider));
}
