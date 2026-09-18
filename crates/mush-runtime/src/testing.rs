use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mush_agent::tool::ToolRegistry;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::{
    Api, AssistantContentPart, AssistantMessage, Model, StopReason, StreamOptions, TextContent,
    Timestamp,
};
use mush_session::ConversationState;

use crate::connection::InProcessConnection;
use crate::session::SessionRuntime;
use crate::turn::TurnConfig;

pub type Requests = Arc<Mutex<Vec<mush_ai::registry::LlmContext>>>;

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
            requests.push(context.clone());
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
        compaction: Default::default(),
        branch_summary: Default::default(),
        auto_refine: Default::default(),
        auto_fork_compact: false,
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

pub struct ToolThenText {
    pub requests: Requests,
    pub gate: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl mush_ai::registry::ApiProvider for ToolThenText {
    fn api(&self) -> Api {
        Api::AnthropicMessages
    }

    async fn stream(
        &self,
        model: &Model,
        context: &mush_ai::registry::LlmContext,
        _options: &StreamOptions,
    ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
        let first = {
            let mut requests = self.requests.lock().unwrap_or_else(|e| e.into_inner());
            requests.push(context.clone());
            requests.len() == 1
        };
        let content = if first {
            vec![AssistantContentPart::ToolCall(mush_ai::types::ToolCall {
                id: mush_ai::types::ToolCallId::from("call_1"),
                name: mush_ai::types::ToolName::from("note"),
                arguments: serde_json::json!({}),
            })]
        } else {
            vec![AssistantContentPart::Text(TextContent {
                text: "done".into(),
            })]
        };
        let message = AssistantMessage::builder()
            .content(content)
            .model(model.id.clone())
            .provider(model.provider.clone())
            .api(model.api)
            .stop_reason(if first {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            })
            .timestamp_ms(Timestamp::zero())
            .build();
        if first {
            self.gate.notified().await;
        }
        Ok(Box::pin(futures::stream::iter([
            mush_ai::stream::StreamEvent::Done {
                reason: message.stop_reason,
                message,
            },
        ])))
    }
}

pub struct NoteTool;

#[async_trait::async_trait]
impl mush_agent::AgentTool for NoteTool {
    fn name(&self) -> &str {
        "note"
    }
    fn label(&self) -> &str {
        "Note"
    }
    fn description(&self) -> &str {
        "keeps a note"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: serde_json::Value) -> mush_agent::ToolResult {
        mush_agent::ToolResult::text("noted")
    }
    fn output_limit(&self) -> mush_agent::OutputLimit {
        mush_agent::OutputLimit::Head
    }
}

#[must_use]
pub fn tool_then_text_connection() -> (InProcessConnection, Requests, Arc<tokio::sync::Notify>) {
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(ToolThenText {
        requests: requests.clone(),
        gate: gate.clone(),
    }));
    let model = echo_model();
    let mut runtime = SessionRuntime::new(ConversationState::new());
    runtime.model_id = model.id.to_string();
    let mut tools = ToolRegistry::new();
    tools.register_shared(Arc::new(NoteTool));
    let connection = InProcessConnection::new(
        runtime,
        tools,
        registry,
        echo_config(model),
        mush_agent::AgentInjections::default(),
    );
    (connection, requests, gate)
}

pub struct Stall {
    pub release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl mush_ai::registry::ApiProvider for Stall {
    fn api(&self) -> Api {
        Api::AnthropicMessages
    }

    async fn stream(
        &self,
        model: &Model,
        _context: &mush_ai::registry::LlmContext,
        _options: &StreamOptions,
    ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
        use futures::StreamExt;
        let message = AssistantMessage::builder()
            .content(vec![AssistantContentPart::Text(TextContent {
                text: "partial answer".into(),
            })])
            .model(model.id.clone())
            .provider(model.provider.clone())
            .api(model.api)
            .stop_reason(StopReason::Stop)
            .timestamp_ms(Timestamp::zero())
            .build();
        let release = Arc::clone(&self.release);
        let rest = futures::stream::once(async move {
            release.notified().await;
            mush_ai::stream::StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            }
        });
        Ok(Box::pin(
            futures::stream::iter([mush_ai::stream::StreamEvent::TextDelta {
                content_index: 0,
                delta: "partial".into(),
            }])
            .chain(rest),
        ))
    }
}

#[must_use]
pub fn stalled_connection() -> (InProcessConnection, Arc<tokio::sync::Notify>) {
    let release = Arc::new(tokio::sync::Notify::new());
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(Stall {
        release: Arc::clone(&release),
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
    (connection, release)
}
