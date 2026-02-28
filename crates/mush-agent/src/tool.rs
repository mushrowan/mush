//! agent tool trait and result types

use mush_ai::types::{ImageContent, TextContent, ToolResultContentPart};

/// result of executing an agent tool
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: Vec<ToolResultContentPart>,
    pub is_error: bool,
}

impl ToolResult {
    /// convenience constructor for a text result
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContentPart::Text(TextContent {
                text: text.into(),
            })],
            is_error: false,
        }
    }

    /// convenience constructor for an error result
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContentPart::Text(TextContent {
                text: text.into(),
            })],
            is_error: true,
        }
    }

    /// convenience constructor for an image result
    pub fn image(data: String, mime_type: String) -> Self {
        Self {
            content: vec![ToolResultContentPart::Image(ImageContent {
                data,
                mime_type,
            })],
            is_error: false,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_text() {
        let result = ToolResult::text("hello");
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            ToolResultContentPart::Text(t) => assert_eq!(t.text, "hello"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn tool_result_error() {
        let result = ToolResult::error("something went wrong");
        assert!(result.is_error);
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
        assert!(!result.is_error);
        match &result.content[0] {
            ToolResultContentPart::Text(t) => assert_eq!(t.text, "hello"),
            _ => panic!("expected text"),
        }
    }
}
