//! batch tool - execute multiple tool calls in parallel
//!
//! each sub-call gets the same truncation treatment as a standalone call.
//! the batch result is marked self-truncating so the agent loop doesn't
//! double-truncate the combined output.
//!
//! calls that target the same file path (edit, write) are executed
//! sequentially in the order they appear to avoid silent conflicts.
//! all other calls run in parallel.

use std::collections::HashMap;

use mush_agent::tool::{AgentTool, OutputLimit, ToolRegistry, ToolResult, parse_tool_args};
use mush_agent::truncation;
use mush_ai::types::ToolResultContentPart;
use serde::Deserialize;

const MAX_CALLS: usize = 25;
const MAX_TOTAL_OUTPUT: usize = 100 * 1024; // 100KB combined output cap

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

// tools whose file operations must be serialised per-path
const FILE_MUTATING: &[&str] = &["edit", "write"];

/// extract the file path from parameters for tools that mutate files
fn file_path_key<'a>(tool_name: &str, params: &'a serde_json::Value) -> Option<&'a str> {
    if FILE_MUTATING
        .iter()
        .any(|t| tool_name.eq_ignore_ascii_case(t))
    {
        params.get("path").and_then(|v| v.as_str())
    } else {
        None
    }
}

type CallResult = (
    usize,
    String,
    Option<OutputLimit>,
    Result<ToolResult, String>,
);

async fn run_call(i: usize, call: BatchCall, tools: &ToolRegistry) -> CallResult {
    if call.tool.eq_ignore_ascii_case("batch") {
        return (
            i,
            call.tool,
            None,
            Err("cannot nest batch inside batch".to_string()),
        );
    }

    let tool = match tools.get(&call.tool) {
        Some(tool) => tool,
        None => {
            return (
                i,
                call.tool.clone(),
                None,
                Err(format!("unknown tool: {}", call.tool)),
            );
        }
    };

    let limit = tool.output_limit();
    let result = tool.execute(call.parameters).await;
    (i, call.tool, Some(limit), Ok(result))
}

/// partition calls into groups: same-path file-mutating calls share a group
/// (executed sequentially), everything else gets its own singleton group
/// (executed in parallel with all other groups)
fn group_by_path(calls: Vec<BatchCall>) -> Vec<Vec<(usize, BatchCall)>> {
    let mut groups: Vec<Vec<(usize, BatchCall)>> = Vec::new();
    let mut path_to_group: HashMap<String, usize> = HashMap::new();

    for (i, call) in calls.into_iter().enumerate() {
        if let Some(path) = file_path_key(&call.tool, &call.parameters) {
            if let Some(&group_idx) = path_to_group.get(path) {
                groups[group_idx].push((i, call));
            } else {
                let idx = groups.len();
                path_to_group.insert(path.to_owned(), idx);
                groups.push(vec![(i, call)]);
            }
        } else {
            groups.push(vec![(i, call)]);
        }
    }

    groups
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

    fn output_limit(&self) -> OutputLimit {
        OutputLimit::SelfManaged
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

            // group same-path file-mutating calls so they run sequentially,
            // everything else runs in parallel across groups
            let groups = group_by_path(params.tool_calls);
            let group_futures = groups.into_iter().map(|group| async {
                let mut results = Vec::with_capacity(group.len());
                for (i, call) in group {
                    results.push(run_call(i, call, &self.tools).await);
                }
                results
            });

            let all_groups = futures::future::join_all(group_futures).await;
            let mut results: Vec<CallResult> = all_groups.into_iter().flatten().collect();
            results.sort_by_key(|(i, _, _, _)| *i);

            let mut success_count = 0;
            let mut error_count = 0;
            let mut output = String::new();
            let mut budget_exhausted = false;

            for (i, tool_name, limit, result) in &results {
                let truncated = match result {
                    Ok(result) => {
                        if result.outcome.is_error() {
                            error_count += 1;
                        } else {
                            success_count += 1;
                        }
                        truncation::apply(result.clone(), limit.unwrap_or_default())
                    }
                    Err(error) => {
                        error_count += 1;
                        ToolResult::error(error.clone())
                    }
                };

                let is_error = truncated.outcome.is_error();
                let status = if is_error { "error" } else { "ok" };
                let header = format!("--- [{i}] {tool_name} [{status}] ---\n");
                let mut item_text = String::new();
                for part in &truncated.content {
                    match part {
                        ToolResultContentPart::Text(text) => item_text.push_str(&text.text),
                        ToolResultContentPart::Image(_) => item_text.push_str("[image]"),
                    }
                }

                // check if adding this item would exceed the total output budget
                let item_size = header.len() + item_text.len() + 2; // +2 for trailing newlines
                if !budget_exhausted && output.len() + item_size > MAX_TOTAL_OUTPUT {
                    budget_exhausted = true;
                    let remaining = results.len() - *i;
                    output.push_str(&format!(
                        "[...{remaining} more items omitted, output budget exceeded ({MAX_TOTAL_OUTPUT} bytes). \
                         use the individual tools directly to see full output.]\n\n"
                    ));
                }

                if budget_exhausted {
                    output.push_str(&format!("--- [{i}] {tool_name} [{status}] --- [omitted]\n"));
                } else {
                    output.push_str(&header);
                    output.push_str(&item_text);
                    output.push_str("\n\n");
                }
            }

            output.push_str(&format!(
                "batch: {success_count}/{total} succeeded, {error_count} failed"
            ));
            if budget_exhausted {
                output.push_str(" (output truncated, budget exceeded)");
            }
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
    async fn total_output_budget_enforced() {
        struct BigTool;

        impl AgentTool for BigTool {
            fn name(&self) -> &str {
                "big"
            }

            fn label(&self) -> &str {
                self.name()
            }

            fn description(&self) -> &str {
                "big output"
            }

            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }

            fn execute(
                &self,
                _params: serde_json::Value,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>>
            {
                // each call returns ~20KB
                let text = "x".repeat(20_000);
                Box::pin(async move { ToolResult::text(text) })
            }
        }

        // 10 calls × 20KB = 200KB, should exceed the 100KB budget
        let calls: Vec<_> = (0..10)
            .map(|_| serde_json::json!({"tool": "big", "parameters": {}}))
            .collect();
        let tool = BatchTool::new(ToolRegistry::from_boxed(vec![Box::new(BigTool)]));
        let result = tool
            .execute(serde_json::json!({ "tool_calls": calls }))
            .await;
        let text = match &result.content[0] {
            ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(text.contains("budget exceeded"));
        assert!(text.contains("[omitted]"));
        assert!(text.contains("batch: 10/10 succeeded, 0 failed"));
        // combined output should be under 150KB (budget + overhead)
        assert!(text.len() < 150_000);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_file_edits_applied_in_order() {
        // custom tool that reads a file, sleeps to widen the race window,
        // then writes the replacement. without sequencing, the second call
        // reads the original content before the first call writes, so one
        // edit is silently lost
        struct SlowEdit {
            dir: std::path::PathBuf,
        }

        impl AgentTool for SlowEdit {
            fn name(&self) -> &str {
                "edit"
            }
            fn label(&self) -> &str {
                "edit"
            }
            fn description(&self) -> &str {
                "slow edit"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            fn execute(
                &self,
                params: serde_json::Value,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>>
            {
                let dir = self.dir.clone();
                Box::pin(async move {
                    let path = dir.join(params["path"].as_str().unwrap());
                    let old = params["oldText"].as_str().unwrap();
                    let new = params["newText"].as_str().unwrap();

                    let content = std::fs::read_to_string(&path).unwrap();
                    // widen the race window so concurrent calls definitely overlap
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let replaced = content.replacen(old, new, 1);
                    std::fs::write(&path, &replaced).unwrap();
                    ToolResult::text(format!("edited {}", path.display()))
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "aaa\nbbb\nccc\n").unwrap();

        let tool = BatchTool::new(ToolRegistry::from_boxed(vec![Box::new(SlowEdit {
            dir: dir.path().to_path_buf(),
        })]));

        let result = tool
            .execute(serde_json::json!({
                "tool_calls": [
                    {"tool": "edit", "parameters": {"path": "test.txt", "oldText": "aaa", "newText": "AAA"}},
                    {"tool": "edit", "parameters": {"path": "test.txt", "oldText": "bbb", "newText": "BBB"}},
                    {"tool": "edit", "parameters": {"path": "test.txt", "oldText": "ccc", "newText": "CCC"}}
                ]
            }))
            .await;

        let text = match &result.content[0] {
            ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(
            text.contains("3/3 succeeded"),
            "all edits should succeed: {text}"
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content, "AAA\nBBB\nCCC\n",
            "all three edits should be reflected in the file"
        );
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
