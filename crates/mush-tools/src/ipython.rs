use std::path::Path;
use std::sync::Arc;

use mush_agent::tool::{AgentTool, OutputLimit, ToolResult, parse_tool_args};
use mush_kernel::{CellOutput, HostError, HostRequestHandler, Kernel, KernelConfig, KernelError};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::util::extract_text;

const DESCRIPTION: &str = "Execute Python code in a persistent Python REPL. Top-level `await` works. \
Variables, imports, functions and loaded data persist across calls. The value of a trailing \
expression is returned. Preloaded: `bash(command, timeout=None)` starts a shell command in the \
background and returns a handle; `await bash(...)` or `await h` waits for the result with \
`.exit_code`, `.ok` and `.output`, and `await h.poll()`, `await h.output()`, `await h.tail(n)`, \
`await h.kill()` inspect a running one. `await edit(path, old, new)` replaces exactly one \
occurrence of `old` in the file at `path`. Use Python itself for reading, searching and \
transforming files.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IpythonArgs {
    code: String,
}

pub struct MushHost {
    cwd: Arc<Path>,
    jobs: crate::kernel_jobs::KernelJobs,
    edit: Arc<dyn AgentTool>,
}

impl MushHost {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self {
            edit: Arc::new(crate::edit::EditTool::new(cwd.clone())),
            jobs: crate::kernel_jobs::KernelJobs::default(),
            cwd,
        }
    }

    #[must_use]
    pub fn jobs(&self) -> &crate::kernel_jobs::KernelJobs {
        &self.jobs
    }
}

fn job_reply(snapshot: &crate::kernel_jobs::JobSnapshot) -> Value {
    let end = snapshot.end.as_ref().map(|end| match end {
        crate::kernel_jobs::JobEnd::Exited(code) => json!({ "kind": "exited", "code": code }),
        crate::kernel_jobs::JobEnd::TimedOut => json!({ "kind": "timed_out" }),
        crate::kernel_jobs::JobEnd::Killed => json!({ "kind": "killed" }),
        crate::kernel_jobs::JobEnd::Failed(reason) => json!({ "kind": "failed", "reason": reason }),
    });
    json!({
        "job_id": snapshot.job_id,
        "pid": snapshot.pid,
        "running": snapshot.running,
        "exit_code": snapshot.exit_code,
        "output": snapshot.output,
        "end": end,
        "duration_ms": snapshot.duration.as_millis(),
    })
}

fn job_id(payload: &Value) -> Result<&str, HostError> {
    payload
        .get("job_id")
        .and_then(Value::as_str)
        .ok_or_else(|| HostError("job_id is required".into()))
}

#[async_trait::async_trait]
impl HostRequestHandler for MushHost {
    async fn handle(&self, kind: &str, payload: Value) -> Result<Value, HostError> {
        match kind {
            "bash.start" => {
                let command = payload
                    .get("command")
                    .and_then(Value::as_str)
                    .ok_or("bash needs a command")?;
                let timeout = payload
                    .get("timeout")
                    .and_then(Value::as_u64)
                    .map(std::time::Duration::from_secs);
                let (job_id, pid) = self
                    .jobs
                    .start(&self.cwd, command, timeout)
                    .await
                    .map_err(HostError)?;
                Ok(json!({ "job_id": job_id, "pid": pid }))
            }
            "bash.wait" => Ok(job_reply(
                &self.jobs.wait(job_id(&payload)?).await.map_err(HostError)?,
            )),
            "bash.poll" => Ok(job_reply(
                &self
                    .jobs
                    .snapshot(job_id(&payload)?)
                    .await
                    .map_err(HostError)?,
            )),
            "bash.kill" => {
                self.jobs.kill(job_id(&payload)?).await.map_err(HostError)?;
                Ok(json!({}))
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
    output_sink: Option<crate::bash::OutputSink>,
}

const LIVE_OUTPUT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

impl IpythonTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self::with_mush_host(MushHost::new(cwd.clone()), cwd)
    }

    pub fn with_mush_host(host: MushHost, cwd: Arc<Path>) -> Self {
        Self::with_host(cwd, Arc::new(host))
    }

    pub fn with_host(cwd: Arc<Path>, host: Arc<dyn HostRequestHandler>) -> Self {
        Self {
            config: KernelConfig {
                cwd: cwd.to_path_buf(),
                ..KernelConfig::default()
            },
            host,
            kernel: tokio::sync::Mutex::new(None),
            output_sink: None,
        }
    }

    #[must_use]
    pub fn with_output_sink(mut self, sink: crate::bash::OutputSink) -> Self {
        self.output_sink = Some(sink);
        self
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
        let sink = self.output_sink.clone();
        let exec_id = mush_agent::exec_context::current_exec_id();
        let mut live = String::new();
        let mut last_emit = std::time::Instant::now() - LIVE_OUTPUT_INTERVAL;
        let mut pending = false;
        let outcome = kernel
            .execute_streaming(code, |event| {
                let Some(sink) = &sink else { return };
                match event {
                    mush_kernel::Event::Stdout { text, .. }
                    | mush_kernel::Event::Stderr { text, .. } => live.push_str(text),
                    _ => return,
                }
                pending = true;
                if last_emit.elapsed() >= LIVE_OUTPUT_INTERVAL {
                    sink(&exec_id, live.trim_end());
                    last_emit = std::time::Instant::now();
                    pending = false;
                }
            })
            .await;
        if pending && let Some(sink) = &sink {
            sink(&exec_id, live.trim_end());
        }
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
    async fn awaiting_bash_gives_the_finished_result() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let result = tool
            .execute(json!({
                "code": "r = await bash('echo out; echo err >&2; exit 3')\n(r.exit_code, r.ok, r.output)"
            }))
            .await;

        assert_eq!(extract_text(&result), "(3, False, 'out\\nerr\\n')");
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn a_handle_survives_the_cell_and_can_be_polled_and_killed() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let started = tool
            .execute(json!({ "code": "h = bash('echo go; sleep 30; echo never')\nh.job_id" }))
            .await;
        let polled = tool
            .execute(json!({
                "code": "import asyncio\nawait asyncio.sleep(0.3)\n(await h.running(), await h.tail(1), h.pid is not None)"
            }))
            .await;
        let killed = tool
            .execute(json!({ "code": "await h.kill()\nr = await h\n(r.killed, r.exit_code, 'never' in r.output)" }))
            .await;

        assert_eq!(extract_text(&started), "(no output)");
        assert_eq!(
            extract_text(&polled),
            "(True, 'go', True)",
            "{}",
            extract_text(&polled)
        );
        assert_eq!(
            extract_text(&killed),
            "(True, -1, False)",
            "{}",
            extract_text(&killed)
        );
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn an_unawaited_finish_reaches_the_notice_sink() {
        let dir = tempfile::tempdir().unwrap();
        let host = MushHost::new(dir.path().into());
        let notices = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        host.jobs().set_notice_sink({
            let notices = notices.clone();
            Arc::new(move |text| notices.lock().unwrap().push(text))
        });
        let tool = IpythonTool::with_mush_host(host, dir.path().into());

        let result = tool
            .execute(
                json!({ "code": "h = bash('exit 5')\nawait asyncio.sleep(0.3)\n'left running'" }),
            )
            .await;

        assert!(result.outcome.is_success(), "{}", extract_text(&result));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let notices = notices.lock().unwrap().clone();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("exit 5"), "{notices:?}");
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

    #[tokio::test]
    async fn live_output_streams_through_the_sink_while_a_cell_runs() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink: crate::bash::OutputSink = {
            let seen = seen.clone();
            Arc::new(move |_exec_id: &str, text: &str| {
                seen.lock().unwrap().push(text.to_string());
            })
        };
        let tool = tool(dir.path()).with_output_sink(sink);

        let result = tool
            .execute(json!({
                "code": "import time\nfor i in range(3):\n    print('tick', i)\n    time.sleep(0.15)"
            }))
            .await;

        assert!(result.outcome.is_success(), "{}", extract_text(&result));
        let seen = seen.lock().unwrap().clone();
        assert!(seen.len() >= 2, "{seen:?}");
        assert_eq!(seen[0], "tick 0", "{seen:?}");
        assert_eq!(seen.last().unwrap(), "tick 0\ntick 1\ntick 2", "{seen:?}");
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
