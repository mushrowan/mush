use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use mush_ai::types::{
    Api, ImageContent, ImageMimeType, TextContent, Timestamp, ToolCallId, ToolName, ToolOutcome,
    ToolResultContentPart, ToolResultMessage,
};
use serde::de::DeserializeOwned;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[must_use]
pub struct ToolResult {
    pub content: Vec<ToolResultContentPart>,
    pub outcome: ToolOutcome,
    pub details: Option<serde_json::Value>,
}

impl ToolResult {
    fn single(part: ToolResultContentPart, outcome: ToolOutcome) -> Self {
        Self {
            content: vec![part],
            outcome,
            details: None,
        }
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::single(
            ToolResultContentPart::Text(TextContent { text: text.into() }),
            ToolOutcome::Success,
        )
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self::single(
            ToolResultContentPart::Text(TextContent { text: text.into() }),
            ToolOutcome::Error,
        )
    }

    pub fn image(data: String, mime_type: ImageMimeType) -> Self {
        Self::single(
            ToolResultContentPart::Image(ImageContent { data, mime_type }),
            ToolOutcome::Success,
        )
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn into_message(self, tool_call_id: ToolCallId, tool_name: ToolName) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id,
            tool_name,
            content: self.content,
            outcome: self.outcome,
            timestamp_ms: Timestamp::now(),
            details: self.details,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputLimit {
    Head,
    Tail,
}

#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;

    fn label(&self) -> &str;

    fn description(&self) -> &str;

    fn parameters_schema(&self) -> serde_json::Value;

    fn parameters_schema_for(&self, _api: Api) -> serde_json::Value {
        self.parameters_schema()
    }

    fn description_for(&self, _api: Api) -> &str {
        self.description()
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult;

    async fn call(&self, mut args: serde_json::Value) -> ToolResult {
        crate::de::coerce_to_schema(&mut args, &self.parameters_schema());
        self.execute(args).await
    }

    fn output_limit(&self) -> OutputLimit;
}

pub type SharedTool = Arc<dyn AgentTool>;

#[must_use]
pub fn tool_definitions(tools: &ToolRegistry, api: Api) -> Vec<mush_ai::registry::ToolDefinition> {
    tools
        .iter()
        .map(|tool| mush_ai::registry::ToolDefinition {
            name: tool.name().to_string(),
            description: tool.description_for(api).to_string(),
            parameters: tool.parameters_schema_for(api),
        })
        .collect()
}

const PREVIEW_VALUE_CHARS: usize = 40;

pub(crate) fn preview_args(args: &serde_json::Value) -> String {
    match args {
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{k}: {}", truncate_preview(&v.to_string())))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        other => truncate_preview(&other.to_string()).into_owned(),
    }
}

fn truncate_preview(s: &str) -> Cow<'_, str> {
    if s.chars().nth(PREVIEW_VALUE_CHARS).is_none() {
        return Cow::Borrowed(s);
    }
    let head: String = s.chars().take(PREVIEW_VALUE_CHARS).collect();
    Cow::Owned(format!("{head}…"))
}

pub fn parse_tool_args<T>(args: serde_json::Value) -> Result<T, ToolResult>
where
    T: DeserializeOwned,
{
    let preview = preview_args(&args);
    serde_json::from_value(args).map_err(|error| {
        ToolResult::error(format!("invalid arguments: {error}\nreceived: {preview}"))
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolKey(String);

impl ToolKey {
    pub fn new(name: &str) -> Self {
        Self(mush_ai::providers::normalize_tool_name(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ToolKey {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Display for ToolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    order: Vec<ToolKey>,
    tools: HashMap<ToolKey, SharedTool>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_shared(tools: impl IntoIterator<Item = SharedTool>) -> Self {
        let mut registry = Self::new();
        registry.extend_shared(tools);
        registry
    }

    #[must_use]
    pub fn from_boxed(tools: Vec<Box<dyn AgentTool>>) -> Self {
        Self::from_shared(tools.into_iter().map(SharedTool::from))
    }

    pub fn register_shared(&mut self, tool: SharedTool) {
        let key = ToolKey::new(tool.name());
        if !self.tools.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.tools.insert(key, tool);
    }

    pub fn extend_shared(&mut self, tools: impl IntoIterator<Item = SharedTool>) {
        for tool in tools {
            self.register_shared(tool);
        }
    }

    pub fn remove(&mut self, name: &str) -> Option<SharedTool> {
        let key = ToolKey::new(name);
        let removed = self.tools.remove(&key)?;
        self.order.retain(|k| k != &key);
        Some(removed)
    }

    pub fn retain_named<S: AsRef<str>>(&mut self, names: &[S]) {
        let keep: Vec<ToolKey> = names
            .iter()
            .map(|name| ToolKey::new(name.as_ref()))
            .collect();
        self.order.retain(|key| keep.contains(key));
        self.tools.retain(|key, _| keep.contains(key));
    }

    pub fn get(&self, name: &str) -> Option<&SharedTool> {
        let key = ToolKey::new(name);
        self.tools.get(&key)
    }

    pub fn iter(&self) -> impl Iterator<Item = &SharedTool> {
        self.order.iter().filter_map(|key| self.tools.get(key))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_text() {
        let result = ToolResult::text("hello");
        assert!(result.outcome.is_success());
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            ToolResultContentPart::Text(t) => assert_eq!(t.text, "hello"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn parse_tool_args_includes_received_args_preview() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct Expected {
            path: String,
        }

        let wrong = serde_json::json!({
            "patern": "not a path",
            "limit": 42,
        });
        let err = match parse_tool_args::<Expected>(wrong) {
            Ok(_) => panic!("expected parse failure"),
            Err(result) => result,
        };
        let ToolResultContentPart::Text(text) = &err.content[0] else {
            panic!("expected text content in error");
        };
        assert!(
            text.text.contains("invalid arguments"),
            "error should still say 'invalid arguments', got: {}",
            text.text
        );
        assert!(
            text.text.contains("patern"),
            "error should include the keys actually sent, got: {}",
            text.text
        );
        assert!(
            text.text.contains("limit"),
            "error should include all keys, got: {}",
            text.text
        );
    }

    #[test]
    fn parse_tool_args_truncates_long_values_in_preview() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct Expected {
            path: String,
        }

        let big = "x".repeat(500);
        let wrong = serde_json::json!({ "wrong_field": big });
        let err = match parse_tool_args::<Expected>(wrong) {
            Ok(_) => panic!("expected parse failure"),
            Err(result) => result,
        };
        let ToolResultContentPart::Text(text) = &err.content[0] else {
            panic!("expected text content");
        };
        assert!(
            !text.text.contains(&"x".repeat(100)),
            "long value should be truncated, got: {}",
            text.text
        );
        assert!(
            text.text.contains("…") || text.text.contains("..."),
            "truncated value should include an ellipsis marker, got: {}",
            text.text
        );
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl AgentTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn label(&self) -> &str {
            "Echo"
        }
        fn description(&self) -> &str {
            "echoes input"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            })
        }
        async fn execute(&self, args: serde_json::Value) -> ToolResult {
            let text = args["text"].as_str().unwrap_or("no text");
            ToolResult::text(text)
        }
        fn output_limit(&self) -> OutputLimit {
            OutputLimit::Head
        }
    }

    #[test]
    fn tool_key_normalises_case_and_underscores() {
        assert_eq!(ToolKey::new("WebSearch"), ToolKey::new("web_search"));
    }

    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl AgentTool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn label(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "named"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(&self, _args: serde_json::Value) -> ToolResult {
            ToolResult::text(self.0)
        }
        fn output_limit(&self) -> OutputLimit {
            OutputLimit::Head
        }
    }

    #[test]
    fn retain_named_keeps_only_the_listed_tools_in_order() {
        let mut registry = ToolRegistry::from_shared(vec![
            Arc::new(NamedTool("read")) as SharedTool,
            Arc::new(NamedTool("bash")) as SharedTool,
            Arc::new(NamedTool("web_search")) as SharedTool,
        ]);

        registry.retain_named(&["WebSearch", "read", "missing"]);

        let names: Vec<&str> = registry.iter().map(|tool| tool.name()).collect();
        assert_eq!(names, ["read", "web_search"]);
        assert!(registry.get("bash").is_none());
    }

    #[test]
    fn registry_lookup_uses_normalised_key() {
        let registry = ToolRegistry::from_shared(vec![Arc::new(EchoTool) as SharedTool]);
        assert!(registry.get("echo").is_some());
        assert!(registry.get("Echo").is_some());
    }

    struct UpperEchoTool;

    #[async_trait::async_trait]
    impl AgentTool for UpperEchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn label(&self) -> &str {
            "Echo"
        }
        fn description(&self) -> &str {
            "echoes input in uppercase"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            })
        }
        async fn execute(&self, args: serde_json::Value) -> ToolResult {
            let text = args["text"].as_str().unwrap_or("no text");
            ToolResult::text(text.to_uppercase())
        }
        fn output_limit(&self) -> OutputLimit {
            OutputLimit::Head
        }
    }

    #[tokio::test]
    async fn later_registry_entry_overrides_earlier_one() {
        let registry = ToolRegistry::from_shared(vec![
            Arc::new(EchoTool) as SharedTool,
            Arc::new(UpperEchoTool) as SharedTool,
        ]);
        let tool = registry.get("echo").unwrap();
        let result = tool.execute(serde_json::json!({"text": "hello"})).await;
        match &result.content[0] {
            ToolResultContentPart::Text(t) => assert_eq!(t.text, "HELLO"),
            _ => panic!("expected text"),
        }
    }
}
