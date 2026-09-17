use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mush_agent::tool::ToolRegistry;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::{
    Api, AssistantContentPart, AssistantMessage, Message, Model, StopReason, StreamOptions,
    TextContent, Timestamp,
};
use mush_session::ConversationState;

use crate::connection::InProcessConnection;
use crate::session::SessionRuntime;
use crate::turn::TurnConfig;

pub type Requests = Arc<Mutex<Vec<Vec<Message>>>>;

pub struct Echo {
    pub requests: Requests,
    pub reply: String,
}

#[async_trait::async_trait]
impl mush_ai::registry::ApiProvider for Echo {
    fn api(&self) -> Api {
        Api::AnthropicMessages
    }

    async fn stream(
        &self,
        model: &Model,
        context: &mush_ai::registry::LlmContext,
        _options: &StreamOptions,
    ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(context.messages.clone());
        }
        let message = AssistantMessage::builder()
            .content(vec![AssistantContentPart::Text(TextContent {
                text: self.reply.clone(),
            })])
            .model(model.id.clone())
            .provider(model.provider.clone())
            .api(model.api)
            .stop_reason(StopReason::Stop)
            .timestamp_ms(Timestamp::zero())
            .build();
        Ok(Box::pin(futures::stream::iter([
            mush_ai::stream::StreamEvent::TextDelta {
                content_index: 0,
                delta: self.reply.clone(),
            },
            mush_ai::stream::StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            },
        ])))
    }
}

#[must_use]
pub fn echo_model() -> Model {
    mush_ai::models::all_models()
        .into_iter()
        .find(|model| model.api == Api::AnthropicMessages)
        .unwrap_or_else(|| unreachable!("the catalogue holds an anthropic model"))
}

#[must_use]
pub fn echo_config(model: Model) -> TurnConfig {
    TurnConfig {
        default_model: model,
        options: StreamOptions::default(),
        max_turns: 4,
        provider_api_keys: HashMap::new(),
        confirm_tools: false,
        auto_compact: false,
        compaction_model: None,
        forbidden_chars: Default::default(),
        tool_allowlist: None,
        generate_title: false,
    }
}

#[must_use]
pub fn echo_connection() -> (InProcessConnection, Requests) {
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(Echo {
        requests: requests.clone(),
        reply: "hi back".into(),
    }));
    let model = echo_model();
    let mut runtime = SessionRuntime::new(ConversationState::new());
    runtime.model_id = model.id.to_string();
    let connection = InProcessConnection::new(
        runtime,
        ToolRegistry::new(),
        registry,
        echo_config(model),
        mush_agent::AgentInjections::default(),
    );
    (connection, requests)
}
