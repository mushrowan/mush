//! batch tool - execute multiple tool calls in parallel
//!
//! each sub-call gets the same truncation treatment as a standalone call.
//! the batch result is marked self-truncating so the agent loop doesn't
//! double-truncate the combined output.

use mush_agent::tool::{AgentTool, ToolRegistry, ToolResult, parse_tool_args};
use mush_agent::truncation;
use mush_ai::types::ToolResultContentPart;
use serde::Deserialize;

const MAX_CALLS: usize = 25;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchArgs {
    tool_calls: Vec<BatchCall>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchCall {
    tool: String,
    parameters: serde_json::Value,
}

pub struct BatchTool {
    tools: ToolRegistry,
}

impl BatchTool {
    #[must_use]
    pub fn new(tools: ToolRegistry) -> Self {
        Self { tools }
    }
}

impl AgentTool for BatchTool {
    fn name(&self) -> &str {
        "batch"
    }

    fn label(&self) -> &str {
        self.name()
    }

    fn description(&self) -> &str {
        "Execute multiple tool calls concurrently to reduce latency. Each call gets the same output limits as a standalone call. Do NOT nest batch inside batch.\n\nGood for: reading multiple files, grep+glob combos, multiple bash commands, multi-part edits.\nBad for: operations that depend on prior output, ordered stateful mutations."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "tool_calls": {
                    "type": "array",
                    "description": "array of tool calls to execute in parallel (max 25)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "tool": {
                                "type": "string",
                                "description": "name of the tool to execute"
                            },
                            "parameters": {
                                "type": "object",
                                "description": "parameters to pass to the tool"
                            }
                        },
                        "required": ["tool", "parameters"]
                    }
                }
            },
            "required": ["tool_calls"]
        })
    }

    fn execute(
        &self,
        params: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let params = match parse_tool_args::<BatchArgs>(params) {
                Ok(params) => params,
                Err(error) => return error,
            };

            if params.tool_calls.is_empty() {
                return ToolResult::error("tool_calls cannot be empty");
            }

            if params.tool_calls.len() > MAX_CALLS {
                return ToolResult::error(format!(
                    "too many tool calls: {} (max {})",
                    params.tool_calls.len(),
                    MAX_CALLS
                ));
            }

            let total = params.tool_calls.len();
            let futures: Vec<_> = params
                .tool_calls
                .into_iter()
                .enumerate()
                .map(|(i, call)| async move {
                    if call.tool.eq_ignore_ascii_case("batch") {
                        return (
                            i,
                            call.tool,
                            Err("cannot nest batch inside batch".to_string()),
                        );
                    }

                    let tool = match self.tools.get(&call.tool) {
                        Some(tool) => tool,
                        None => {
                            return (
                                i,
                                call.tool.clone(),
                                Err(format!("unknown tool: {}", call.tool)),
                            );
                        }
                    };

                    let result = tool.execute(call.parameters).await;
                    (i, call.tool, Ok(result))
                })
                .collect();

            let results = futures::future::join_all(futures).await;

            let mut success_count = 0;
            let mut error_count = 0;
            let mut output = String::new();

            for (i, tool_name, result) in &results {
                let truncated = match result {
                    Ok(result) => {
                        if result.outcome.is_error() {
                            error_count += 1;
                        } else {
                            success_count += 1;
                        }
                        if truncation::self_truncating(tool_name) {
                            result.clone()
                        } else {
                            truncation::truncate_tool_output(result.clone())
                        }
                    }
                    Err(error) => {
                        error_count += 1;
                        ToolResult::error(error.clone())
                    }
                };

                output.push_str(&format!("--- [{i}] {tool_name} ---\n"));
                for part in &truncated.content {
                    match part {
                        ToolResultContentPart::Text(text) => output.push_str(&text.text),
                        ToolResultContentPart::Image(_) => output.push_str("[image]"),
                    }
                }
                output.push_str("\n\n");
            }

            output.push_str(&format!(
                "batch: {success_count}/{total} succeeded, {error_count} failed"
            ));
            ToolResult::text(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_required_tool_calls() {
        let tool = BatchTool::new(ToolRegistry::new());
        let schema = tool.parameters_schema();
        assert_eq!(schema["required"], serde_json::json!(["tool_calls"]));
    }

    #[tokio::test]
    async fn empty_calls_returns_error() {
        let tool = BatchTool::new(ToolRegistry::new());
        let result = tool.execute(serde_json::json!({ "tool_calls": [] })).await;
        assert!(result.outcome.is_error());
    }

    #[tokio::test]
    async fn too_many_calls_returns_error() {
        let tool = BatchTool::new(ToolRegistry::new());
        let calls: Vec<_> = (0..26)
            .map(|_| serde_json::json!({ "tool": "read", "parameters": {} }))
            .collect();
        let result = tool
            .execute(serde_json::json!({ "tool_calls": calls }))
            .await;
        assert!(result.outcome.is_error());
    }

    #[tokio::test]
    async fn batch_self_nesting_blocked() {
        let tool = BatchTool::new(ToolRegistry::new());
        let result = tool
            .execute(serde_json::json!({
                "tool_calls": [{"tool": "batch", "parameters": {"tool_calls": []}}]
            }))
            .await;
        let text = match &result.content[0] {
            ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(text.contains("cannot nest batch inside batch"));
    }

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let tool = BatchTool::new(ToolRegistry::new());
        let result = tool
            .execute(serde_json::json!({
                "tool_calls": [{"tool": "does-not-exist", "parameters": {}}]
            }))
            .await;
        let text = match &result.content[0] {
            ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(text.contains("unknown tool"));
    }

    #[tokio::test]
    async fn large_output_truncated_like_standalone() {
        struct LargeTool;

        impl AgentTool for LargeTool {
            fn name(&self) -> &str {
                "large"
            }

            fn label(&self) -> &str {
                self.name()
            }

            fn description(&self) -> &str {
                "large"
            }

            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }

            fn execute(
                &self,
                _params: serde_json::Value,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>>
            {
                let lines: Vec<String> = (0..5000).map(|i| format!("line {i}")).collect();
                Box::pin(async move { ToolResult::text(lines.join("\n")) })
            }
        }

        let tool = BatchTool::new(ToolRegistry::from_boxed(vec![Box::new(LargeTool)]));
        let result = tool
            .execute(serde_json::json!({
                "tool_calls": [{"tool": "large", "parameters": {}}]
            }))
            .await;
        let text = match &result.content[0] {
            ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(text.contains("batch: 1/1 succeeded, 0 failed"));
        assert!(text.contains("lines truncated"));
        assert!(text.contains("Use grep to search"));
        assert!(text.contains("line 0"));
        assert!(text.contains("line 4999"));
    }

    #[tokio::test]
    async fn tool_lookup_is_case_insensitive() {
        struct DummyTool;

        impl AgentTool for DummyTool {
            fn name(&self) -> &str {
                "DuMmY"
            }

            fn label(&self) -> &str {
                self.name()
            }

            fn description(&self) -> &str {
                "dummy"
            }

            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }

            fn execute(
                &self,
                _params: serde_json::Value,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>>
            {
                Box::pin(async { ToolResult::text("ok") })
            }
        }

        let tool = BatchTool::new(ToolRegistry::from_boxed(vec![Box::new(DummyTool)]));
        let result = tool
            .execute(serde_json::json!({
                "tool_calls": [{"tool": "dummy", "parameters": {}}]
            }))
            .await;
        let text = match &result.content[0] {
            ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(text.contains("ok"));
    }
}
