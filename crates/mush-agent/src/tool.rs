//! agent tool trait and result types

use std::collections::HashMap;
use std::sync::Arc;

use mush_ai::types::{
    ImageContent, ImageMimeType, TextContent, ToolOutcome, ToolResultContentPart,
};
use serde::de::DeserializeOwned;

/// result of executing an agent tool
#[derive(Debug, Clone)]
#[must_use]
pub struct ToolResult {
    pub content: Vec<ToolResultContentPart>,
    pub outcome: ToolOutcome,
}

impl ToolResult {
    /// convenience constructor for a text result
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContentPart::Text(TextContent {
                text: text.into(),
            })],
            outcome: ToolOutcome::Success,
        }
    }

    /// convenience constructor for an error result
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContentPart::Text(TextContent {
                text: text.into(),
            })],
            outcome: ToolOutcome::Error,
        }
    }

    /// convenience constructor for an image result
    pub fn image(data: String, mime_type: ImageMimeType) -> Self {
        Self {
            content: vec![ToolResultContentPart::Image(ImageContent {
                data,
                mime_type,
            })],
            outcome: ToolOutcome::Success,
        }
    }
}

/// trait for tools that the agent can invoke
///
/// tools are given a json arguments object and return a result.
/// they can be cancelled via the abort signal.
pub trait AgentTool: Send + Sync {
    /// unique name for this tool
    fn name(&self) -> &str;

    /// human-readable label for UI display
    fn label(&self) -> &str;

    /// description for the LLM
    fn description(&self) -> &str;

    /// json schema for the parameters
    fn parameters_schema(&self) -> serde_json::Value;

    /// execute the tool with the given arguments
    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>>;
}

pub type SharedTool = Arc<dyn AgentTool>;

pub fn parse_tool_args<T>(args: serde_json::Value) -> Result<T, ToolResult>
where
    T: DeserializeOwned,
{
    serde_json::from_value(args)
        .map_err(|error| ToolResult::error(format!("invalid arguments: {error}")))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolKey(String);

impl ToolKey {
    pub fn new(name: &str) -> Self {
        Self(name.to_lowercase().replace('_', ""))
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

    #[must_use]
    pub fn with_shared(&self, tools: impl IntoIterator<Item = SharedTool>) -> Self {
        let mut merged = self.clone();
        merged.extend_shared(tools);
        merged
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
    fn tool_result_error() {
        let result = ToolResult::error("something went wrong");
        assert!(result.outcome.is_error());
    }

    struct EchoTool;

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
        fn execute(
            &self,
            args: serde_json::Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
            Box::pin(async move {
                let text = args["text"].as_str().unwrap_or("no text");
                ToolResult::text(text)
            })
        }
    }

    #[tokio::test]
    async fn echo_tool_works() {
        let tool = EchoTool;
        let result = tool.execute(serde_json::json!({"text": "hello"})).await;
        assert!(result.outcome.is_success());
        match &result.content[0] {
            ToolResultContentPart::Text(t) => assert_eq!(t.text, "hello"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn tool_key_normalises_case_and_underscores() {
        assert_eq!(ToolKey::new("WebSearch"), ToolKey::new("web_search"));
    }

    #[test]
    fn registry_lookup_uses_normalised_key() {
        let registry = ToolRegistry::from_shared(vec![Arc::new(EchoTool) as SharedTool]);
        assert!(registry.get("echo").is_some());
        assert!(registry.get("Echo").is_some());
    }

    struct UpperEchoTool;

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
        fn execute(
            &self,
            args: serde_json::Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
            Box::pin(async move {
                let text = args["text"].as_str().unwrap_or("no text");
                ToolResult::text(text.to_uppercase())
            })
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
