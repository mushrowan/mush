//! bash tool - executes shell commands with timeout

use std::path::PathBuf;
use std::process::Stdio;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

const DEFAULT_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BashOutput {
    #[default]
    Text,
    Json,
}

impl BashOutput {
    fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    #[serde(default = "default_timeout_secs")]
    timeout: u64,
    #[serde(default)]
    output: BashOutput,
    /// run the command in the background and return a job id immediately.
    /// use bash_status to poll for completion. keeps API cache warm during
    /// long-running commands like nix builds or large test suites
    #[serde(default)]
    background: bool,
    /// allow starting a background job when one is already running.
    /// defaults to false to prevent accidental CPU overload.
    /// hard-capped at 3 concurrent background jobs regardless
    #[serde(default)]
    concurrent: bool,
}

const fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

/// sender for streaming partial output lines from bash
pub type OutputSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

pub struct BashTool {
    cwd: PathBuf,
    /// optional callback for streaming output lines as they arrive
    output_sink: Option<OutputSink>,
    /// shared registry for background jobs (None = background mode disabled)
    bg_registry: Option<crate::background::BackgroundJobRegistry>,
}

impl BashTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            output_sink: None,
            bg_registry: None,
        }
    }

    pub fn with_output_sink(mut self, sink: OutputSink) -> Self {
        self.output_sink = Some(sink);
        self
    }

    pub fn with_background_registry(
        mut self,
        registry: crate::background::BackgroundJobRegistry,
    ) -> Self {
        self.bg_registry = Some(registry);
        self
    }
}

impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn label(&self) -> &str {
        "Bash"
    }
    fn description(&self) -> &str {
        "Execute a bash command in the current working directory. Returns stdout and stderr. \
         Output is truncated to the last 2000 lines or 50KB (whichever is hit first). \
         If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds. \
         For long-running commands (builds, test suites), set background: true to run \
         asynchronously and poll with bash_status to avoid cache busting. \
         Prefer the grep tool over bash grep/rg for searching file contents, \
         and find/glob tools over bash find for locating files."
    }

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Tail
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "bash command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "timeout in seconds (optional, default 120)"
                },
                "output": {
                    "type": "string",
                    "description": "output format: 'text' (default) or 'json' with structured fields (stdout, stderr, exit_code, timed_out)",
                    "enum": ["text", "json"]
                },
                "background": {
                    "type": "boolean",
                    "description": "run in background, return job id immediately. poll with bash_status tool. use for commands >2min to keep cache warm"
                },
                "concurrent": {
                    "type": "boolean",
                    "description": "allow running alongside existing background jobs (default: only 1 at a time, hard cap: 3)"
                }
            },
            "required": ["command"]
        })
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let args = match parse_tool_args::<BashArgs>(args) {
                Ok(args) => args,
                Err(error) => return error,
            };

            if args.background {
                return self.execute_background(args).await;
            }

            run_command(
                &self.cwd,
                &args.command,
                args.timeout,
                self.output_sink.as_ref(),
                args.output.is_json(),
            )
            .await
        })
    }
}

impl BashTool {
    async fn execute_background(&self, args: BashArgs) -> ToolResult {
        let registry = match &self.bg_registry {
            Some(r) => r,
            None => {
                return ToolResult::error(
                    "background execution not available (no job registry configured)",
                );
            }
        };

        if let Err(msg) = registry.check_can_start(args.concurrent).await {
            return ToolResult::error(msg);
        }

        let id = registry.next_id();
        let state = crate::background::JobState {
            id: id.clone(),
            command: args.command.clone(),
            status: crate::background::JobStatus::Running,
            stdout: String::new(),
            stderr: String::new(),
            started: std::time::Instant::now(),
            cwd: self.cwd.clone(),
        };

        let handle = registry.insert(state).await;
        let timeout_secs = args.timeout;
        let command = args.command.clone();
        let cwd = self.cwd.clone();

        // spawn the command in a background task
        tokio::spawn(async move {
            run_background_command(handle, &cwd, &command, timeout_secs).await;
        });

        let json = serde_json::json!({
            "job_id": id,
            "status": "running",
            "command": args.command,
            "message": format!(
                "command started in background. poll with bash_status tool using job_id: {id}"
            ),
        });
        ToolResult::text(json.to_string())
    }
}

/// run a command and stream output into a shared job state
async fn run_background_command(
    handle: std::sync::Arc<tokio::sync::RwLock<crate::background::JobState>>,
    cwd: &std::path::Path,
    command: &str,
    timeout_secs: u64,
) {
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let mut state = handle.write().await;
            state.status = crate::background::JobStatus::Failed(format!("failed to spawn: {e}"));
            return;
        }
    };

    let timeout = tokio::time::Duration::from_secs(timeout_secs);
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_handle = tokio::spawn(stream_pipe(stdout_pipe, None));
    let stderr_handle = tokio::spawn(stream_pipe(stderr_pipe, None));

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            let stdout = stdout_handle.await.unwrap_or_default();
            let stderr = stderr_handle.await.unwrap_or_default();
            let mut state = handle.write().await;
            state.stdout = stdout;
            state.stderr = stderr;
            state.status = crate::background::JobStatus::Failed(format!("command failed: {e}"));
            return;
        }
        Err(_) => {
            let _ = child.kill().await;
            let stdout = stdout_handle.await.unwrap_or_default();
            let stderr = stderr_handle.await.unwrap_or_default();
            let mut state = handle.write().await;
            state.stdout = stdout;
            state.stderr = stderr;
            state.status = crate::background::JobStatus::TimedOut;
            return;
        }
    };

    let stdout = stdout_handle.await.unwrap_or_default();
    let stderr = stderr_handle.await.unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);

    let mut state = handle.write().await;
    state.stdout = stdout;
    state.stderr = stderr;
    state.status = crate::background::JobStatus::Done { exit_code };
}

async fn run_command(
    cwd: &std::path::Path,
    command: &str,
    timeout_secs: u64,
    output_sink: Option<&OutputSink>,
    json_output: bool,
) -> ToolResult {
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // isolate child from the TUI's process group so it can't write to the
    // controlling terminal (which would inject bytes into crossterm's parser)
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("failed to spawn command: {e}")),
    };

    let started = std::time::Instant::now();
    let timeout = tokio::time::Duration::from_secs(timeout_secs);

    // stream stdout and stderr concurrently, forwarding lines to sink
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_handle = tokio::spawn(stream_pipe(stdout_pipe, output_sink.cloned()));
    let stderr_handle = tokio::spawn(stream_pipe(stderr_pipe, output_sink.cloned()));

    // wait for the process, or kill on timeout
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return ToolResult::error(format!("command failed: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            let stdout = stdout_handle.await.unwrap_or_default();
            let stderr = stderr_handle.await.unwrap_or_default();
            let duration = started.elapsed();
            if json_output {
                return format_result(stdout, stderr, -1, true, duration, true);
            }
            return ToolResult::error(format!("command timed out after {timeout_secs}s"));
        }
    };

    let stdout = stdout_handle.await.unwrap_or_default();
    let stderr = stderr_handle.await.unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);
    let duration = started.elapsed();

    format_result(stdout, stderr, exit_code, false, duration, json_output)
}

fn format_result(
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
    duration: std::time::Duration,
    json_output: bool,
) -> ToolResult {
    let duration_secs = (duration.as_secs_f32() * 10.0).round() / 10.0;

    if json_output {
        let json = serde_json::json!({
            "stdout": &stdout,
            "stderr": &stderr,
            "exit_code": exit_code,
            "timed_out": timed_out,
            "duration_seconds": duration_secs,
            "stdout_lines": stdout.lines().count(),
            "stderr_lines": stderr.lines().count(),
            "stdout_bytes": stdout.len(),
            "stderr_bytes": stderr.len(),
        });
        if exit_code != 0 || timed_out {
            ToolResult {
                content: vec![mush_ai::types::ToolResultContentPart::Text(
                    mush_ai::types::TextContent {
                        text: json.to_string(),
                    },
                )],
                outcome: mush_ai::types::ToolOutcome::Error,
            }
        } else {
            ToolResult::text(json.to_string())
        }
    } else {
        let mut output = String::new();

        if !stdout.is_empty() {
            output.push_str(&stdout);
        }

        if !stderr.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&stderr);
        }

        if output.is_empty() {
            output = "(no output)".into();
        }

        // structured preamble so the model sees exit status without parsing
        let preamble = format!("Exit code: {exit_code}\nDuration: {duration_secs}s\n\n");

        // agent loop handles truncation (middle-out with actionable hint)
        if exit_code != 0 {
            let text = format!("{preamble}{output}");
            ToolResult {
                content: vec![mush_ai::types::ToolResultContentPart::Text(
                    mush_ai::types::TextContent { text },
                )],
                outcome: mush_ai::types::ToolOutcome::Error,
            }
        } else {
            ToolResult::text(format!("{preamble}{output}"))
        }
    }
}

/// read a pipe line-by-line, forwarding to sink with throttling
async fn stream_pipe<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    sink: Option<OutputSink>,
) -> String {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let Some(pipe) = pipe else {
        return String::new();
    };
    let mut reader = BufReader::new(pipe);
    let mut output = String::new();
    let mut line = String::new();
    let mut last_emit = std::time::Instant::now();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                output.push_str(&line);
                // throttle sink to avoid flooding the TUI (~10 updates/sec)
                if let Some(ref sink) = sink
                    && last_emit.elapsed() >= std::time::Duration::from_millis(100)
                {
                    sink(line.trim_end());
                    last_emit = std::time::Instant::now();
                }
            }
            Err(_) => break,
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;

    #[tokio::test]
    async fn run_echo_command() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo hello", 10, None, false).await;
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("hello"));
    }

    #[tokio::test]
    async fn run_failing_command() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "exit 1", 10, None, false).await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("Exit code: 1"));
    }

    #[tokio::test]
    async fn run_command_with_stderr() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo error >&2", 10, None, false).await;
        let text = extract_text(&result);
        assert!(text.contains("error"));
    }

    #[tokio::test]
    async fn run_command_timeout() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "sleep 30", 1, None, false).await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("timed out"));
    }

    #[tokio::test]
    async fn run_json_output() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo hello && echo err >&2", 10, None, true).await;
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(json["stdout"].as_str().unwrap().contains("hello"));
        assert!(json["stderr"].as_str().unwrap().contains("err"));
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["timed_out"], false);
    }

    #[tokio::test]
    async fn run_json_output_failure() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo oops >&2; exit 42", 10, None, true).await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["exit_code"], 42);
        assert!(json["stderr"].as_str().unwrap().contains("oops"));
    }

    #[test]
    fn output_limit_is_tail() {
        use mush_agent::tool::OutputLimit;
        let tool = BashTool::new(PathBuf::from("."));
        assert_eq!(tool.output_limit(), OutputLimit::Tail);
    }

    #[tokio::test]
    async fn background_returns_job_id() {
        let registry = crate::background::BackgroundJobRegistry::new();
        let tool = BashTool::new(std::env::current_dir().unwrap())
            .with_background_registry(registry.clone());

        let result = tool
            .execute(serde_json::json!({
                "command": "echo hello",
                "background": true
            }))
            .await;
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("bg_0"), "should contain job id: {text}");
        assert!(text.contains("running"), "should show running: {text}");
    }

    #[tokio::test]
    async fn background_without_registry_errors() {
        let tool = BashTool::new(std::env::current_dir().unwrap());
        let result = tool
            .execute(serde_json::json!({
                "command": "echo hello",
                "background": true
            }))
            .await;
        assert!(result.outcome.is_error());
    }

    #[tokio::test]
    async fn background_job_completes() {
        let registry = crate::background::BackgroundJobRegistry::new();
        let tool = BashTool::new(std::env::current_dir().unwrap())
            .with_background_registry(registry.clone());

        let result = tool
            .execute(serde_json::json!({
                "command": "echo background_done",
                "background": true
            }))
            .await;
        assert!(
            result.outcome.is_success(),
            "background spawn should succeed"
        );

        // wait a bit for the background task to finish
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let handle = registry.get("bg_0").await.unwrap();
        let state = handle.read().await;
        assert!(
            !state.status.is_running(),
            "job should have completed: {:?}",
            state.status
        );
        assert!(
            state.stdout.contains("background_done"),
            "should capture output: {}",
            state.stdout
        );
    }

    #[tokio::test]
    async fn background_concurrency_guard() {
        let registry = crate::background::BackgroundJobRegistry::new();
        let tool = BashTool::new(std::env::current_dir().unwrap())
            .with_background_registry(registry.clone());

        // start a long-running background job
        let result = tool
            .execute(serde_json::json!({
                "command": "sleep 10",
                "background": true
            }))
            .await;
        assert!(
            result.outcome.is_success(),
            "first background job should start"
        );

        // second job without concurrent flag should fail
        let result = tool
            .execute(serde_json::json!({
                "command": "echo second",
                "background": true
            }))
            .await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(
            text.contains("concurrent: true"),
            "should suggest flag: {text}"
        );
    }
}
