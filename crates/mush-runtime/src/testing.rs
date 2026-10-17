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

pub type SeenBetas = Arc<Mutex<Vec<Option<mush_ai::types::AnthropicBetas>>>>;

pub struct RecordsBetas {
    pub seen: SeenBetas,
}

#[async_trait::async_trait]
impl mush_ai::registry::ApiProvider for RecordsBetas {
    fn api(&self) -> Api {
        Api::AnthropicMessages
    }

    async fn stream(
        &self,
        model: &Model,
        context: &mush_ai::registry::LlmContext,
        options: &StreamOptions,
    ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(options.anthropic_betas.clone());
        }
        Echo {
            requests: Arc::default(),
            reply: "ok".into(),
        }
        .stream(model, context, options)
        .await
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

fn echo_model_connection(registry: ApiRegistry, tools: ToolRegistry) -> InProcessConnection {
    let model = echo_model();
    let mut runtime = SessionRuntime::new(ConversationState::new());
    runtime.model_id = model.id.to_string();
    InProcessConnection::new(
        runtime,
        tools,
        registry,
        echo_config(model),
        mush_agent::AgentInjections::default(),
    )
}

#[must_use]
pub fn echo_connection() -> (InProcessConnection, Requests) {
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(Echo {
        requests: requests.clone(),
        reply: "hi back".into(),
    }));
    let connection = echo_model_connection(registry, ToolRegistry::new());
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

pub struct HangingTool;

#[async_trait::async_trait]
impl mush_agent::AgentTool for HangingTool {
    fn name(&self) -> &str {
        "note"
    }
    fn label(&self) -> &str {
        "Note"
    }
    fn description(&self) -> &str {
        "never returns"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: serde_json::Value) -> mush_agent::ToolResult {
        std::future::pending::<()>().await;
        mush_agent::ToolResult::text("unreachable")
    }
    fn output_limit(&self) -> mush_agent::OutputLimit {
        mush_agent::OutputLimit::Head
    }
}

#[must_use]
pub fn hanging_tool_connection() -> (InProcessConnection, Requests) {
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(tokio::sync::Notify::new());
    gate.notify_one();
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(ToolThenText {
        requests: requests.clone(),
        gate,
    }));
    let mut tools = ToolRegistry::new();
    tools.register_shared(Arc::new(HangingTool));
    let connection = echo_model_connection(registry, tools);
    (connection, requests)
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
    let mut tools = ToolRegistry::new();
    tools.register_shared(Arc::new(NoteTool));
    let connection = echo_model_connection(registry, tools);
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
    let connection = echo_model_connection(registry, ToolRegistry::new());
    (connection, release)
}

pub type Stamps = Arc<Mutex<Vec<std::time::Instant>>>;

pub struct Paced {
    pub deltas: usize,
    pub gap: std::time::Duration,
    pub sent: Stamps,
}

#[async_trait::async_trait]
impl mush_ai::registry::ApiProvider for Paced {
    fn api(&self) -> Api {
        Api::AnthropicMessages
    }

    async fn stream(
        &self,
        model: &Model,
        _context: &mush_ai::registry::LlmContext,
        _options: &StreamOptions,
    ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
        let text: String = (0..self.deltas).map(|index| format!("d{index} ")).collect();
        let message = AssistantMessage::builder()
            .content(vec![AssistantContentPart::Text(TextContent { text })])
            .model(model.id.clone())
            .provider(model.provider.clone())
            .api(model.api)
            .stop_reason(StopReason::Stop)
            .timestamp_ms(Timestamp::zero())
            .build();
        let (deltas, gap, sent) = (self.deltas, self.gap, Arc::clone(&self.sent));
        Ok(Box::pin(futures::stream::unfold(0, move |index| {
            let (message, sent) = (message.clone(), Arc::clone(&sent));
            async move {
                if index > deltas {
                    return None;
                }
                if index == deltas {
                    let done = mush_ai::stream::StreamEvent::Done {
                        reason: StopReason::Stop,
                        message,
                    };
                    return Some((done, index + 1));
                }
                tokio::time::sleep(gap).await;
                if let Ok(mut sent) = sent.lock() {
                    sent.push(std::time::Instant::now());
                }
                let delta = mush_ai::stream::StreamEvent::TextDelta {
                    content_index: 0,
                    delta: format!("d{index} "),
                };
                Some((delta, index + 1))
            }
        })))
    }
}

#[must_use]
pub fn paced_connection(deltas: usize, gap: std::time::Duration) -> (InProcessConnection, Stamps) {
    let sent: Stamps = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(Paced {
        deltas,
        gap,
        sent: Arc::clone(&sent),
    }));
    let connection = echo_model_connection(registry, ToolRegistry::new());
    (connection, sent)
}

#[cfg(test)]
static DIRS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub fn tempdir() -> std::io::Result<tempfile::TempDir> {
    if cfg!(target_os = "macos") {
        tempfile::tempdir_in("/private/tmp")
    } else {
        tempfile::tempdir()
    }
}

#[cfg(test)]
pub struct IsolatedDirs {
    pub data: tempfile::TempDir,
    pub config: tempfile::TempDir,
    previous: [(&'static str, Option<std::ffi::OsString>); 2],
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl IsolatedDirs {
    pub async fn acquire() -> Self {
        let lock = DIRS_LOCK.lock().await;
        let data = tempfile::tempdir().unwrap_or_else(|error| panic!("data tempdir: {error}"));
        let config = tempfile::tempdir().unwrap_or_else(|error| panic!("config tempdir: {error}"));
        let previous = [
            ("MUSH_DATA_DIR", std::env::var_os("MUSH_DATA_DIR")),
            ("MUSH_CONFIG_DIR", std::env::var_os("MUSH_CONFIG_DIR")),
        ];
        unsafe {
            std::env::set_var("MUSH_DATA_DIR", data.path());
            std::env::set_var("MUSH_CONFIG_DIR", config.path());
        }
        Self {
            data,
            config,
            previous,
            _lock: lock,
        }
    }
}

#[cfg(test)]
impl Drop for IsolatedDirs {
    fn drop(&mut self) {
        for (key, value) in &self.previous {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}
