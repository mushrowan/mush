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
    #[serde(
        default = "default_poll_timeout",
        deserialize_with = "mush_agent::de::uint"
    )]
    timeout: u64,
    #[serde(default)]
    action: StatusAction,
}

#[derive(Default, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum StatusAction {
    #[default]
    Poll,
    Kill,
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

#[async_trait::async_trait]
impl AgentTool for BashStatusTool {
    fn name(&self) -> &str {
        "bash_status"
    }
    fn label(&self) -> &str {
        "Bash Status"
    }
    fn description(&self) -> &str {
        "Poll a background bash job for completion. Returns current status and new output since the previous poll. \
         The timeout is only how long this poll waits, not the job timeout. If it expires \
         while the job is still running, this tool returns a successful running status. \
         Use timeout to wait up to N seconds for the job to finish (default 30s, 0 for instant check). \
         Use action=kill to stop a running background job by id. \
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
                    "description": "seconds this poll waits for completion (default 30, 0 for instant check). this does not kill the job; expiry returns running status"
                },
                "action": {
                    "type": "string",
                    "description": "action to perform: poll for status or kill the running background job",
                    "enum": ["poll", "kill"],
                    "default": "poll"
                }
            },
            "required": ["job_id"]
        })
    }

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        // matches BashTool: bash output's signal lives at the end
        mush_agent::tool::OutputLimit::Tail
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<StatusArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        // reap expired jobs while we're here
        self.registry.reap_expired().await;

        let handle = match self.registry.get(&args.job_id).await {
            Some(h) => h,
            None => {
                return ToolResult::error(format!("no background job with id: {}", args.job_id));
            }
        };

        if matches!(args.action, StatusAction::Kill) {
            {
                let mut state = handle.write().await;
                if state.status.is_running() {
                    state.finish(JobStatus::Killed);
                }
            }
            self.registry.kill(&args.job_id).await;
            let mut state = handle.write().await;
            return format_job_result(&mut state);
        }

        // poll with timeout: check periodically until done or timeout
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(args.timeout);
        let poll_interval = tokio::time::Duration::from_millis(500);

        loop {
            let state = handle.read().await;
            if !state.status.is_running() {
                drop(state);
                let mut state = handle.write().await;
                return format_job_result(&mut state);
            }
            drop(state);

            if tokio::time::Instant::now() >= deadline {
                let mut state = handle.write().await;
                return format_running_status(
                    &mut state,
                    if args.timeout > 0 {
                        PollWait::Elapsed
                    } else {
                        PollWait::Instant
                    },
                );
            }

            tokio::time::sleep(poll_interval).await;
        }
    }
}

fn format_job_result(state: &mut crate::background::JobState) -> ToolResult {
    let output = take_new_output(state);

    let (status, exit_code) = match &state.status {
        JobStatus::Done { exit_code } if *exit_code == 0 => (BackgroundStatus::Done, *exit_code),
        JobStatus::Done { exit_code } => (BackgroundStatus::Failed, *exit_code),
        JobStatus::TimedOut => (BackgroundStatus::TimedOut, -1),
        JobStatus::Killed => (BackgroundStatus::Killed, -1),
        JobStatus::Failed(e) => {
            return ToolResult::error(format!("background job {} failed: {e}", state.id));
        }
        JobStatus::Running => unreachable!("called format_job_result on running job"),
    };

    // use the stable run-time duration recorded at finish time. polling
    // a long-since-finished job must not show a duration that climbs
    // with wall-clock; that masked `Status: timed_out` once and made
    // the agent loop on bash_status thinking the job was still running
    let elapsed = state.run_duration().as_secs();
    let preamble = format!(
        "Exit code: {exit_code}\nStatus: {}\nDuration: {elapsed}s\n\n",
        status.as_str()
    );

    if status.is_error() {
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

fn take_new_output(state: &mut crate::background::JobState) -> String {
    let output = format_new_output(state);
    state.stdout_consumed_bytes = state.stdout.len();
    state.stderr_consumed_bytes = state.stderr.len();
    output
}

fn format_new_output(state: &crate::background::JobState) -> String {
    let stdout = state
        .stdout
        .get(state.stdout_consumed_bytes..)
        .unwrap_or("");
    let stderr = state
        .stderr
        .get(state.stderr_consumed_bytes..)
        .unwrap_or("");
    let mut output = String::new();

    if !stdout.is_empty() {
        output.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(stderr);
    }

    if output.is_empty() {
        if state.stdout_consumed_bytes == 0 && state.stderr_consumed_bytes == 0 {
            "(no output)".into()
        } else {
            "(no new output since previous bash_status poll)".into()
        }
    } else {
        output
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundStatus {
    Done,
    Failed,
    TimedOut,
    Killed,
}

impl BackgroundStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Killed => "killed",
        }
    }

    const fn is_error(self) -> bool {
        matches!(self, Self::Failed | Self::TimedOut)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollWait {
    Instant,
    Elapsed,
}

impl PollWait {
    const fn elapsed(self) -> bool {
        matches!(self, Self::Elapsed)
    }

    const fn message(self) -> &'static str {
        match self {
            Self::Elapsed => {
                "poll timed out, job still running. no error: poll again to check for completion"
            }
            Self::Instant => "still running. poll again to check for completion",
        }
    }

    const fn status(self) -> &'static str {
        match self {
            Self::Elapsed => "poll_timed_out",
            Self::Instant => "instant_check",
        }
    }
}

fn format_running_status(
    state: &mut crate::background::JobState,
    poll_wait: PollWait,
) -> ToolResult {
    let elapsed = state.run_duration().as_secs();
    let stdout_lines = state.stdout.lines().count();
    let stderr_lines = state.stderr.lines().count();
    let new_stdout = state
        .stdout
        .get(state.stdout_consumed_bytes..)
        .unwrap_or("")
        .lines()
        .count();
    let new_stderr = state
        .stderr
        .get(state.stderr_consumed_bytes..)
        .unwrap_or("")
        .lines()
        .count();
    let output = take_new_output(state);

    let json = serde_json::json!({
        "job_id": state.id,
        "status": "running",
        "command": state.command,
        "elapsed_seconds": elapsed,
        "poll_status": poll_wait.status(),
        "stdout_lines_so_far": stdout_lines,
        "stderr_lines_so_far": stderr_lines,
        "new_stdout_lines": new_stdout,
        "new_stderr_lines": new_stderr,
        "output": output,
        "poll_wait_elapsed": poll_wait.elapsed(),
        "message": poll_wait.message(),
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
                stdout_consumed_bytes: 0,
                stderr_consumed_bytes: 0,
                started: std::time::Instant::now(),
                finished: Some(std::time::Instant::now()),
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
    async fn completed_job_returns_only_new_output_after_first_poll() {
        let registry = make_registry();
        registry
            .insert(crate::background::JobState {
                id: "bg_0".into(),
                command: "echo hello".into(),
                status: JobStatus::Done { exit_code: 0 },
                stdout: "hello\n".into(),
                stderr: String::new(),
                stdout_consumed_bytes: 0,
                stderr_consumed_bytes: 0,
                started: std::time::Instant::now(),
                finished: Some(std::time::Instant::now()),
                cwd: std::sync::Arc::from(std::path::Path::new(".")),
            })
            .await;

        let tool = BashStatusTool::new(registry);
        let first = tool
            .execute(serde_json::json!({"job_id": "bg_0", "timeout": 0}))
            .await;
        let second = tool
            .execute(serde_json::json!({"job_id": "bg_0", "timeout": 0}))
            .await;
        let first_text = extract_text(&first);
        let second_text = extract_text(&second);

        assert!(
            first_text.contains("hello"),
            "first poll should include output: {first_text}"
        );
        assert!(
            !second_text.contains("hello"),
            "second poll should not replay consumed output: {second_text}"
        );
        assert!(
            second_text.contains("no new output"),
            "second poll should explain the missing output: {second_text}"
        );
    }

    #[tokio::test]
    async fn running_poll_consumes_only_output_seen_so_far() {
        let registry = make_registry();
        let handle = registry
            .insert(crate::background::JobState {
                id: "bg_0".into(),
                command: "slow".into(),
                status: JobStatus::Running,
                stdout: "first\n".into(),
                stderr: String::new(),
                stdout_consumed_bytes: 0,
                stderr_consumed_bytes: 0,
                started: std::time::Instant::now(),
                finished: None,
                cwd: std::sync::Arc::from(std::path::Path::new(".")),
            })
            .await;

        let tool = BashStatusTool::new(registry);
        let first = tool
            .execute(serde_json::json!({"job_id": "bg_0", "timeout": 0}))
            .await;
        {
            let mut state = handle.write().await;
            state.stdout.push_str("second\n");
        }
        let second = tool
            .execute(serde_json::json!({"job_id": "bg_0", "timeout": 0}))
            .await;

        let first_text = extract_text(&first);
        let second_text = extract_text(&second);
        assert!(first_text.contains("first"), "first poll: {first_text}");
        assert!(
            !second_text.contains("first"),
            "second poll must not replay old output: {second_text}"
        );
        assert!(
            second_text.contains("second"),
            "second poll should include new output: {second_text}"
        );
        assert!(
            second_text.contains(r#""new_stdout_lines":1"#),
            "second poll should report the delta line count: {second_text}"
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
                stdout_consumed_bytes: 0,
                stderr_consumed_bytes: 0,
                started: std::time::Instant::now(),
                finished: None,
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
        assert!(
            text.contains("poll_wait_elapsed"),
            "should make poll timeout status explicit: {text}"
        );
        assert!(
            text.contains("no error") || text.contains("poll again"),
            "should not imply the running status is a failure: {text}"
        );
    }

    #[tokio::test]
    async fn poll_timeout_on_running_job_is_successful_status() {
        let registry = make_registry();
        registry
            .insert(crate::background::JobState {
                id: "bg_0".into(),
                command: "sleep 100".into(),
                status: JobStatus::Running,
                stdout: String::new(),
                stderr: String::new(),
                stdout_consumed_bytes: 0,
                stderr_consumed_bytes: 0,
                started: std::time::Instant::now(),
                finished: None,
                cwd: std::sync::Arc::from(std::path::Path::new(".")),
            })
            .await;

        let tool = BashStatusTool::new(registry);
        let result = tool
            .execute(serde_json::json!({"job_id": "bg_0", "timeout": 1}))
            .await;
        let text = extract_text(&result);

        assert!(
            result.outcome.is_success(),
            "poll wait expiry should not be a tool error: {text}"
        );
        assert!(
            text.contains("\"poll_wait_elapsed\":true"),
            "should distinguish a poll wait expiry from instant status: {text}"
        );
        assert!(text.contains("no error"), "should say no error: {text}");
        assert!(
            text.contains(r#""poll_status":"poll_timed_out""#),
            "should expose poll timeout as a running poll status, not a job timeout: {text}"
        );
        assert!(
            text.contains("poll timed out, job still running"),
            "should not phrase poll expiry like a command failure: {text}"
        );
    }

    #[tokio::test]
    async fn kill_action_marks_running_job_killed() {
        let registry = make_registry();
        registry
            .insert(crate::background::JobState {
                id: "bg_0".into(),
                command: "sleep 100".into(),
                status: JobStatus::Running,
                stdout: "partial\n".into(),
                stderr: String::new(),
                stdout_consumed_bytes: 0,
                stderr_consumed_bytes: 0,
                started: std::time::Instant::now(),
                finished: None,
                cwd: std::sync::Arc::from(std::path::Path::new(".")),
            })
            .await;

        let tool = BashStatusTool::new(registry.clone());
        let result = tool
            .execute(serde_json::json!({
                "job_id": "bg_0",
                "action": "kill"
            }))
            .await;
        let text = extract_text(&result);

        assert!(result.outcome.is_success(), "kill should succeed: {text}");
        assert!(text.contains("killed"), "should report killed: {text}");
        let handle = registry.get("bg_0").await.unwrap();
        let state = handle.read().await;
        assert!(
            !state.status.is_running(),
            "killed job must not count as running"
        );
    }

    #[tokio::test]
    async fn kill_action_stops_spawned_background_process() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("survived");
        let marker_str = marker.display().to_string();
        let registry = make_registry();
        let bash = crate::bash::BashTool::new(std::env::current_dir().unwrap().into())
            .with_background_registry(registry.clone());
        let status = BashStatusTool::new(registry.clone());

        let started = bash
            .execute(serde_json::json!({
                "command": format!("sleep 3 && touch {marker_str}"),
                "background": true
            }))
            .await;
        assert!(started.outcome.is_success());

        let killed = status
            .execute(serde_json::json!({
                "job_id": "bg_0",
                "action": "kill"
            }))
            .await;
        let text = extract_text(&killed);
        assert!(killed.outcome.is_success(), "kill should succeed: {text}");

        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(
            !marker.exists(),
            "background child should have been killed before it touched the marker"
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
                stdout_consumed_bytes: 0,
                stderr_consumed_bytes: 0,
                started: std::time::Instant::now(),
                finished: None,
                cwd: std::sync::Arc::from(std::path::Path::new(".")),
            })
            .await;

        let tool = BashStatusTool::new(registry);
        let result = tool
            .execute(serde_json::json!({"job_id": "bg_0", "timeout": 0}))
            .await;
        assert!(result.outcome.is_error());
    }

    #[tokio::test]
    async fn duration_reports_run_time_not_wall_clock_since_start() {
        // a job that started 200s ago and finished after running for
        // 30s should report "Duration: 30s" no matter when it's polled,
        // not the 200s elapsed since `started`. losing this invariant
        // masks `Status: timed_out` behind a duration that keeps
        // climbing on every poll, making it look like the job is still
        // running
        let registry = make_registry();
        let now = std::time::Instant::now();
        let started = now - std::time::Duration::from_secs(200);
        let finished = started + std::time::Duration::from_secs(30);
        registry
            .insert(crate::background::JobState {
                id: "bg_0".into(),
                command: "nix flake check".into(),
                status: JobStatus::TimedOut,
                stdout: String::new(),
                stderr: String::new(),
                stdout_consumed_bytes: 0,
                stderr_consumed_bytes: 0,
                started,
                finished: Some(finished),
                cwd: std::sync::Arc::from(std::path::Path::new(".")),
            })
            .await;

        let tool = BashStatusTool::new(registry);
        let result = tool
            .execute(serde_json::json!({"job_id": "bg_0", "timeout": 0}))
            .await;
        let text = extract_text(&result);
        assert!(
            text.contains("Duration: 30s"),
            "should report run-time duration (30s), not wall-clock since start: {text}"
        );
        assert!(
            !text.contains("Duration: 200s"),
            "must not report wall-clock since start: {text}"
        );
    }
}
