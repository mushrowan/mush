use mush_agent::exec_context::{self, TOOL_EXEC_ID};
use mush_agent::tool::{AgentTool, OutputLimit, ToolRegistry, ToolResult, parse_tool_args};
use mush_agent::tool_grouping;
use mush_agent::truncation;
use mush_ai::types::ToolResultContentPart;
use serde::Deserialize;

const MAX_CALLS: usize = 25;
const MAX_TOTAL_OUTPUT: usize = mush_agent::truncation::MAX_BYTES;

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
    #[serde(deserialize_with = "mush_agent::de::json_seq")]
    tool_calls: Vec<BatchCall>,
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

struct CallOutcome {
    tool: String,
    limit: Option<OutputLimit>,
    result: Result<ToolResult, String>,
}

impl CallOutcome {
    fn rejected(tool: String, reason: String) -> Self {
        Self {
            tool,
            limit: None,
            result: Err(reason),
        }
    }

    fn failed(&self) -> bool {
        match &self.result {
            Ok(result) => result.outcome.is_error(),
            Err(_) => true,
        }
    }

    fn into_section(self, index: usize) -> Section {
        let result = match self.result {
            Ok(result) => truncation::apply_for_tool(
                result,
                self.limit.unwrap_or(OutputLimit::Head),
                Some(self.tool.as_str()),
            ),
            Err(error) => ToolResult::error(error),
        };
        Section::new(index, &self.tool, &result)
    }
}

async fn run_call(call: BatchCall, tools: &ToolRegistry) -> CallOutcome {
    if call.tool.eq_ignore_ascii_case("batch") {
        return CallOutcome::rejected(call.tool, "cannot nest batch inside batch".to_string());
    }

    let Some(tool) = tools.get(&call.tool) else {
        let reason = format!("unknown tool: {}", call.tool);
        return CallOutcome::rejected(call.tool, reason);
    };

    let limit = tool.output_limit();
    let result = tool.call(call.parameters).await;
    CallOutcome {
        tool: call.tool,
        limit: Some(limit),
        result: Ok(result),
    }
}

async fn run_sequential(
    calls: Vec<BatchCall>,
    batch_id: &str,
    tools: &ToolRegistry,
) -> Vec<CallOutcome> {
    let mut outcomes = Vec::with_capacity(calls.len());
    let mut aborted = false;
    for (i, call) in calls.into_iter().enumerate() {
        if aborted {
            outcomes.push(CallOutcome::rejected(
                call.tool,
                "skipped after earlier failure (sequential=true)".to_string(),
            ));
            continue;
        }
        let outcome = TOOL_EXEC_ID
            .scope(
                exec_context::batch_sub_exec_id(batch_id, i),
                run_call(call, tools),
            )
            .await;
        aborted = outcome.failed();
        outcomes.push(outcome);
    }
    outcomes
}

async fn run_concurrent(
    calls: Vec<BatchCall>,
    batch_id: &str,
    tools: &ToolRegistry,
) -> Vec<CallOutcome> {
    let calls: Vec<(usize, BatchCall)> = calls.into_iter().enumerate().collect();
    tool_grouping::execute_grouped(
        calls,
        |(_, call)| tool_grouping::file_path_key(&call.tool, &call.parameters),
        move |(i, call)| {
            let exec_id = exec_context::batch_sub_exec_id(batch_id, i);
            TOOL_EXEC_ID.scope(exec_id, run_call(call, tools))
        },
    )
    .await
}

struct Section {
    header: String,
    text: String,
}

impl Section {
    fn new(index: usize, tool: &str, result: &ToolResult) -> Self {
        let status = if result.outcome.is_error() {
            "error"
        } else {
            "ok"
        };
        let mut text = String::new();
        for part in &result.content {
            match part {
                ToolResultContentPart::Text(part) => text.push_str(&part.text),
                ToolResultContentPart::Image(_) => text.push_str("[image]"),
            }
        }
        Self {
            header: format!("--- [{index}] {tool} [{status}] ---\n"),
            text,
        }
    }

    fn len(&self) -> usize {
        self.header.len() + self.text.len() + 2
    }
}

fn render_sections(sections: &[Section]) -> String {
    let total_size: usize = sections.iter().map(Section::len).sum();
    if total_size <= MAX_TOTAL_OUTPUT {
        return join_sections(sections, total_size);
    }

    let saved_path = truncation::save_batch_output(&join_sections(sections, total_size + 256));

    let overhead_per_item = 80;
    let usable = MAX_TOTAL_OUTPUT.saturating_sub(sections.len() * overhead_per_item + 256);
    let per_item_budget = usable / sections.len();

    let mut output = String::new();
    for section in sections {
        output.push_str(&section.header);
        if section.text.len() <= per_item_budget {
            output.push_str(&section.text);
        } else {
            let file_ref = extract_file_path(&section.text);
            let truncated = truncation::truncate_to_budget(&section.text, per_item_budget);
            output.push_str(truncated);
            let omitted = section.text.len() - truncated.len();
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
    output
}

fn join_sections(sections: &[Section], capacity: usize) -> String {
    let mut output = String::with_capacity(capacity);
    for section in sections {
        output.push_str(&section.header);
        output.push_str(&section.text);
        output.push_str("\n\n");
    }
    output
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
        "Execute multiple tool calls concurrently to reduce latency. Each call gets the same output limits as a standalone call. If any child call fails, the batch result is an error and the per-call sections show which call failed. Do NOT nest batch inside batch.\n\nGood for: reading multiple files, grep+glob combos, multiple bash commands, multi-part edits.\nBad for: operations that depend on prior output, ordered stateful mutations.\n\nSet sequential=true to run calls strictly in submission order, stopping at the first failure. Use for dependent operations (e.g. mkdir then cd then run)."
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
        OutputLimit::Head
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
        let batch_id = exec_context::current_exec_id();

        let outcomes = if params.sequential {
            run_sequential(params.tool_calls, &batch_id, &self.tools).await
        } else {
            run_concurrent(params.tool_calls, &batch_id, &self.tools).await
        };

        let mut error_count = 0;
        let sections: Vec<Section> = outcomes
            .into_iter()
            .enumerate()
            .map(|(i, outcome)| {
                if outcome.failed() {
                    error_count += 1;
                }
                outcome.into_section(i)
            })
            .collect();
        let success_count = total - error_count;

        let mut output = render_sections(&sections);
        output.push_str(&format!(
            "batch: {success_count}/{total} succeeded, {error_count} failed"
        ));

        if error_count > 0 {
            ToolResult::error(output)
        } else {
            ToolResult::text(output)
        }
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
    async fn tool_calls_arrive_as_a_json_string() {
        let tool = BatchTool::new(ToolRegistry::new());
        let result = tool
            .execute(serde_json::json!({
                "tool_calls": r#"[{"tool":"does-not-exist","parameters":{}}]"#
            }))
            .await;
        let text = match &result.content[0] {
            ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(
            text.contains("unknown tool"),
            "the string should have parsed as the array it holds: {text}"
        );
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
        assert!(
            result.outcome.is_error(),
            "batch should be an error when any child fails: {text}"
        );
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

            fn output_limit(&self) -> mush_agent::tool::OutputLimit {
                mush_agent::tool::OutputLimit::Head
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
        assert!(text.contains("Full output:"));
        assert!(text.contains("line 0"));
        assert!(!text.contains("line 4999"));
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

            fn output_limit(&self) -> mush_agent::tool::OutputLimit {
                mush_agent::tool::OutputLimit::Head
            }
            async fn execute(&self, params: serde_json::Value) -> ToolResult {
                let idx = params.get("idx").and_then(|v| v.as_u64()).unwrap_or(0);
                let text = format!("MARKER_{idx}\n{}", "x".repeat(20_000));
                ToolResult::text(text)
            }
        }

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

        for i in 0..10 {
            assert!(
                text.contains(&format!("[{i}] big")),
                "missing header for tool {i}: {text}"
            );
        }

        assert!(
            text.contains("full output:"),
            "missing spillover file path: {text}"
        );

        let markers_visible: Vec<bool> = (0..10)
            .map(|i| text.contains(&format!("MARKER_{i}")))
            .collect();
        assert!(
            markers_visible.iter().all(|v| *v),
            "not all markers visible (fair truncation failed): {markers_visible:?}"
        );

        assert!(text.contains("batch: 10/10 succeeded, 0 failed"));
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
            fn output_limit(&self) -> mush_agent::tool::OutputLimit {
                mush_agent::tool::OutputLimit::Head
            }
            async fn execute(&self, params: serde_json::Value) -> ToolResult {
                let idx = params.get("idx").and_then(|v| v.as_u64()).unwrap_or(0);
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

        for i in 0..8 {
            assert!(
                text.contains(&format!("/tmp/tool_{i}.txt")),
                "missing file path for tool {i}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_file_edits_applied_in_order() {
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
            fn output_limit(&self) -> mush_agent::tool::OutputLimit {
                mush_agent::tool::OutputLimit::Head
            }
            async fn execute(&self, params: serde_json::Value) -> ToolResult {
                let dir = self.dir.clone();
                let path = dir.join(params["path"].as_str().unwrap());
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
            fn output_limit(&self) -> mush_agent::tool::OutputLimit {
                mush_agent::tool::OutputLimit::Head
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
            fn output_limit(&self) -> mush_agent::tool::OutputLimit {
                mush_agent::tool::OutputLimit::Head
            }
            async fn execute(&self, params: serde_json::Value) -> ToolResult {
                let id = params["id"].as_str().unwrap_or("?").to_string();
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
            fn output_limit(&self) -> mush_agent::tool::OutputLimit {
                mush_agent::tool::OutputLimit::Head
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

            fn output_limit(&self) -> mush_agent::tool::OutputLimit {
                mush_agent::tool::OutputLimit::Head
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
}
