//! batch tool - execute multiple tool calls in parallel
//!
//! each sub-call gets the same truncation treatment as a standalone call.
//! the combined output targets the central truncation budget (MAX_BYTES)
//! so the agent loop's final truncation pass is a no-op.
//!
//! calls that target the same file path (edit, write) are executed
//! sequentially in the order they appear to avoid silent conflicts.
//! all other calls run in parallel.

use mush_agent::tool::{AgentTool, OutputLimit, ToolRegistry, ToolResult, parse_tool_args};
use mush_agent::tool_grouping;
use mush_agent::truncation;
use mush_ai::types::ToolResultContentPart;
use serde::Deserialize;

const MAX_CALLS: usize = 25;
const MAX_TOTAL_OUTPUT: usize = mush_agent::truncation::MAX_BYTES;

/// truncate text to a byte budget on a line boundary
fn truncate_to_budget(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    // find last newline before budget
    match text[..budget].rfind('\n') {
        Some(pos) => &text[..pos],
        None => &text[..budget],
    }
}

/// extract a "Full output: /path" reference from truncated tool output
fn extract_file_path(text: &str) -> Option<&str> {
    for line in text.lines() {
        if let Some(pos) = line.find("Full output: ") {
            return Some(&line[pos + "Full output: ".len()..]);
        }
    }
    None
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchArgs {
    tool_calls: Vec<BatchCall>,
    /// when true, run tool_calls strictly in submission order and stop
    /// at the first failure. defaults to false (parallel / path-grouped)
    #[serde(default)]
    sequential: bool,
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

#[async_trait::async_trait]
impl AgentTool for BatchTool {
    fn name(&self) -> &str {
        "batch"
    }

    fn label(&self) -> &str {
        self.name()
    }

    fn description(&self) -> &str {
        "Execute multiple tool calls concurrently to reduce latency. Each call gets the same output limits as a standalone call. Do NOT nest batch inside batch.\n\nGood for: reading multiple files, grep+glob combos, multiple bash commands, multi-part edits.\nBad for: operations that depend on prior output, ordered stateful mutations.\n\nSet sequential=true to run calls strictly in submission order, stopping at the first failure. Use for dependent operations (e.g. mkdir then cd then run)."
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
                },
                "sequential": {
                    "type": "boolean",
                    "description": "when true, run tool_calls in order and stop at the first failure. default false"
                }
            },
            "required": ["tool_calls"]
        })
    }

    fn output_limit(&self) -> OutputLimit {
        OutputLimit::Middle
    }

    async fn execute(&self, params: serde_json::Value) -> ToolResult {
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

        let mut results: Vec<CallResult> = if params.sequential {
            // sequential mode: one call at a time, abort remaining on first
            // failure. the unrun tail is reported as "skipped" so the agent
            // sees what didn't execute
            let mut out = Vec::with_capacity(total);
            let mut aborted = false;
            for (i, call) in params.tool_calls.into_iter().enumerate() {
                if aborted {
                    out.push((
                        i,
                        call.tool,
                        None,
                        Err("skipped after earlier failure (sequential=true)".to_string()),
                    ));
                    continue;
                }
                let result = run_call(i, call, &self.tools).await;
                let is_failure = match &result.3 {
                    Err(_) => true,
                    Ok(tr) => tr.outcome.is_error(),
                };
                out.push(result);
                if is_failure {
                    aborted = true;
                }
            }
            out
        } else {
            // parallel mode: same-path file-mutating calls share a group
            // (executed sequentially), everything else gets its own group
            // (executed in parallel with all other groups). uses the
            // shared mush_agent::tool_grouping helper so the agent loop
            // and the batch tool apply identical serialisation rules
            let calls: Vec<(usize, BatchCall)> =
                params.tool_calls.into_iter().enumerate().collect();
            let tools = &self.tools;
            tool_grouping::execute_grouped(
                calls,
                |(_, call)| tool_grouping::file_path_key(&call.tool, &call.parameters),
                move |(i, call)| async move { run_call(i, call, tools).await },
            )
            .await
        };
        results.sort_by_key(|(i, _, _, _)| *i);

        let mut success_count = 0;
        let mut error_count = 0;

        // first pass: individually truncate each result and extract text
        let mut items: Vec<(usize, String, String, String)> = Vec::with_capacity(results.len());
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

            let status = if truncated.outcome.is_error() {
                "error"
            } else {
                "ok"
            };
            let header = format!("--- [{i}] {tool_name} [{status}] ---\n");
            let mut item_text = String::new();
            for part in &truncated.content {
                match part {
                    ToolResultContentPart::Text(text) => item_text.push_str(&text.text),
                    ToolResultContentPart::Image(_) => item_text.push_str("[image]"),
                }
            }
            items.push((*i, tool_name.clone(), header, item_text));
        }

        // check if total exceeds budget
        let total_size: usize = items.iter().map(|(_, _, h, t)| h.len() + t.len() + 2).sum();

        let mut output = String::new();
        if total_size <= MAX_TOTAL_OUTPUT {
            // everything fits, no truncation needed
            for (_, _, header, item_text) in &items {
                output.push_str(header);
                output.push_str(item_text);
                output.push_str("\n\n");
            }
        } else {
            // build full output and save to file
            let mut full_output = String::with_capacity(total_size + 256);
            for (_, _, header, item_text) in &items {
                full_output.push_str(header);
                full_output.push_str(item_text);
                full_output.push_str("\n\n");
            }
            let saved_path = truncation::save_batch_output(&full_output);

            // fair truncation: give each item a share of the budget
            let overhead_per_item = 80; // header + truncation notice
            let usable = MAX_TOTAL_OUTPUT.saturating_sub(items.len() * overhead_per_item + 256);
            let per_item_budget = usable / items.len();

            for (_, _, header, item_text) in &items {
                output.push_str(header);
                if item_text.len() <= per_item_budget {
                    output.push_str(item_text);
                } else {
                    // preserve any file path from individual truncation
                    let file_ref = extract_file_path(item_text);
                    let truncated = truncate_to_budget(item_text, per_item_budget);
                    output.push_str(truncated);
                    let omitted = item_text.len() - truncated.len();
                    output.push_str(&format!("\n[…truncated, {omitted} bytes omitted"));
                    if let Some(path) = file_ref {
                        output.push_str(&format!(". full output: {path}"));
                    }
                    output.push(']');
                }
                output.push_str("\n\n");
            }

            let path_note = match saved_path {
                Some(p) => format!("full output: {}", p.display()),
                None => "full output could not be saved".into(),
            };
            output.push_str(&format!(
                "[batch output exceeded {MAX_TOTAL_OUTPUT} byte budget. {path_note}]\n"
            ));
        }

        output.push_str(&format!(
            "batch: {success_count}/{total} succeeded, {error_count} failed"
        ));
        ToolResult::text(output)
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

        #[async_trait::async_trait]
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

            async fn execute(&self, _params: serde_json::Value) -> ToolResult {
                let lines: Vec<String> = (0..5000).map(|i| format!("line {i}")).collect();
                ToolResult::text(lines.join("\n"))
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
        assert!(text.contains("Use the Grep tool to search"));
        assert!(text.contains("line 0"));
        assert!(text.contains("line 4999"));
    }

    #[tokio::test]
    async fn total_output_budget_truncates_fairly_with_spillover() {
        struct BigTool;

        #[async_trait::async_trait]
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

            async fn execute(&self, params: serde_json::Value) -> ToolResult {
                // each call returns ~20KB with a unique marker
                let idx = params.get("idx").and_then(|v| v.as_u64()).unwrap_or(0);
                let text = format!("MARKER_{idx}\n{}", "x".repeat(20_000));
                ToolResult::text(text)
            }
        }

        // 10 calls x ~20KB = 200KB, should exceed the MAX_TOTAL_OUTPUT budget
        let calls: Vec<_> = (0..10)
            .map(|i| serde_json::json!({"tool": "big", "parameters": {"idx": i}}))
            .collect();
        let tool = BatchTool::new(ToolRegistry::from_boxed(vec![Box::new(BigTool)]));
        let result = tool
            .execute(serde_json::json!({ "tool_calls": calls }))
            .await;
        let text = match &result.content[0] {
            ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };

        // every tool should have a header (no tool completely omitted)
        for i in 0..10 {
            assert!(
                text.contains(&format!("[{i}] big")),
                "missing header for tool {i}: {text}"
            );
        }

        // should mention the spillover file path
        assert!(
            text.contains("full output:"),
            "missing spillover file path: {text}"
        );

        // each tool's output should be truncated, not just later ones
        // count how many tools have their marker visible
        let markers_visible: Vec<bool> = (0..10)
            .map(|i| text.contains(&format!("MARKER_{i}")))
            .collect();
        // all markers should be present (fair truncation keeps a preview of each)
        assert!(
            markers_visible.iter().all(|v| *v),
            "not all markers visible (fair truncation failed): {markers_visible:?}"
        );

        assert!(text.contains("batch: 10/10 succeeded, 0 failed"));
        // inline output should be bounded
        assert!(
            text.len() < 150_000,
            "inline output too large: {} bytes",
            text.len()
        );
    }

    #[tokio::test]
    async fn batch_preserves_individual_file_paths() {
        struct PathTool;

        #[async_trait::async_trait]
        impl AgentTool for PathTool {
            fn name(&self) -> &str {
                "pathtool"
            }
            fn label(&self) -> &str {
                self.name()
            }
            fn description(&self) -> &str {
                "returns output with a file path reference"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, params: serde_json::Value) -> ToolResult {
                let idx = params.get("idx").and_then(|v| v.as_u64()).unwrap_or(0);
                // simulate individually-truncated output with a file path hint in the middle
                let text = format!(
                    "{}\n\n[…500 lines truncated (1000 total). Full output: /tmp/tool_{idx}.txt]\n\n{}",
                    "x".repeat(15_000),
                    "y".repeat(15_000),
                );
                ToolResult::text(text)
            }
        }

        let calls: Vec<_> = (0..8)
            .map(|i| serde_json::json!({"tool": "pathtool", "parameters": {"idx": i}}))
            .collect();
        let tool = BatchTool::new(ToolRegistry::from_boxed(vec![Box::new(PathTool)]));
        let result = tool
            .execute(serde_json::json!({ "tool_calls": calls }))
            .await;
        let text = match &result.content[0] {
            ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };

        // each tool's individual file path should survive batch truncation
        for i in 0..8 {
            assert!(
                text.contains(&format!("/tmp/tool_{i}.txt")),
                "missing file path for tool {i}"
            );
        }
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

        #[async_trait::async_trait]
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
            async fn execute(&self, params: serde_json::Value) -> ToolResult {
                let dir = self.dir.clone();
                let path = dir.join(params["path"].as_str().unwrap());
                let old = params["oldText"].as_str().unwrap();
                let new = params["newText"].as_str().unwrap();

                let content = std::fs::read_to_string(&path).unwrap();
                // widen the race window so concurrent calls definitely overlap
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let replaced = content.replacen(old, new, 1);
                std::fs::write(&path, &replaced).unwrap();
                ToolResult::text(format!("edited {}", path.display()))
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sequential_mode_runs_in_order_and_stops_on_first_failure() {
        // a tool that records the order of calls and errors on the marked
        // call. sequential=true should run calls strictly in submission
        // order and refuse to execute anything after a failure
        use std::sync::Mutex;

        struct OrderTool {
            log: std::sync::Arc<Mutex<Vec<String>>>,
        }

        #[async_trait::async_trait]
        impl AgentTool for OrderTool {
            fn name(&self) -> &str {
                "order"
            }
            fn label(&self) -> &str {
                "order"
            }
            fn description(&self) -> &str {
                "records call order"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, params: serde_json::Value) -> ToolResult {
                let id = params["id"].as_str().unwrap_or("?").to_string();
                self.log.lock().unwrap().push(id.clone());
                if params
                    .get("fail")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    return ToolResult::error(format!("boom at {id}"));
                }
                ToolResult::text(format!("ran {id}"))
            }
        }

        let log = std::sync::Arc::new(Mutex::new(Vec::new()));
        let tool = BatchTool::new(ToolRegistry::from_boxed(vec![Box::new(OrderTool {
            log: log.clone(),
        })]));

        let result = tool
            .execute(serde_json::json!({
                "sequential": true,
                "tool_calls": [
                    {"tool": "order", "parameters": {"id": "a"}},
                    {"tool": "order", "parameters": {"id": "b", "fail": true}},
                    {"tool": "order", "parameters": {"id": "c"}},
                    {"tool": "order", "parameters": {"id": "d"}},
                ]
            }))
            .await;

        // only a and b should have actually run; c and d were skipped
        let log = log.lock().unwrap().clone();
        assert_eq!(
            log,
            vec!["a".to_string(), "b".to_string()],
            "sequential should stop after first failure, got log {log:?}"
        );

        let text = match &result.content[0] {
            ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        // all four entries appear in the output (either ran or skipped)
        for id in ["a", "b", "c", "d"] {
            assert!(
                text.contains(&format!(
                    "[{}]",
                    ["a", "b", "c", "d"].iter().position(|x| *x == id).unwrap()
                )),
                "missing header for {id}: {text}"
            );
        }
        assert!(
            text.contains("skipped after earlier failure"),
            "skipped entries should explain why: {text}"
        );
        // summary counts: 1 success, 3 failures (b failed, c+d skipped-as-error)
        assert!(
            text.contains("1/4 succeeded"),
            "expected 1/4 succeeded in summary: {text}"
        );
    }

    #[tokio::test]
    async fn sequential_mode_preserves_order_without_failures() {
        use std::sync::Mutex;

        struct OrderTool {
            log: std::sync::Arc<Mutex<Vec<String>>>,
        }

        #[async_trait::async_trait]
        impl AgentTool for OrderTool {
            fn name(&self) -> &str {
                "order"
            }
            fn label(&self) -> &str {
                "order"
            }
            fn description(&self) -> &str {
                "records"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, params: serde_json::Value) -> ToolResult {
                let id = params["id"].as_str().unwrap_or("?").to_string();
                // short sleep so parallel-mode would interleave but
                // sequential mode still observes strict order
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                self.log.lock().unwrap().push(id.clone());
                ToolResult::text(format!("ran {id}"))
            }
        }

        let log = std::sync::Arc::new(Mutex::new(Vec::new()));
        let tool = BatchTool::new(ToolRegistry::from_boxed(vec![Box::new(OrderTool {
            log: log.clone(),
        })]));
        let _ = tool
            .execute(serde_json::json!({
                "sequential": true,
                "tool_calls": [
                    {"tool": "order", "parameters": {"id": "a"}},
                    {"tool": "order", "parameters": {"id": "b"}},
                    {"tool": "order", "parameters": {"id": "c"}},
                ]
            }))
            .await;
        let log = log.lock().unwrap().clone();
        assert_eq!(
            log,
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            "sequential should observe submission order: {log:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_file_edits_applied_in_order_with_different_path_syntax() {
        // when the model emits the same file with different syntactic
        // forms (e.g. "test.txt" and "./test.txt") the batch tool should
        // still detect the conflict and serialise the calls. without
        // path normalisation the two strings would key separate groups
        // and race
        struct SlowEdit {
            dir: std::path::PathBuf,
        }

        #[async_trait::async_trait]
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
            async fn execute(&self, params: serde_json::Value) -> ToolResult {
                let raw = params["path"].as_str().unwrap();
                let stripped = raw.strip_prefix("./").unwrap_or(raw);
                let path = self.dir.join(stripped);
                let old = params["oldText"].as_str().unwrap();
                let new = params["newText"].as_str().unwrap();
                let content = std::fs::read_to_string(&path).unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let replaced = content.replacen(old, new, 1);
                std::fs::write(&path, &replaced).unwrap();
                ToolResult::text(format!("edited {}", path.display()))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "aaa\nbbb\n").unwrap();

        let tool = BatchTool::new(ToolRegistry::from_boxed(vec![Box::new(SlowEdit {
            dir: dir.path().to_path_buf(),
        })]));

        let result = tool
            .execute(serde_json::json!({
                "tool_calls": [
                    {"tool": "edit", "parameters": {"path": "test.txt", "oldText": "aaa", "newText": "AAA"}},
                    {"tool": "edit", "parameters": {"path": "./test.txt", "oldText": "bbb", "newText": "BBB"}},
                ]
            }))
            .await;

        let text = match &result.content[0] {
            ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(
            text.contains("2/2 succeeded"),
            "edits should both succeed: {text}"
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content, "AAA\nBBB\n",
            "both edits should be reflected even though the paths differ syntactically"
        );
    }

    #[tokio::test]
    async fn tool_lookup_is_case_insensitive() {
        struct DummyTool;

        #[async_trait::async_trait]
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

            async fn execute(&self, _params: serde_json::Value) -> ToolResult {
                ToolResult::text("ok")
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

    #[test]
    fn batch_budget_matches_central_truncation_limit() {
        // batch's total output budget must equal the central truncation
        // MAX_BYTES so the central pass is a no-op for batch output
        assert_eq!(
            MAX_TOTAL_OUTPUT,
            mush_agent::truncation::MAX_BYTES,
            "batch budget diverged from central truncation limit"
        );
    }
}
