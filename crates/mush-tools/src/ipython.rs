use std::path::Path;
use std::sync::Arc;

use mush_agent::tool::{AgentTool, OutputLimit, ToolResult, parse_tool_args};
use mush_kernel::{CellOutput, HostError, HostRequestHandler, Kernel, KernelConfig, KernelError};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::util::extract_text;

const DESCRIPTION: &str = "Execute Python code in a persistent Python REPL. Top-level `await` works. \
Variables, imports, functions and loaded data persist across calls. The value of a trailing \
expression is returned. Preloaded: `await bash(command, timeout=None)` runs a shell command and \
returns a result with `.exit_code`, `.ok` and `.output`; `await edit(path, old, new)` replaces \
exactly one occurrence of `old` in the file at `path`. Use Python itself for reading, searching \
and transforming files.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IpythonArgs {
    code: String,
}

pub struct MushHost {
    bash: Arc<dyn AgentTool>,
    edit: Arc<dyn AgentTool>,
}

impl MushHost {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self {
            bash: Arc::new(crate::bash::BashTool::new(cwd.clone())),
            edit: Arc::new(crate::edit::EditTool::new(cwd)),
        }
    }
}

#[async_trait::async_trait]
impl HostRequestHandler for MushHost {
    async fn handle(&self, kind: &str, payload: Value) -> Result<Value, HostError> {
        match kind {
            "bash" => {
                let command = payload
                    .get("command")
                    .and_then(Value::as_str)
                    .ok_or("bash needs a command")?;
                let mut args = json!({ "command": command, "output": "json" });
                if let Some(timeout) = payload.get("timeout").filter(|t| !t.is_null()) {
                    args["timeout"] = timeout.clone();
                }
                let result = self.bash.execute(args).await;
                let text = extract_text(&result);
                match serde_json::from_str::<Value>(&text) {
                    Ok(report) => {
                        let stdout = report["stdout"].as_str().unwrap_or_default();
                        let stderr = report["stderr"].as_str().unwrap_or_default();
                        let mut output = stdout.to_string();
                        if !stderr.is_empty() {
                            if !output.is_empty() && !output.ends_with('\n') {
                                output.push('\n');
                            }
                            output.push_str(stderr);
                        }
                        Ok(json!({
                            "exit_code": report["exit_code"].as_i64().unwrap_or(-1),
                            "output": output,
                            "timed_out": report["timed_out"].as_bool().unwrap_or(false),
                        }))
                    }
                    Err(_) => Err(HostError(text)),
                }
            }
            "edit" => {
                let args = json!({
                    "path": payload.get("path").cloned().unwrap_or(Value::Null),
                    "oldText": payload.get("old").cloned().unwrap_or(Value::Null),
                    "newText": payload.get("new").cloned().unwrap_or(Value::Null),
                });
                let result = self.edit.execute(args).await;
                let text = extract_text(&result);
                if result.outcome.is_success() {
                    Ok(json!({ "summary": text }))
                } else {
                    Err(HostError(text))
                }
            }
            other => Err(HostError(format!("unknown host request {other}"))),
        }
    }
}

pub struct IpythonTool {
    config: KernelConfig,
    host: Arc<dyn HostRequestHandler>,
    kernel: tokio::sync::Mutex<Option<Kernel>>,
}

impl IpythonTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        let host: Arc<dyn HostRequestHandler> = Arc::new(MushHost::new(cwd.clone()));
        Self::with_host(cwd, host)
    }

    pub fn with_host(cwd: Arc<Path>, host: Arc<dyn HostRequestHandler>) -> Self {
        Self {
            config: KernelConfig {
                cwd: cwd.to_path_buf(),
                ..KernelConfig::default()
            },
            host,
            kernel: tokio::sync::Mutex::new(None),
        }
    }

    pub async fn shutdown(&self) {
        if let Some(kernel) = self.kernel.lock().await.take() {
            let _ = kernel.shutdown().await;
        }
    }

    async fn run(&self, code: &str) -> Result<CellOutput, KernelError> {
        let mut slot = self.kernel.lock().await;
        if slot.is_none() {
            *slot = Some(Kernel::start(self.config.clone(), self.host.clone()).await?);
        }
        let kernel = slot.as_ref().ok_or(KernelError::Pipes)?;
        let outcome = kernel.execute(code).await;
        if matches!(outcome, Err(KernelError::Exited(_))) {
            *slot = None;
        }
        outcome
    }
}

#[must_use]
pub fn format_cell_output(output: &CellOutput) -> String {
    let mut text = String::new();
    if !output.stdout.is_empty() {
        text.push_str(&output.stdout);
    }
    if !output.stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&output.stderr);
    }
    if let Some(result) = &output.result {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(result);
    }
    if let Some(error) = &output.error {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if error.traceback.is_empty() {
            text.push_str(&format!("{}: {}", error.ename, error.evalue));
        } else {
            text.push_str(&error.traceback.join("\n"));
        }
    }
    if text.is_empty() {
        text.push_str("(no output)");
    }
    text
}

#[async_trait::async_trait]
impl AgentTool for IpythonTool {
    fn name(&self) -> &str {
        "ipython"
    }

    fn label(&self) -> &str {
        "Python"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "python code to run in the persistent kernel"
                }
            },
            "required": ["code"]
        })
    }

    fn output_limit(&self) -> OutputLimit {
        OutputLimit::Tail
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let args = match parse_tool_args::<IpythonArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };
        match self.run(&args.code).await {
            Ok(output) => {
                let text = format_cell_output(&output);
                if output.ok {
                    ToolResult::text(text)
                } else {
                    ToolResult::error(text)
                }
            }
            Err(error) => ToolResult::error(format!("kernel: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_kernel::CellError;

    fn tool(dir: &Path) -> IpythonTool {
        IpythonTool::new(dir.into())
    }

    #[tokio::test]
    async fn state_persists_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let first = tool.execute(json!({ "code": "total = 40" })).await;
        let second = tool.execute(json!({ "code": "total + 2" })).await;

        assert!(first.outcome.is_success(), "{}", extract_text(&first));
        assert_eq!(extract_text(&first), "(no output)");
        assert_eq!(extract_text(&second), "42");
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn a_failing_cell_is_an_error_result_with_the_traceback() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let result = tool
            .execute(json!({ "code": "print('before')\n{}['k']" }))
            .await;

        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.starts_with("before\n"), "{text}");
        assert!(text.contains("KeyError: 'k'"), "{text}");
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn bash_runs_through_the_host_and_reports_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let result = tool
            .execute(json!({
                "code": "r = await bash('echo out; echo err >&2; exit 3')\n(r.exit_code, r.output)"
            }))
            .await;

        assert_eq!(extract_text(&result), "(3, 'out\\nerr\\n')");
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn edit_runs_through_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let tool = tool(dir.path());

        let result = tool
            .execute(json!({
                "code": format!("summary = await edit({:?}, 'world', 'there')\nsummary.splitlines()[0]", path.display().to_string())
            }))
            .await;

        assert!(result.outcome.is_success(), "{}", extract_text(&result));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello there\n");
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_edit_raises_in_the_cell() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let tool = tool(dir.path());

        let result = tool
            .execute(json!({
                "code": format!("await edit({:?}, 'missing', 'x')", path.display().to_string())
            }))
            .await;

        assert!(result.outcome.is_error());
        assert!(
            extract_text(&result).contains("HostError"),
            "{}",
            extract_text(&result)
        );
        tool.shutdown().await;
    }

    #[test]
    fn output_formatting_orders_stdout_stderr_result_error() {
        let output = CellOutput {
            stdout: "out".into(),
            stderr: "warn".into(),
            result: Some("7".into()),
            error: Some(CellError {
                ename: "KeyboardInterrupt".into(),
                evalue: "interrupted".into(),
                traceback: vec![],
            }),
            ok: false,
            duration: Default::default(),
        };

        assert_eq!(
            format_cell_output(&output),
            "out\n[stderr]\nwarn\n7\nKeyboardInterrupt: interrupted"
        );
    }
}
