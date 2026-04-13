//! bash_status tool - poll background bash jobs for completion

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::background::{BackgroundJobRegistry, JobStatus};

const DEFAULT_POLL_TIMEOUT_SECS: u64 = 30;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    /// job id returned by bash with background: true
    #[serde(rename = "job_id")]
    job_id: String,
    /// how long to wait for the job to finish before returning
    /// current status (default 30s, set to 0 for instant check)
    #[serde(default = "default_poll_timeout")]
    timeout: u64,
}

const fn default_poll_timeout() -> u64 {
    DEFAULT_POLL_TIMEOUT_SECS
}

pub struct BashStatusTool {
    registry: BackgroundJobRegistry,
}

impl BashStatusTool {
    pub fn new(registry: BackgroundJobRegistry) -> Self {
        Self { registry }
    }
}

impl AgentTool for BashStatusTool {
    fn name(&self) -> &str {
        "bash_status"
    }
    fn label(&self) -> &str {
        "Bash Status"
    }
    fn description(&self) -> &str {
        "Poll a background bash job for completion. Returns current status and output. \
         Use timeout to wait up to N seconds for the job to finish (default 30s, 0 for instant check). \
         Each poll keeps the API cache warm during long-running commands."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "job_id": {
                    "type": "string",
                    "description": "job id from bash background execution"
                },
                "timeout": {
                    "type": "integer",
                    "description": "seconds to wait for completion (default 30, 0 for instant check)"
                }
            },
            "required": ["job_id"]
        })
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let args = match parse_tool_args::<StatusArgs>(args) {
                Ok(args) => args,
                Err(error) => return error,
            };

            // reap expired jobs while we're here
            self.registry.reap_expired().await;

            let handle = match self.registry.get(&args.job_id).await {
                Some(h) => h,
                None => {
                    return ToolResult::error(format!(
                        "no background job with id: {}",
                        args.job_id
                    ));
                }
            };

            // poll with timeout: check periodically until done or timeout
            let deadline =
                tokio::time::Instant::now() + tokio::time::Duration::from_secs(args.timeout);
            let poll_interval = tokio::time::Duration::from_millis(500);

            loop {
                let state = handle.read().await;
                if !state.status.is_running() {
                    return format_job_result(&state);
                }
                drop(state);

                if tokio::time::Instant::now() >= deadline {
                    let state = handle.read().await;
                    return format_running_status(&state);
                }

                tokio::time::sleep(poll_interval).await;
            }
        })
    }
}

fn format_job_result(state: &crate::background::JobState) -> ToolResult {
    let mut output = String::new();
    if !state.stdout.is_empty() {
        output.push_str(&state.stdout);
    }
    if !state.stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&state.stderr);
    }
    if output.is_empty() {
        output = "(no output)".into();
    }

    let (status_str, exit_code) = match &state.status {
        JobStatus::Done { exit_code } => ("done", *exit_code),
        JobStatus::TimedOut => ("timed_out", -1),
        JobStatus::Failed(e) => {
            return ToolResult::error(format!("background job {} failed: {e}", state.id));
        }
        JobStatus::Running => unreachable!("called format_job_result on running job"),
    };

    let elapsed = state.started.elapsed().as_secs();
    let preamble =
        format!("Exit code: {exit_code}\nStatus: {status_str}\nDuration: {elapsed}s\n\n");

    if exit_code != 0 {
        ToolResult {
            content: vec![mush_ai::types::ToolResultContentPart::Text(
                mush_ai::types::TextContent {
                    text: format!("{preamble}{output}"),
                },
            )],
            outcome: mush_ai::types::ToolOutcome::Error,
        }
    } else {
        ToolResult::text(format!("{preamble}{output}"))
    }
}

fn format_running_status(state: &crate::background::JobState) -> ToolResult {
    let elapsed = state.started.elapsed().as_secs();
    let stdout_lines = state.stdout.lines().count();
    let stderr_lines = state.stderr.lines().count();

    let json = serde_json::json!({
        "job_id": state.id,
        "status": "running",
        "command": state.command,
        "elapsed_seconds": elapsed,
        "stdout_lines_so_far": stdout_lines,
        "stderr_lines_so_far": stderr_lines,
        "message": "still running. poll again to check for completion",
    });
    ToolResult::text(json.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;

    fn make_registry() -> BackgroundJobRegistry {
        BackgroundJobRegistry::new()
    }

    #[tokio::test]
    async fn status_nonexistent_job() {
        let registry = make_registry();
        let tool = BashStatusTool::new(registry);
        let result = tool.execute(serde_json::json!({"job_id": "bg_99"})).await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("bg_99"), "should mention job id: {text}");
    }

    #[tokio::test]
    async fn status_completed_job() {
        let registry = make_registry();
        let handle = registry
            .insert(crate::background::JobState {
                id: "bg_0".into(),
                command: "echo hello".into(),
                status: JobStatus::Done { exit_code: 0 },
                stdout: "hello\n".into(),
                stderr: String::new(),
                started: std::time::Instant::now(),
                cwd: std::sync::Arc::from(std::path::Path::new(".")),
            })
            .await;
        drop(handle);

        let tool = BashStatusTool::new(registry);
        let result = tool
            .execute(serde_json::json!({"job_id": "bg_0", "timeout": 0}))
            .await;
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("hello"), "should contain output: {text}");
        assert!(
            text.contains("Exit code: 0"),
            "should show exit code: {text}"
        );
    }

    #[tokio::test]
    async fn status_running_job_returns_progress() {
        let registry = make_registry();
        registry
            .insert(crate::background::JobState {
                id: "bg_0".into(),
                command: "sleep 100".into(),
                status: JobStatus::Running,
                stdout: "partial output\n".into(),
                stderr: String::new(),
                started: std::time::Instant::now(),
                cwd: std::sync::Arc::from(std::path::Path::new(".")),
            })
            .await;

        let tool = BashStatusTool::new(registry);
        let result = tool
            .execute(serde_json::json!({"job_id": "bg_0", "timeout": 0}))
            .await;
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("running"), "should show running: {text}");
        assert!(
            text.contains("stdout_lines_so_far"),
            "should show progress: {text}"
        );
    }

    #[tokio::test]
    async fn status_failed_job() {
        let registry = make_registry();
        registry
            .insert(crate::background::JobState {
                id: "bg_0".into(),
                command: "bad".into(),
                status: JobStatus::Failed("spawn failed".into()),
                stdout: String::new(),
                stderr: String::new(),
                started: std::time::Instant::now(),
                cwd: std::sync::Arc::from(std::path::Path::new(".")),
            })
            .await;

        let tool = BashStatusTool::new(registry);
        let result = tool
            .execute(serde_json::json!({"job_id": "bg_0", "timeout": 0}))
            .await;
        assert!(result.outcome.is_error());
    }
}
