//! agent tool trait and result types

use std::borrow::Cow;
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

/// how a tool's output should be truncated when it exceeds limits
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputLimit {
    /// keep the start (file reads, directory listings)
    Head,
    /// keep the end (command output, logs)
    Tail,
    /// keep head + tail with gap marker
    #[default]
    Middle,
}

/// trait for tools that the agent can invoke
///
/// tools are given a json arguments object and return a result.
/// they can be cancelled via the abort signal.
#[async_trait::async_trait]
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
    async fn execute(&self, args: serde_json::Value) -> ToolResult;

    /// how this tool's output should be truncated by the agent loop
    fn output_limit(&self) -> OutputLimit {
        OutputLimit::Middle
    }
}

pub type SharedTool = Arc<dyn AgentTool>;

/// max characters shown per field value in the invalid-args preview.
/// short enough to keep the error readable in a terminal, long enough
/// to let the model tell typical paths / strings apart
const PREVIEW_VALUE_CHARS: usize = 40;

/// render a compact preview of the args that failed to parse.
///
/// objects render as `{key: value, key: value}` with each value
/// truncated to [`PREVIEW_VALUE_CHARS`] characters (with an ellipsis
/// marker). non-object values stringify via serde_json directly,
/// truncated the same way. the preview is best-effort debug output,
/// so we don't try to round-trip or re-parse it
fn preview_args(args: &serde_json::Value) -> String {
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
    // `.chars().nth(N).is_some()` is O(N+1) instead of O(len): we only
    // need to know "more than N chars?" not the exact count. returns
    // `Cow::Borrowed` for short inputs to skip a heap allocation
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

    #[test]
    fn parse_tool_args_includes_received_args_preview() {
        // when the model sends args that don't match the schema, include
        // a compact preview of what was actually received so the agent
        // (or a human reading the log) can tell what went wrong at a
        // glance. particularly helpful for models that confuse similar
        // tools and send the wrong shape
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
        // the serde error is still there
        assert!(
            text.text.contains("invalid arguments"),
            "error should still say 'invalid arguments', got: {}",
            text.text
        );
        // the received field names should appear so the model can see
        // what it sent
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
        // long strings are clipped to keep the preview readable at roughly
        // 40 chars per field. this matters when a model sends a
        // multi-kilobyte blob as the wrong field and the error would
        // otherwise drown the terminal
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
        // the full 500-char string must not appear verbatim; an ellipsis
        // should indicate truncation
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
    }

    #[test]
    fn default_output_limit_is_middle() {
        let tool = EchoTool;
        assert_eq!(tool.output_limit(), OutputLimit::Middle);
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
