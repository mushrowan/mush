//! bash tool - executes shell commands with timeout and output truncation

use std::path::PathBuf;
use std::process::Stdio;

use mush_agent::tool::{AgentTool, ToolResult};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_BYTES: usize = 50 * 1024;
const MAX_OUTPUT_LINES: usize = 2000;

pub struct BashTool {
    cwd: PathBuf,
}

impl BashTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
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
         Output is truncated to last 2000 lines or 50KB (whichever is hit first). \
         Optionally provide a timeout in seconds."
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

            run_command(&self.cwd, command, timeout).await
        })
    }
}

async fn run_command(cwd: &PathBuf, command: &str, timeout_secs: u64) -> ToolResult {
    let mut child = match tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("failed to spawn command: {e}")),
    };

    let timeout = tokio::time::Duration::from_secs(timeout_secs);

    // wait for the process, or kill on timeout
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return ToolResult::error(format!("command failed: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            return ToolResult::error(format!("command timed out after {timeout_secs}s"));
        }
    };

    // read stdout/stderr after process has exited
    let stdout = read_pipe(child.stdout.take()).await;
    let stderr = read_stderr_pipe(child.stderr.take()).await;
    let exit_code = status.code().unwrap_or(-1);

    let mut text = String::new();

    if !stdout.is_empty() {
        text.push_str(&truncate_output(&stdout));
    }

    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&truncate_output(&stderr));
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
            is_error: true,
        }
    } else {
        ToolResult::text(text)
    }
}

async fn read_pipe(pipe: Option<tokio::process::ChildStdout>) -> String {
    use tokio::io::AsyncReadExt;
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = pipe.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).to_string()
}

// overload for stderr pipe type
async fn read_stderr_pipe(pipe: Option<tokio::process::ChildStderr>) -> String {
    use tokio::io::AsyncReadExt;
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = pipe.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).to_string()
}

fn truncate_output(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();

    if lines.len() <= MAX_OUTPUT_LINES && output.len() <= MAX_OUTPUT_BYTES {
        return output.to_string();
    }

    // take from the end (most recent output is usually most relevant)
    let mut result_lines = Vec::new();
    let mut bytes = 0;

    for line in lines.iter().rev() {
        if result_lines.len() >= MAX_OUTPUT_LINES || bytes + line.len() >= MAX_OUTPUT_BYTES {
            break;
        }
        result_lines.push(*line);
        bytes += line.len() + 1;
    }

    result_lines.reverse();

    let truncated_count = lines.len() - result_lines.len();
    let mut result = String::new();
    if truncated_count > 0 {
        result.push_str(&format!("[{truncated_count} lines truncated]\n\n"));
    }
    result.push_str(&result_lines.join("\n"));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_output() {
        let output = "line 1\nline 2\nline 3";
        assert_eq!(truncate_output(output), output);
    }

    #[test]
    fn truncate_long_output() {
        let lines: Vec<String> = (1..=3000).map(|i| format!("line {i}")).collect();
        let output = lines.join("\n");

        let truncated = truncate_output(&output);
        assert!(truncated.contains("["));
        assert!(truncated.contains("truncated]"));
        // should contain lines from the end
        assert!(truncated.contains("line 3000"));
        assert!(!truncated.contains("line 1\n"));
    }

    #[tokio::test]
    async fn run_echo_command() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo hello", 10).await;
        assert!(!result.is_error);
        let text = extract_text(&result);
        assert!(text.contains("hello"));
    }

    #[tokio::test]
    async fn run_failing_command() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "exit 1", 10).await;
        assert!(result.is_error);
        let text = extract_text(&result);
        assert!(text.contains("exited with code 1"));
    }

    #[tokio::test]
    async fn run_command_with_stderr() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo error >&2", 10).await;
        let text = extract_text(&result);
        assert!(text.contains("error"));
    }

    #[tokio::test]
    async fn run_command_timeout() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "sleep 30", 1).await;
        assert!(result.is_error);
        let text = extract_text(&result);
        assert!(text.contains("timed out"));
    }

    fn extract_text(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|p| match p {
                mush_ai::types::ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
