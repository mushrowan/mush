//! batch tool - execute multiple tool calls in parallel

use mush_agent::tool::{AgentTool, ToolResult};
use mush_ai::types::ToolResultContentPart;

const MAX_CALLS: usize = 25;
/// max total output bytes across all batch results to prevent context flooding
const MAX_TOTAL_OUTPUT_BYTES: usize = 100 * 1024;
/// reserve room for the summary so truncation doesn't hide it
const SUMMARY_RESERVE_BYTES: usize = 512;

pub struct BatchTool {
    tools: Vec<Box<dyn AgentTool>>,
}

impl BatchTool {
    #[must_use]
    pub fn new(tools: Vec<Box<dyn AgentTool>>) -> Self {
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
        "Execute multiple tool calls concurrently to reduce latency. All calls run in parallel; ordering is NOT guaranteed. Partial failures do not stop other calls. Do NOT nest batch inside batch.\n\nGood for: reading many files, grep+glob combos, multiple bash commands, multi-part edits.\nBad for: operations that depend on prior output, ordered stateful mutations.\n\nPayload format (JSON array):\n[{\"tool\": \"read\", \"parameters\": {\"path\": \"src/main.rs\"}}, {\"tool\": \"grep\", \"parameters\": {\"pattern\": \"TODO\"}}]"
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
            let calls = match params.get("tool_calls").and_then(serde_json::Value::as_array) {
                Some(a) => a,
                None => return ToolResult::error("missing required parameter: tool_calls"),
            };

            if calls.is_empty() {
                return ToolResult::error("tool_calls cannot be empty");
            }

            if calls.len() > MAX_CALLS {
                return ToolResult::error(format!(
                    "too many tool calls: {} (max {})",
                    calls.len(),
                    MAX_CALLS
                ));
            }

            let calls: Vec<_> = calls.iter().take(MAX_CALLS).collect();
            let total = calls.len();

            let futures: Vec<_> = calls
                .into_iter()
                .enumerate()
                .map(|(i, call)| async move {
                    let tool_name = call
                        .get("tool")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");

                    let params = call
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));

                    if tool_name.eq_ignore_ascii_case("batch") {
                        return (
                            i,
                            tool_name.to_string(),
                            Err("cannot nest batch inside batch".to_string()),
                        );
                    }

                    let tool = match self.tools.iter().find(|t| t.name().eq_ignore_ascii_case(tool_name)) {
                        Some(t) => t,
                        None => {
                            return (
                                i,
                                tool_name.to_string(),
                                Err(format!("unknown tool: {tool_name}")),
                            );
                        }
                    };

                    let result = tool.execute(params).await;
                    (i, tool_name.to_string(), Ok(result))
                })
                .collect();

            let results = futures::future::join_all(futures).await;

            // format output, tracking total size to avoid context flooding
            let mut output = String::new();
            let mut success_count = 0;
            let mut error_count = 0;
            let mut total_bytes = 0;
            let mut items_truncated = 0;
            let content_budget = MAX_TOTAL_OUTPUT_BYTES.saturating_sub(SUMMARY_RESERVE_BYTES);

            for (i, tool_name, result) in &results {
                let mut item = format!("--- [{i}] {tool_name} ---\n");
                match result {
                    Ok(r) => {
                        if r.outcome.is_error() {
                            error_count += 1;
                            item.push_str("ERROR: ");
                        } else {
                            success_count += 1;
                        }
                        for part in &r.content {
                            match part {
                                ToolResultContentPart::Text(t) => item.push_str(&t.text),
                                ToolResultContentPart::Image(_) => item.push_str("[image]"),
                            }
                        }
                    }
                    Err(e) => {
                        error_count += 1;
                        item.push_str(&format!("ERROR: {e}"));
                    }
                }
                item.push_str("\n\n");

                if total_bytes >= content_budget {
                    items_truncated += 1;
                    continue;
                }

                let remaining = content_budget - total_bytes;
                if item.len() > remaining {
                    items_truncated += 1;
                    output.push_str(&item[..remaining]);
                    break;
                }

                total_bytes += item.len();
                output.push_str(&item);
            }

            let mut summary = format!("batch: {success_count}/{total} succeeded, {error_count} failed");
            if items_truncated > 0 {
                summary.push_str(&format!(", {items_truncated} items truncated due to combined size"));
            }
            output.push_str(&summary);

            ToolResult::text(output)
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
        assert_eq!(schema["required"], serde_json::json!(["tool_calls"]));
    }

    #[tokio::test]
    async fn empty_calls_returns_error() {
        let tool = BatchTool::new(vec![]);
        let result = tool.execute(serde_json::json!({ "tool_calls": [] })).await;
        assert!(result.outcome.is_error());
    }

    #[tokio::test]
    async fn too_many_calls_returns_error() {
        let tool = BatchTool::new(vec![]);
        let calls: Vec<_> = (0..26)
            .map(|_| serde_json::json!({ "tool": "read", "parameters": {} }))
            .collect();
        let result = tool.execute(serde_json::json!({ "tool_calls": calls })).await;
        assert!(result.outcome.is_error());
    }

    #[tokio::test]
    async fn batch_self_nesting_blocked() {
        let tool = BatchTool::new(vec![]);
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
        let tool = BatchTool::new(vec![]);
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
    async fn summary_survives_large_output() {
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
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
                Box::pin(async { ToolResult::text("x".repeat(MAX_TOTAL_OUTPUT_BYTES)) })
            }
        }

        let tool = BatchTool::new(vec![Box::new(LargeTool)]);
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
        assert!(text.contains("1 items truncated due to combined size"));
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
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
                Box::pin(async { ToolResult::text("ok") })
            }
        }

        let tool = BatchTool::new(vec![Box::new(DummyTool)]);
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
