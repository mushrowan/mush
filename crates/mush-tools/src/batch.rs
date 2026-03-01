//! batch tool - execute multiple tool calls in parallel

use mush_agent::tool::{AgentTool, ToolResult};
use mush_ai::types::ToolResultContentPart;

const MAX_CALLS: usize = 25;

/// batch tool that runs multiple tool calls concurrently.
/// holds references to the available tools so it can dispatch.
pub struct BatchTool {
    tools: Vec<Box<dyn AgentTool>>,
}

impl BatchTool {
    pub fn new(tools: Vec<Box<dyn AgentTool>>) -> Self {
        Self { tools }
    }

    fn find_tool(&self, name: &str) -> Option<&dyn AgentTool> {
        self.tools.iter().find(|t| t.name() == name).map(|t| &**t)
    }
}

impl AgentTool for BatchTool {
    fn name(&self) -> &str {
        "batch"
    }
    fn label(&self) -> &str {
        "Batch"
    }
    fn description(&self) -> &str {
        "Execute multiple tool calls concurrently to reduce latency. All calls run in parallel; \
         ordering is NOT guaranteed. Partial failures do not stop other calls. \
         Do NOT nest batch inside batch.\n\n\
         Good for: reading many files, grep+glob combos, multiple bash commands, multi-part edits.\n\
         Bad for: operations that depend on prior output, ordered stateful mutations.\n\n\
         Payload format (JSON array):\n\
         [{\"tool\": \"read\", \"parameters\": {\"path\": \"src/main.rs\"}}, \
         {\"tool\": \"grep\", \"parameters\": {\"pattern\": \"TODO\"}}]"
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
                                "description": "parameters for the tool"
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
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let calls = match args["tool_calls"].as_array() {
                Some(a) => a,
                None => return ToolResult::error("missing required parameter: tool_calls"),
            };

            if calls.is_empty() {
                return ToolResult::error("tool_calls must not be empty");
            }

            let calls: Vec<_> = calls.iter().take(MAX_CALLS).collect();
            let total = calls.len();

            // build futures for all calls
            let futures: Vec<_> = calls
                .iter()
                .enumerate()
                .map(|(i, call)| async move {
                    let tool_name = call["tool"].as_str().unwrap_or("unknown");

                    if tool_name == "batch" {
                        return (
                            i,
                            tool_name.to_string(),
                            Err("cannot nest batch inside batch".to_string()),
                        );
                    }

                    let tool = match self.find_tool(tool_name) {
                        Some(t) => t,
                        None => {
                            return (
                                i,
                                tool_name.to_string(),
                                Err(format!("unknown tool: {tool_name}")),
                            );
                        }
                    };

                    let params = call["parameters"].clone();
                    let result = tool.execute(params).await;
                    (i, tool_name.to_string(), Ok(result))
                })
                .collect();

            let results = futures::future::join_all(futures).await;

            // format output
            let mut output = String::new();
            let mut success_count = 0;
            let mut error_count = 0;

            for (i, tool_name, result) in &results {
                output.push_str(&format!("--- [{i}] {tool_name} ---\n"));
                match result {
                    Ok(r) => {
                        if r.is_error {
                            error_count += 1;
                            output.push_str("ERROR: ");
                        } else {
                            success_count += 1;
                        }
                        for part in &r.content {
                            match part {
                                ToolResultContentPart::Text(t) => output.push_str(&t.text),
                                ToolResultContentPart::Image(_) => output.push_str("[image]"),
                            }
                        }
                    }
                    Err(e) => {
                        error_count += 1;
                        output.push_str(&format!("ERROR: {e}"));
                    }
                }
                output.push_str("\n\n");
            }

            output.push_str(&format!(
                "batch: {success_count}/{total} succeeded, {error_count} failed"
            ));

            if error_count > 0 {
                // still return as non-error so the model can see partial results
                ToolResult::text(output)
            } else {
                ToolResult::text(output)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_required_tool_calls() {
        let tool = BatchTool::new(vec![]);
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "tool_calls"));
    }

    #[tokio::test]
    async fn empty_calls_returns_error() {
        let tool = BatchTool::new(vec![]);
        let result = tool.execute(serde_json::json!({"tool_calls": []})).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn unknown_tool_reports_error() {
        let tool = BatchTool::new(vec![]);
        let result = tool
            .execute(serde_json::json!({
                "tool_calls": [
                    {"tool": "nonexistent", "parameters": {}}
                ]
            }))
            .await;
        assert!(!result.is_error); // partial results are non-error
        let text = match &result.content[0] {
            ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(text.contains("unknown tool"));
    }

    #[tokio::test]
    async fn batch_self_nesting_blocked() {
        let tool = BatchTool::new(vec![]);
        let result = tool
            .execute(serde_json::json!({
                "tool_calls": [
                    {"tool": "batch", "parameters": {"tool_calls": []}}
                ]
            }))
            .await;
        let text = match &result.content[0] {
            ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(text.contains("cannot nest batch"));
    }
}
