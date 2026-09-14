use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mush_agent::tool::{AgentTool, OutputLimit, ToolRegistry, ToolResult};
use mush_ai::registry::{ApiProvider, ApiRegistry, LlmContext, ProviderError};
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;

struct FakeProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ApiProvider for FakeProvider {
    fn api(&self) -> Api {
        Api::AnthropicMessages
    }

    async fn stream(
        &self,
        model: &Model,
        _: &LlmContext,
        _: &StreamOptions,
    ) -> Result<mush_ai::EventStream, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(call < 2);
        let content = if call == 0 {
            vec![
                AssistantContentPart::Text(TextContent {
                    text: "fake stream before tool".into(),
                }),
                AssistantContentPart::ToolCall(ToolCall {
                    id: "fake-call".into(),
                    name: "synthetic-tool".into(),
                    arguments: serde_json::json!({}),
                }),
            ]
        } else {
            vec![AssistantContentPart::Text(TextContent {
                text: "fake stream completed".into(),
            })]
        };
        let reason = if call == 0 {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
        let message = AssistantMessage::builder()
            .model(model.id.clone())
            .provider(Provider::Custom("synthetic".into()))
            .api(Api::AnthropicMessages)
            .content(content)
            .stop_reason(reason)
            .timestamp_ms(Timestamp::zero())
            .build();
        let events = vec![
            StreamEvent::TextDelta {
                content_index: 0,
                delta: message.text(),
            },
            StreamEvent::Done { reason, message },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct FakeTool(PathBuf);

#[async_trait::async_trait]
impl AgentTool for FakeTool {
    fn name(&self) -> &str {
        "synthetic-tool"
    }
    fn label(&self) -> &str {
        "synthetic-tool"
    }
    fn description(&self) -> &str {
        "synthetic fixture"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{}})
    }
    fn output_limit(&self) -> OutputLimit {
        OutputLimit::Head
    }
    async fn execute(&self, _: serde_json::Value) -> ToolResult {
        std::fs::write(self.0.join("tool-started"), "started").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(40), async {
            while !self.0.join("finish-tool").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        ToolResult::text("synthetic tool finished")
    }
}

#[tokio::test]
#[ignore]
async fn subagent_panel_pty_fixture() {
    let dir = PathBuf::from(
        std::env::var_os("MUSH_PANEL_FIXTURE").expect("synthetic fixture directory required"),
    );
    let mut config = super::TuiConfig::for_test();
    config.cwd = dir.clone();
    config.session_id = "synthetic-panel-session".into();
    config.model.id = "synthetic-panel-model".into();
    config.model.name = "synthetic-panel-model".into();
    config.model.provider = Provider::Custom("synthetic".into());
    config.model.api = Api::AnthropicMessages;
    config.model.base_url = "http://invalid.invalid".into();
    config.provider_api_keys.insert(
        "synthetic".into(),
        mush_ai::ApiKey::new("synthetic").unwrap(),
    );
    config.terminal_policy.image_probe = crate::ImageProbeMode::Disabled;
    config.terminal_policy.keyboard_enhancement = crate::KeyboardEnhancementMode::Disabled;
    config.max_turns = 3;
    if std::env::var_os("MUSH_PANEL_REMAP").is_some() {
        let keys: crate::keybinds::KeysConfig =
            toml::from_str("[shared]\ntoggle_subagents = 'ctrl+p'").unwrap();
        assert!(config.keymap.override_scoped(&keys.scoped()).is_empty());
    }
    let saved_dir = dir.clone();
    config.save_session = Some(Arc::new(move |snapshot| {
        let value = serde_json::json!({"session":snapshot.session_id.to_string(),"messages":snapshot.primary.context()});
        std::fs::write(saved_dir.join("saved.json"), value.to_string()).unwrap();
    }));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(FakeProvider {
        calls: Arc::clone(&calls),
    }));
    let mut tools = ToolRegistry::new();
    tools.register_shared(Arc::new(FakeTool(dir.clone())));
    crate::run_tui(config, &tools, &registry, None)
        .await
        .unwrap();
    std::fs::write(
        dir.join("provider-calls"),
        calls.load(Ordering::SeqCst).to_string(),
    )
    .unwrap();
}
