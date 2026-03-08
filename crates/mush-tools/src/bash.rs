//! bash tool - executes shell commands with timeout

use std::path::PathBuf;
use std::process::Stdio;

use mush_agent::tool::{AgentTool, ToolResult};

const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// sender for streaming partial output lines from bash
pub type OutputSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

pub struct BashTool {
    cwd: PathBuf,
    /// optional callback for streaming output lines as they arrive
    output_sink: Option<OutputSink>,
}

impl BashTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            output_sink: None,
        }
    }

    pub fn with_output_sink(mut self, sink: OutputSink) -> Self {
        self.output_sink = Some(sink);
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
         Output is truncated to 2000 lines or 50KB (whichever is hit first). If truncated, \
         full output is saved to a temp file. Optionally provide a timeout in seconds."
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
            let Some(command) = args["command"].as_str() else {
                return ToolResult::error("missing required parameter: command");
            };

            let timeout = args["timeout"].as_u64().unwrap_or(DEFAULT_TIMEOUT_SECS);
            let json_output = args["output"].as_str() == Some("json");

            run_command(&self.cwd, command, timeout, self.output_sink.as_ref(), json_output).await
        })
    }
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
            if json_output {
                return format_result(stdout, stderr, -1, true, true);
            }
            return ToolResult::error(format!("command timed out after {timeout_secs}s"));
        }
    };

    let stdout = stdout_handle.await.unwrap_or_default();
    let stderr = stderr_handle.await.unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);
    let timed_out = false;

    format_result(stdout, stderr, exit_code, timed_out, json_output)
}

fn format_result(
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
    json_output: bool,
) -> ToolResult {
    // truncation is handled by the agent layer (truncation::truncate_tool_output),
    // so we just pass the raw output through here

    if json_output {
        let json = serde_json::json!({
            "stdout": &stdout,
            "stderr": &stderr,
            "exit_code": exit_code,
            "timed_out": timed_out,
            "stdout_lines": stdout.lines().count(),
            "stderr_lines": stderr.lines().count(),
            "stdout_bytes": stdout.len(),
            "stderr_bytes": stderr.len(),
        });
        if exit_code != 0 || timed_out {
            ToolResult {
                content: vec![mush_ai::types::ToolResultContentPart::Text(
                    mush_ai::types::TextContent { text: json.to_string() },
                )],
                outcome: mush_ai::types::ToolOutcome::Error,
            }
        } else {
            ToolResult::text(json.to_string())
        }
    } else {
        let mut text = String::new();

        if !stdout.is_empty() {
            text.push_str(&stdout);
        }

        if !stderr.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&stderr);
        }

        if text.is_empty() {
            text = "(no output)".into();
        }

        if exit_code != 0 {
            text.push_str(&format!("\n\nCommand exited with code {exit_code}"));
            ToolResult {
                content: vec![mush_ai::types::ToolResultContentPart::Text(
                    mush_ai::types::TextContent { text },
                )],
                outcome: mush_ai::types::ToolOutcome::Error,
            }
        } else {
            ToolResult::text(text)
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
        assert!(text.contains("exited with code 1"));
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
}
