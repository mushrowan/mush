use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use mush_agent::tool::{AgentTool, OutputLimit, ToolResult, parse_tool_args};
use mush_kernel::{CellOutput, HostError, HostRequestHandler, Kernel, KernelConfig, KernelError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::util::{extract_text, lock, read_lock, write_lock};

const DESCRIPTION: &str = "Execute Python code in a persistent Python REPL. Top-level `await` works. \
Variables, imports, functions and loaded data persist across calls. The value of a trailing \
expression is returned. Preloaded: `bash(command, timeout=None)` starts a shell command in the \
background and returns a handle; `await bash(...)` or `await h` waits for the result with \
`.exit_code`, `.ok` and `.output`, and `await h.poll()`, `await h.output()`, `await h.tail(n)`, \
`await h.kill()` inspect a running one. `await edit(path, old, new)` replaces exactly one \
occurrence of `old` in the file at `path`. `attach_image(path)` shows you an image file. Use \
Python itself for reading, searching and transforming files.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IpythonArgs {
    code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellDiff {
    pub path: String,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentMessage {
    pub receiver: String,
    pub status: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellErrorDetails {
    pub ename: String,
    pub evalue: String,
    pub traceback: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CellDetails {
    pub duration_ms: u64,
    pub stdout: String,
    pub stderr: String,
    pub result: Option<String>,
    pub error: Option<CellErrorDetails>,
    pub diffs: Vec<CellDiff>,
    pub images: usize,
    pub kernel_restarted: bool,
    pub sent_messages: Vec<SentMessage>,
    pub background: String,
}

impl CellDetails {
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }

    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    #[must_use]
    pub fn output_text(&self) -> String {
        let mut text = String::new();
        for part in [self.stdout.as_str(), self.stderr.as_str()]
            .into_iter()
            .chain(self.result.as_deref())
            .chain([self.background.as_str()])
        {
            if part.trim().is_empty() {
                continue;
            }
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(part);
        }
        text
    }
}

pub struct MushHost {
    cwd: Arc<Path>,
    jobs: crate::kernel_jobs::KernelJobs,
    edit: Arc<dyn AgentTool>,
    web_search: RwLock<Option<Arc<dyn AgentTool>>>,
    diffs: Mutex<HashMap<String, Vec<CellDiff>>>,
    sent: Mutex<HashMap<String, Vec<SentMessage>>>,
    extension: RwLock<Option<Arc<dyn HostRequestHandler>>>,
    sandbox: RwLock<Option<Arc<mush_sandbox::Sandbox>>>,
}

impl MushHost {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self {
            edit: crate::redaction_guard::RedactionMarkerGuard::wrap(
                Arc::new(crate::edit::EditTool::new(cwd.clone()).without_read_ledger()),
                crate::redaction_guard::GuardedTool::Edit,
                cwd.clone(),
            ),
            jobs: crate::kernel_jobs::KernelJobs::default(),
            web_search: RwLock::new(None),
            cwd,
            diffs: Mutex::new(HashMap::new()),
            sent: Mutex::new(HashMap::new()),
            extension: RwLock::new(None),
            sandbox: RwLock::new(None),
        }
    }

    pub fn take_sent_messages(&self, origin: &str) -> Vec<SentMessage> {
        lock(&self.sent).remove(origin).unwrap_or_default()
    }

    fn record_sent(&self, origin: Option<&str>, payload: &Value, reply: &Value) {
        let Some(origin) = origin else { return };
        let text = payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let receipts = match reply.get("receipts").and_then(Value::as_array) {
            Some(receipts) => receipts.clone(),
            None => vec![reply.clone()],
        };
        let mut sent = lock(&self.sent);
        let entry = sent.entry(origin.to_string()).or_default();
        for receipt in receipts {
            let status = match receipt.get("error") {
                Some(_) => "failed",
                None => receipt
                    .get("deliveryStatus")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            };
            entry.push(SentMessage {
                receiver: receipt
                    .pointer("/target/sessionName")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                status: status.to_string(),
                text: text.clone(),
            });
        }
    }

    pub fn set_sandbox(&self, sandbox: Option<Arc<mush_sandbox::Sandbox>>) {
        self.jobs.set_sandbox(sandbox.clone());
        *write_lock(&self.sandbox) = sandbox;
    }

    pub fn set_web_search(&self, tool: Arc<dyn AgentTool>) {
        *write_lock(&self.web_search) = Some(tool);
    }

    pub fn set_extension(&self, extension: Arc<dyn HostRequestHandler>) {
        *write_lock(&self.extension) = Some(extension);
    }

    fn extension(&self) -> Option<Arc<dyn HostRequestHandler>> {
        read_lock(&self.extension).clone()
    }

    pub fn take_diffs(&self, origin: &str) -> Vec<CellDiff> {
        lock(&self.diffs).remove(origin).unwrap_or_default()
    }

    fn record_diff(&self, origin: Option<&str>, path: String, diff: String) {
        let Some(origin) = origin else { return };
        lock(&self.diffs)
            .entry(origin.to_string())
            .or_default()
            .push(CellDiff { path, diff });
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

fn origin(payload: &Value) -> Option<&str> {
    payload.get("origin").and_then(Value::as_str)
}

async fn successful_text(tool: &dyn AgentTool, args: Value) -> Result<String, HostError> {
    let result = tool.execute(args).await;
    let text = extract_text(&result);
    if result.outcome.is_success() {
        Ok(text)
    } else {
        Err(HostError(text))
    }
}

impl MushHost {
    async fn start_bash(&self, payload: &Value) -> Result<Value, HostError> {
        let command = payload
            .get("command")
            .and_then(Value::as_str)
            .ok_or("bash needs a command")?;
        let timeout = match payload.get("timeout") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_f64()
                    .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                    .map(Duration::from_secs_f64)
                    .ok_or("timeout must be a positive number of seconds")?,
            ),
        };
        let cwd = payload
            .get("cwd")
            .and_then(Value::as_str)
            .map_or_else(|| self.cwd.to_path_buf(), std::path::PathBuf::from);
        let env: Option<Vec<(String, String)>> =
            payload.get("env").and_then(Value::as_object).map(|env| {
                env.iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                    .collect()
            });
        let (job_id, pid) = self
            .jobs
            .start(&cwd, command, timeout, origin(payload), env.as_deref())
            .await
            .map_err(HostError)?;
        Ok(json!({ "job_id": job_id, "pid": pid }))
    }

    async fn search_web(&self, payload: Value) -> Result<Value, HostError> {
        let tool = read_lock(&self.web_search)
            .clone()
            .ok_or_else(|| HostError("web search is not available here".into()))?;
        let text = successful_text(tool.as_ref(), payload).await?;
        Ok(json!({ "text": text }))
    }

    async fn apply_edit(&self, payload: &Value) -> Result<Value, HostError> {
        let sandbox = read_lock(&self.sandbox).clone();
        if let Some(sandbox) = sandbox {
            let path = payload
                .get("path")
                .and_then(Value::as_str)
                .ok_or("edit needs a path")?;
            let old = payload
                .get("old")
                .and_then(Value::as_str)
                .ok_or("edit needs old text")?;
            let new = payload
                .get("new")
                .and_then(Value::as_str)
                .ok_or("edit needs new text")?;
            let allow_marker = payload
                .get(crate::redaction_guard::ALLOW_REDACTION_MARKER_KEY)
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !allow_marker && crate::redaction_guard::introduced_marker(old, new) {
                return Err(HostError("edit introduces a redaction marker; set allow_redaction_marker only when literal marker text is intended".into()));
            }
            let target = self.cwd.join(path);
            let old = old.to_string();
            let new = new.to_string();
            tokio::task::spawn_blocking(move || sandbox.edit_file(&target, &old, &new))
                .await
                .map_err(|error| HostError(error.to_string()))?
                .map_err(|error| HostError(error.to_string()))?;
            self.record_diff(
                origin(payload),
                path.to_string(),
                format!(
                    "-{old_text}\n+{new_text}",
                    old_text = payload["old"].as_str().unwrap_or_default(),
                    new_text = payload["new"].as_str().unwrap_or_default()
                ),
            );
            return Ok(json!({"summary": format!("edited {path}")}));
        }
        let args = json!({
            "path": payload.get("path").cloned().unwrap_or(Value::Null),
            "oldText": payload.get("old").cloned().unwrap_or(Value::Null),
            "newText": payload.get("new").cloned().unwrap_or(Value::Null),
            crate::redaction_guard::ALLOW_REDACTION_MARKER_KEY: payload
                .get(crate::redaction_guard::ALLOW_REDACTION_MARKER_KEY)
                .cloned()
                .unwrap_or(Value::Bool(false)),
        });
        let text = successful_text(self.edit.as_ref(), args).await?;
        let (summary, diff) = text.split_once('\n').unwrap_or((text.as_str(), ""));
        let path = payload
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.record_diff(origin(payload), path, diff.to_string());
        Ok(json!({ "summary": summary }))
    }
}

#[async_trait::async_trait]
impl HostRequestHandler for MushHost {
    fn configure_sandbox(&self, sandbox: Arc<mush_sandbox::Sandbox>) {
        self.set_sandbox(Some(sandbox));
    }

    fn sandboxed_edit(&self) -> bool {
        read_lock(&self.sandbox).is_some()
    }

    fn cancellable(&self, kind: &str) -> bool {
        matches!(
            kind,
            "bash.wait" | "rlm.collect" | "decisions.wait" | "websearch"
        )
    }

    async fn handle(&self, kind: &str, payload: Value) -> Result<Value, HostError> {
        if let Some(sandbox) = read_lock(&self.sandbox).as_ref()
            && !sandbox.allows_host_request(kind)
        {
            return Err(HostError(format!("sandbox denied host request {kind}")));
        }
        match kind {
            "bash.start" => self.start_bash(&payload).await,
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
            "bash.running" => Ok(json!({
                "running": self
                    .jobs
                    .running(job_id(&payload)?)
                    .await
                    .map_err(HostError)?,
            })),
            "bash.cancel" => {
                self.jobs
                    .cancel(job_id(&payload)?)
                    .await
                    .map_err(HostError)?;
                Ok(json!({}))
            }
            "bash.kill" => {
                self.jobs.kill(job_id(&payload)?).await.map_err(HostError)?;
                Ok(json!({}))
            }
            "websearch" => self.search_web(payload).await,
            "edit" => self.apply_edit(&payload).await,
            other => match self.extension() {
                Some(extension) => {
                    let reply = extension.handle(other, payload.clone()).await?;
                    if other == "agent_message.send" {
                        self.record_sent(origin(&payload), &payload, &reply);
                    }
                    Ok(reply)
                }
                None => Err(HostError(format!("unknown host request {other}"))),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusyKernelChoice {
    Wait,
    Kill,
    Cancel,
}

pub type BusyKernelPrompt = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = BusyKernelChoice> + Send>>
        + Send
        + Sync,
>;

pub const KERNEL_RESTART_NOTICE: &str = "<ipython_kernel_reset>\nThe Python kernel was restarted \
after an interrupted cell kept running. Variables, imports, tasks and open resources from before \
the restart are gone; recreate them before using them.\n</ipython_kernel_reset>";

pub const KERNEL_REPAIR_NOTICE: &str = "<ipython_kernel_reset>\nThe Python kernel broke its \
protocol, so this cell was stopped and the kernel was restarted. Its state was reloaded from the \
last snapshot where one existed; anything created since then is gone.\n</ipython_kernel_reset>";

struct CellRun {
    output: CellOutput,
    kernel_restarted: bool,
}

pub struct IpythonTool {
    config: KernelConfig,
    session: Mutex<Option<mush_kernel::KernelSession>>,
    host: Arc<dyn HostRequestHandler>,
    mush_host: Option<Arc<MushHost>>,
    kernel: Arc<tokio::sync::Mutex<Option<Kernel>>>,
    output_sink: Option<crate::bash::OutputSink>,
    busy_prompt: Option<BusyKernelPrompt>,
    repair_notice: Mutex<Option<String>>,
    snapshot_generation: Arc<AtomicU64>,
    snapshot_preempt: Arc<tokio::sync::Notify>,
    snapshot_debounce: Duration,
    model: Mutex<Option<(String, bool)>>,
    live_pid: Mutex<Option<u32>>,
    released: AtomicBool,
    waker: Mutex<Option<mush_kernel::Waker>>,
    waiting: Mutex<Option<mush_kernel::Waiting>>,
}

const SNAPSHOT_DEBOUNCE: Duration = Duration::from_millis(1500);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

const LIVE_OUTPUT_INTERVAL: Duration = Duration::from_millis(100);
const LIVE_OUTPUT_CAP_BYTES: usize = 2 * mush_kernel::DEFAULT_MAX_OUTPUT_CHARS;

impl IpythonTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self::with_mush_host(MushHost::new(cwd.clone()), cwd)
    }

    pub fn with_mush_host(host: MushHost, cwd: Arc<Path>) -> Self {
        let host = Arc::new(host);
        let mut tool = Self::with_host(cwd, host.clone());
        tool.mush_host = Some(host);
        tool
    }

    pub fn with_host(cwd: Arc<Path>, host: Arc<dyn HostRequestHandler>) -> Self {
        Self {
            config: KernelConfig {
                cwd: cwd.to_path_buf(),
                ..KernelConfig::default()
            },
            session: Mutex::new(None),
            host,
            mush_host: None,
            kernel: Arc::new(tokio::sync::Mutex::new(None)),
            output_sink: None,
            busy_prompt: None,
            repair_notice: Mutex::new(None),
            snapshot_generation: Arc::new(AtomicU64::new(0)),
            snapshot_preempt: Arc::new(tokio::sync::Notify::new()),
            snapshot_debounce: SNAPSHOT_DEBOUNCE,
            model: Mutex::new(None),
            live_pid: Mutex::new(None),
            released: AtomicBool::new(false),
            waker: Mutex::new(None),
            waiting: Mutex::new(None),
        }
    }

    fn schedule_snapshot(&self) {
        let Some(path) = lock(&self.session)
            .as_ref()
            .map(|session| session.state_path.clone())
        else {
            return;
        };
        let generation = self.snapshot_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let current = self.snapshot_generation.clone();
        let preempt = self.snapshot_preempt.clone();
        let kernel = self.kernel.clone();
        let debounce = self.snapshot_debounce;
        tokio::spawn(async move {
            tokio::time::sleep(debounce).await;
            let still_latest = || current.load(Ordering::SeqCst) == generation;
            let preempted = preempt.notified();
            tokio::pin!(preempted);
            preempted.as_mut().enable();
            if !still_latest() {
                return;
            }
            let slot = kernel.lock().await;
            let Some(kernel) = slot.as_ref() else { return };
            if !still_latest() || kernel.busy() {
                return;
            }
            if let Some(parent) = path.parent()
                && let Err(error) = std::fs::create_dir_all(parent)
            {
                tracing::warn!(%error, "kernel state directory unavailable");
                return;
            }
            tokio::select! {
                biased;
                () = &mut preempted => {
                    tracing::debug!("a cell started, so the kernel state auto-save stopped");
                }
                outcome = kernel.snapshot(&path, None) => match outcome {
                    Ok(report) => tracing::debug!(
                        saved = report.saved.len(),
                        skipped = report.skipped.len(),
                        "kernel state auto-saved"
                    ),
                    Err(error) => tracing::warn!(%error, "kernel state auto-save failed"),
                },
            }
        });
    }

    fn preempt_snapshot(&self) {
        self.snapshot_generation.fetch_add(1, Ordering::SeqCst);
        self.snapshot_preempt.notify_waiters();
    }

    #[must_use]
    pub fn with_output_sink(mut self, sink: crate::bash::OutputSink) -> Self {
        self.output_sink = Some(crate::bash::redacting_sink(sink));
        self
    }

    #[must_use]
    pub fn with_busy_prompt(mut self, prompt: BusyKernelPrompt) -> Self {
        self.busy_prompt = Some(prompt);
        self
    }

    pub fn with_sandbox(mut self, sandbox: Arc<mush_sandbox::Sandbox>) -> Result<Self, String> {
        self.set_sandbox(Some(sandbox))?;
        Ok(self)
    }

    pub fn set_sandbox(
        &mut self,
        sandbox: Option<Arc<mush_sandbox::Sandbox>>,
    ) -> Result<(), String> {
        if self
            .kernel
            .try_lock()
            .map_err(|_| "kernel is running".to_string())?
            .is_some()
            || self
                .mush_host
                .as_ref()
                .is_some_and(|host| host.jobs().any_running())
        {
            return Err("sandbox cannot be changed while the kernel or jobs are running".into());
        }
        let sandbox = match sandbox {
            Some(sandbox) => {
                let plan = mush_kernel::PythonPlan::resolve(
                    self.config.python.as_deref(),
                    self.config.managed_venv.as_ref(),
                )
                .map_err(|error| error.to_string())?;
                let python = plan.python_if_present().ok_or("initialize the managed Python environment in a trusted session before sandbox launch")?;
                let mut paths = mush_kernel::python::runtime_read_paths(&python)
                    .map_err(|error| error.to_string())?;
                if let mush_kernel::PythonPlan::Managed(venv) = &plan {
                    paths.push(venv.dir.clone());
                    paths.extend(venv.skills.iter().map(|skill| skill.dir.clone()));
                }
                Some(Arc::new(
                    sandbox
                        .with_paths(&paths, &[])
                        .map_err(|error| error.to_string())?,
                ))
            }
            None => None,
        };
        if let Some(host) = &self.mush_host {
            host.set_sandbox(sandbox.clone());
        }
        self.config.sandbox = sandbox;
        Ok(())
    }

    pub fn kernel_config_mut(&mut self) -> &mut KernelConfig {
        &mut self.config
    }

    #[must_use]
    pub fn mush_host(&self) -> Option<&Arc<MushHost>> {
        self.mush_host.as_ref()
    }

    fn kernel_config(&self) -> KernelConfig {
        let mut config = self.config.clone();
        if let Some(session) = lock(&self.session).as_ref() {
            config.env.extend(session.env());
        }
        config
    }

    fn journal_kernel(&self, pid: Option<u32>, active: bool) {
        {
            let mut live = lock(&self.live_pid);
            if active {
                *live = pid;
            } else if pid.is_none() || *live == pid {
                *live = None;
            }
        }
        if let Some(host) = &self.mush_host {
            host.jobs().journal(pid, active);
        }
    }

    async fn retire(&self, kernel: Kernel) {
        let pid = kernel.pid();
        *lock(&self.waker) = None;
        let _ = kernel.shutdown().await;
        self.journal_kernel(pid, false);
    }

    pub async fn shutdown(&self) {
        if let Some(kernel) = self.kernel.lock().await.take() {
            self.retire(kernel).await;
        }
    }

    async fn run(&self, code: &str) -> Result<CellRun, KernelError> {
        let mut kernel_restarted = false;
        loop {
            match self.run_once(code).await {
                Err(KernelError::Busy) => {}
                Err(error) => return Err(error),
                Ok(output) => {
                    return Ok(CellRun {
                        output,
                        kernel_restarted,
                    });
                }
            }
            let choice = match &self.busy_prompt {
                Some(prompt) => prompt().await,
                None => BusyKernelChoice::Cancel,
            };
            match choice {
                BusyKernelChoice::Wait => {}
                BusyKernelChoice::Kill => {
                    if let Some(kernel) = self.kernel.lock().await.take() {
                        let pid = kernel.pid();
                        kernel.kill().await;
                        self.journal_kernel(pid, false);
                    }
                    kernel_restarted = true;
                }
                BusyKernelChoice::Cancel => return Err(KernelError::Busy),
            }
        }
    }

    async fn started<'a>(
        &self,
        slot: &'a mut Option<Kernel>,
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<&'a Kernel, KernelError> {
        if slot.is_none() {
            let progress = |message: &str| {
                tracing::info!("{message}");
                progress(message);
            };
            let kernel =
                Kernel::start_with_progress(self.kernel_config(), self.host.clone(), &progress)
                    .await?;
            self.journal_kernel(kernel.pid(), true);
            *lock(&self.waker) = Some(kernel.waker());
            if self.released.swap(false, Ordering::SeqCst)
                && let Some(path) = self.latest_snapshot()
            {
                let outcome = kernel
                    .restore(&path)
                    .await
                    .map_err(|error| error.to_string());
                *lock(&self.repair_notice) = Some(mush_kernel::restore_notice_text(&outcome));
            }
            *slot = Some(kernel);
        }
        slot.as_ref().ok_or(KernelError::Pipes)
    }

    async fn run_once(&self, code: &str) -> Result<CellOutput, KernelError> {
        self.preempt_snapshot();
        let mut slot = self.kernel.lock().await;
        let sink = self.output_sink.clone();
        let exec_id = mush_agent::exec_context::current_exec_id();
        if slot
            .as_ref()
            .is_some_and(|kernel| kernel.protocol_failure().is_some())
        {
            self.repair(&mut slot).await;
        }
        let kernel = self
            .started(&mut slot, &|message| {
                if let Some(sink) = &sink {
                    sink(&exec_id, message);
                }
            })
            .await?;
        let mut live = String::new();
        let mut last_emit = std::time::Instant::now() - LIVE_OUTPUT_INTERVAL;
        let mut pending = false;
        let origin = (!exec_id.is_empty()).then_some(exec_id.as_str());
        let outcome = kernel
            .execute_from(code, origin, |event| {
                if let mush_kernel::Event::Wait { on, .. } = event {
                    *lock(&self.waiting) = on.clone().map(|on| mush_kernel::Waiting {
                        on,
                        since_ms: now_ms(),
                    });
                    return;
                }
                let Some(sink) = &sink else { return };
                let text = match event {
                    mush_kernel::Event::Stdout { text, .. }
                    | mush_kernel::Event::Stderr { text, .. } => text,
                    _ => return,
                };
                if live.len() >= LIVE_OUTPUT_CAP_BYTES {
                    return;
                }
                live.push_str(text);
                live.truncate(live.floor_char_boundary(LIVE_OUTPUT_CAP_BYTES));
                pending = true;
                if last_emit.elapsed() >= LIVE_OUTPUT_INTERVAL {
                    sink(&exec_id, live.trim_end());
                    last_emit = std::time::Instant::now();
                    pending = false;
                }
            })
            .await;
        *lock(&self.waiting) = None;
        if pending && let Some(sink) = &sink {
            sink(&exec_id, live.trim_end());
        }
        match &outcome {
            Err(KernelError::Exited(_)) => {
                if let Some(kernel) = slot.take() {
                    self.journal_kernel(kernel.pid(), false);
                    kernel.kill().await;
                }
            }
            Err(KernelError::Corrupted(_)) => self.repair(&mut slot).await,
            _ => {}
        }
        outcome
    }

    async fn repair(&self, slot: &mut Option<Kernel>) {
        if let Some(kernel) = slot.take() {
            self.journal_kernel(kernel.pid(), false);
            kernel.kill().await;
        }
        let kernel = match Kernel::start(self.kernel_config(), self.host.clone()).await {
            Ok(kernel) => kernel,
            Err(error) => {
                tracing::warn!(%error, "kernel protocol repair could not start a kernel");
                return;
            }
        };
        self.journal_kernel(kernel.pid(), true);
        *lock(&self.waker) = Some(kernel.waker());
        if let Some(path) = self.latest_snapshot() {
            let outcome = kernel
                .restore(&path)
                .await
                .map_err(|error| error.to_string());
            *lock(&self.repair_notice) = Some(mush_kernel::restore_notice_text(&outcome));
            if kernel.protocol_failure().is_some() {
                tracing::warn!("kernel broke its protocol again while restoring, starting fresh");
                self.journal_kernel(kernel.pid(), false);
                kernel.kill().await;
                return;
            }
        }
        *slot = Some(kernel);
    }

    fn vision_refusal(&self) -> Option<String> {
        match &*lock(&self.model) {
            Some((id, false)) => Some(format!(
                "\n[image not attached: {id} does not support vision. Tell the user to switch to a \
                 vision-capable model to load images into context.]"
            )),
            Some((_, true)) | None => None,
        }
    }

    fn latest_snapshot(&self) -> Option<std::path::PathBuf> {
        lock(&self.session)
            .as_ref()
            .map(|session| session.state_path.clone())
            .filter(|path| path.is_file())
    }
}

#[async_trait::async_trait]
impl mush_kernel::KernelState for IpythonTool {
    async fn snapshot(&self, path: &Path) -> Result<Option<mush_kernel::SnapshotReport>, String> {
        let slot = self.kernel.lock().await;
        let Some(kernel) = slot.as_ref() else {
            return Ok(None);
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        kernel
            .snapshot(path, None)
            .await
            .map(Some)
            .map_err(|e| e.to_string())
    }

    async fn restore_from(
        &self,
        path: &Path,
    ) -> Option<Result<mush_kernel::RestoreReport, String>> {
        if !path.is_file() {
            return None;
        }
        let mut slot = self.kernel.lock().await;
        self.released.store(false, Ordering::SeqCst);
        let outcome = match self.started(&mut slot, &|_| {}).await {
            Ok(kernel) => kernel.restore(path).await,
            Err(error) => Err(error),
        };
        match &outcome {
            Ok(report) => tracing::info!(
                restored = report.restored.len(),
                failed = report.failed.len(),
                "kernel state restored"
            ),
            Err(error) => tracing::warn!(%error, "kernel state restore failed"),
        }
        Some(outcome.map_err(|error| error.to_string()))
    }

    async fn live_names(&self) -> Option<Result<Vec<String>, String>> {
        let slot = self.kernel.lock().await;
        let kernel = slot.as_ref()?;
        Some(kernel.list_names().await.map_err(|error| error.to_string()))
    }

    async fn reset(&self) {
        self.snapshot_generation.fetch_add(1, Ordering::SeqCst);
        self.released.store(false, Ordering::SeqCst);
        if let Some(kernel) = self.kernel.lock().await.take() {
            self.retire(kernel).await;
        }
    }

    fn attach_session(&self, session: mush_kernel::KernelSession) {
        self.snapshot_generation.fetch_add(1, Ordering::SeqCst);
        self.released.store(false, Ordering::SeqCst);
        *lock(&self.session) = Some(session);
    }

    fn set_model(&self, id: &str, accepts_images: bool) {
        *lock(&self.model) = Some((id.to_string(), accepts_images));
    }

    fn background_work(&self) -> bool {
        self.mush_host
            .as_ref()
            .is_some_and(|host| host.jobs().any_running())
    }

    fn pid(&self) -> Option<u32> {
        *lock(&self.live_pid)
    }

    fn waiting(&self) -> Option<mush_kernel::Waiting> {
        lock(&self.waiting).clone()
    }

    fn wake_waits(&self) {
        let Some(waker) = lock(&self.waker).clone() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = waker.wake().await {
                    tracing::debug!(%error, "could not wake the kernel's waits");
                }
            });
        }
    }

    async fn release(&self) -> mush_kernel::Release {
        use mush_kernel::Release;
        let Some(path) = lock(&self.session)
            .as_ref()
            .map(|session| session.state_path.clone())
        else {
            return Release::Failed("the kernel has no session to save its state in".into());
        };
        let Ok(mut slot) = self.kernel.try_lock() else {
            return Release::Busy;
        };
        let Some(kernel) = slot.as_ref() else {
            return Release::NotRunning;
        };
        let has_children = || {
            kernel
                .pid()
                .is_some_and(|pid| mush_kernel::process::has_children(pid) != Some(false))
        };
        if kernel.busy() || self.background_work() || has_children() {
            return Release::Busy;
        }
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return Release::Failed(error.to_string());
        }
        let report = match kernel.snapshot(&path, None).await {
            Ok(report) => report,
            Err(error) => return Release::Failed(error.to_string()),
        };
        if report.background_tasks > 0 || self.background_work() {
            return Release::Busy;
        }
        self.snapshot_generation.fetch_add(1, Ordering::SeqCst);
        self.released.store(true, Ordering::SeqCst);
        let Some(kernel) = slot.take() else {
            return Release::NotRunning;
        };
        drop(slot);
        self.retire(kernel).await;
        Release::Released(report)
    }
}

fn attached_images(
    attachments: &[mush_kernel::Attachment],
    text: &mut String,
) -> Vec<mush_ai::types::ToolResultContentPart> {
    use base64::Engine;
    use mush_ai::types::{ImageContent, ImageMimeType, ToolResultContentPart};
    let mut parts = Vec::new();
    for attachment in attachments {
        let Some(mime_type) = ImageMimeType::from_mime_type(&attachment.mime_type) else {
            continue;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&attachment.data) else {
            continue;
        };
        let name = attachment.path.as_deref().unwrap_or("attachment");
        match crate::attachment::shrink(&bytes) {
            Ok(crate::attachment::Shrunk::Jpeg { bytes, note }) => {
                text.push_str(&format!("\n[image {name}: {note}]"));
                parts.push(ToolResultContentPart::Image(ImageContent {
                    data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    mime_type: ImageMimeType::Jpeg,
                }));
            }
            Ok(crate::attachment::Shrunk::Unchanged) => {
                parts.push(ToolResultContentPart::Image(ImageContent {
                    data: attachment.data.clone(),
                    mime_type,
                }))
            }
            Err(error) => text.push_str(&format!("\n[image {name} dropped: {error}]")),
        }
    }
    parts
}

#[must_use]
pub fn format_cell_output(output: &CellOutput) -> String {
    fn start_line(text: &mut String) {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
    }

    let mut text = output.stdout.clone();
    if !output.stderr.is_empty() {
        start_line(&mut text);
        text.push_str("[stderr]\n");
        text.push_str(&output.stderr);
    }
    if let Some(result) = &output.result {
        start_line(&mut text);
        text.push_str(result);
    }
    if let Some(error) = &output.error {
        start_line(&mut text);
        if error.traceback.is_empty() {
            text.push_str(&format!("{}: {}", error.ename, error.evalue));
        } else {
            text.push_str(&error.traceback.join("\n"));
        }
    }
    if !output.background.is_empty() {
        start_line(&mut text);
        text.push_str("[background output (unattributed)]\n");
        text.push_str(&output.background);
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
        let exec_id = mush_agent::exec_context::current_exec_id();
        match self.run(&args.code).await {
            Ok(run) => {
                let mut text = format_cell_output(&run.output);
                if run.kernel_restarted {
                    text = format!("{KERNEL_RESTART_NOTICE}\n\n{text}");
                }
                if let Some(notice) = lock(&self.repair_notice).take() {
                    text = format!("{notice}\n\n{text}");
                }
                if run.output.ok {
                    self.schedule_snapshot();
                }
                let (diffs, sent_messages) = match &self.mush_host {
                    Some(host) => (host.take_diffs(&exec_id), host.take_sent_messages(&exec_id)),
                    None => (Vec::new(), Vec::new()),
                };
                let refusal = (!run.output.attachments.is_empty())
                    .then(|| self.vision_refusal())
                    .flatten();
                let images = match &refusal {
                    Some(refusal) => {
                        text.push_str(refusal);
                        Vec::new()
                    }
                    None => attached_images(&run.output.attachments, &mut text),
                };
                let output = run.output;
                let details = CellDetails {
                    duration_ms: u64::try_from(output.duration.as_millis()).unwrap_or(u64::MAX),
                    stdout: output.stdout,
                    stderr: output.stderr,
                    result: output.result,
                    error: output.error.map(|error| CellErrorDetails {
                        ename: error.ename,
                        evalue: error.evalue,
                        traceback: error.traceback,
                    }),
                    diffs,
                    images: images.len(),
                    kernel_restarted: run.kernel_restarted,
                    sent_messages,
                    background: output.background,
                };
                let mut result = if output.ok && refusal.is_none() {
                    ToolResult::text(text)
                } else {
                    ToolResult::error(text)
                };
                result.content.extend(images);
                result.with_details(details.to_value())
            }
            Err(error @ KernelError::Corrupted(_)) => {
                ToolResult::error(format!("kernel: {error}\n\n{KERNEL_REPAIR_NOTICE}"))
            }
            Err(error) => ToolResult::error(format!("kernel: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;
    use mush_kernel::CellError;

    fn tool(dir: &Path) -> IpythonTool {
        IpythonTool::new(dir.into())
    }

    #[test]
    fn sent_messages_read_single_and_broadcast_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let host = MushHost::new(dir.path().into());
        let payload = json!({ "message": "check the tests" });
        host.record_sent(
            Some("cell"),
            &payload,
            &json!({ "deliveryStatus": "queued", "target": { "sessionName": "auth" } }),
        );
        host.record_sent(
            Some("cell"),
            &payload,
            &json!({ "receipts": [
                { "deliveryStatus": "delivered", "target": { "sessionName": "root" } },
                { "error": "gone", "target": { "sessionName": "beta" } },
            ] }),
        );
        let sent = host.take_sent_messages("cell");
        let rows: Vec<_> = sent
            .iter()
            .map(|sent| (sent.receiver.as_str(), sent.status.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("auth", "queued"),
                ("root", "delivered"),
                ("beta", "failed")
            ]
        );
        assert!(sent.iter().all(|sent| sent.text == "check the tests"));
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
                "code": "import asyncio\nfor _ in range(500):\n    if await h.tail(1):\n        break\n    await asyncio.sleep(0.02)\n(await h.running(), await h.tail(1), h.pid is not None)"
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
        let notices = Arc::new(std::sync::Mutex::new(
            Vec::<crate::kernel_jobs::JobNotice>::new(),
        ));
        host.jobs().set_notice_sink({
            let notices = notices.clone();
            Arc::new(move |notice| notices.lock().unwrap().push(notice))
        });
        let tool = IpythonTool::with_mush_host(host, dir.path().into());

        let result = mush_agent::exec_context::TOOL_EXEC_ID
            .scope(
                "call_9".into(),
                tool.execute(
                    json!({ "code": "h = bash('exit 5')\nawait asyncio.sleep(0.3)\n'left running'" }),
                ),
            )
            .await;

        assert!(result.outcome.is_success(), "{}", extract_text(&result));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while notices.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let notices = notices.lock().unwrap().clone();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].done.command, "exit 5");
        assert_eq!(notices[0].done.exit_code, 5);
        assert_eq!(notices[0].origin.as_deref(), Some("call_9"));
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn bash_takes_a_fractional_timeout_reports_a_duration_and_refuses_an_empty_command() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let result = tool
            .execute(json!({
                "code": "r = await bash('sleep 5', timeout=0.3)\nq = await bash('sleep 0.2')\ntry:\n    bash('')\n    empty = 'ran'\nexcept TypeError:\n    empty = 'refused'\n(r.timed_out, r.duration < 3, q.duration >= 0.2, empty)"
            }))
            .await;

        assert_eq!(
            extract_text(&result),
            "(True, True, True, 'refused')",
            "{}",
            extract_text(&result)
        );
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn bash_and_edit_follow_the_kernel_cwd_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), "old\n").unwrap();
        let tool = tool(dir.path());

        let result = tool
            .execute(json!({
                "code": "import os\nfor key in [k for k in os.environ if k.startswith('DIRENV_')]:\n    del os.environ[key]\nos.chdir('sub')\nos.environ['MUSH_TEST_VAR'] = 'yes'\nos.environ['PATH'] = '/mush-test-bin:' + os.environ['PATH']\nawait edit('a.txt', 'old', 'new')\nr = await bash('basename \"$PWD\"; echo $MUSH_TEST_VAR; echo \"$PATH\" | grep -c mush-test-bin; cat a.txt')\nr.output"
            }))
            .await;

        assert_eq!(
            extract_text(&result),
            "'sub\\nyes\\n1\\nnew\\n'",
            "{}",
            extract_text(&result)
        );
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn interrupting_an_awaited_bash_kills_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let tool = Arc::new(tool(dir.path()));
        let cell = {
            let tool = tool.clone();
            let code = format!(
                "await bash('echo $$ > {}; exec sleep 30')",
                pidfile.display()
            );
            tokio::spawn(async move { tool.execute(json!({ "code": code })).await })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !pidfile.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        cell.abort();
        let _ = cell.await;

        let next = tool.execute(json!({ "code": "1 + 1" })).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while unsafe { libc::kill(pid, 0) } == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        assert_eq!(extract_text(&next), "2");
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn a_wait_cancelled_before_the_job_starts_leaves_the_handle_usable() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let result = tool
            .execute(json!({
                "code": "h = bash('exit 4')\nwaiting = asyncio.ensure_future(h.running())\nawait asyncio.sleep(0)\nwaiting.cancel()\n(await h).exit_code"
            }))
            .await;

        assert_eq!(extract_text(&result), "4", "{}", extract_text(&result));
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn a_background_handle_outlives_a_cancelled_wait_and_still_sends_its_notice() {
        let dir = tempfile::tempdir().unwrap();
        let host = MushHost::new(dir.path().into());
        let notices = Arc::new(std::sync::Mutex::new(
            Vec::<crate::kernel_jobs::JobNotice>::new(),
        ));
        host.jobs().set_notice_sink({
            let notices = notices.clone();
            Arc::new(move |notice| notices.lock().unwrap().push(notice))
        });
        let tool = IpythonTool::with_mush_host(host, dir.path().into());

        let release = dir.path().join("release");
        let code = format!(
            "h = bash('while [ ! -e {} ]; do sleep 0.02; done; exit 3')\nh.pid\ntry:\n    await asyncio.wait_for(h, 0.1)\nexcept TimeoutError:\n    pass\nawait h.running()",
            release.display()
        );
        let result = tool.execute(json!({ "code": code })).await;
        std::fs::write(&release, "").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while notices.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        assert_eq!(extract_text(&result), "True");
        let notices = notices.lock().unwrap().clone();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].done.exit_code, 3);
        tool.shutdown().await;
    }

    const STUBBORN: &str = "import time\nwhile True:\n    try:\n        time.sleep(0.05)\n    except KeyboardInterrupt:\n        pass";

    async fn leave_a_stubborn_cell_running(tool: &Arc<IpythonTool>, dir: &Path) {
        let started = dir.join("stubborn-started");
        let code = format!(
            "import pathlib\npathlib.Path({:?}).touch()\n{STUBBORN}",
            started.display().to_string()
        );
        let stubborn = {
            let tool = tool.clone();
            tokio::spawn(async move { tool.execute(json!({ "code": code })).await })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !started.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        stubborn.abort();
        let _ = stubborn.await;
    }

    #[tokio::test]
    async fn a_busy_kernel_with_no_prompt_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut tool = tool(dir.path());
        tool.kernel_config_mut().busy_wait = std::time::Duration::from_millis(400);
        let tool = Arc::new(tool);
        leave_a_stubborn_cell_running(&tool, dir.path()).await;

        let result = tool.execute(json!({ "code": "1 + 1" })).await;

        assert!(result.outcome.is_error());
        assert_eq!(
            extract_text(&result),
            "kernel: an interrupted cell is still running"
        );
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn choosing_kill_restarts_the_kernel_and_tells_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let mut tool = tool(dir.path());
        tool.kernel_config_mut().busy_wait = std::time::Duration::from_millis(400);
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tool = Arc::new(tool.with_busy_prompt({
            let asked = asked.clone();
            Arc::new(move || {
                asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Box::pin(async { BusyKernelChoice::Kill })
            })
        }));
        leave_a_stubborn_cell_running(&tool, dir.path()).await;

        let result = tool.execute(json!({ "code": "1 + 1" })).await;

        assert!(result.outcome.is_success(), "{}", extract_text(&result));
        assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(
            extract_text(&result),
            format!("{KERNEL_RESTART_NOTICE}\n\n2")
        );
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn edit_runs_through_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let tool = tool(dir.path());

        let result = mush_agent::exec_context::TOOL_EXEC_ID
            .scope(
                "call_3".into(),
                tool.execute(json!({
                    "code": format!("await edit({:?}, 'world', 'there')", path.display().to_string())
                })),
            )
            .await;

        assert!(result.outcome.is_success(), "{}", extract_text(&result));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello there\n");
        assert_eq!(
            extract_text(&result),
            format!("'edited {}'", path.display())
        );
        let details = CellDetails::from_value(result.details.as_ref().unwrap()).unwrap();
        assert_eq!(details.diffs.len(), 1);
        assert_eq!(details.diffs[0].path, path.display().to_string());
        assert!(
            details.diffs[0].diff.contains("there"),
            "{}",
            details.diffs[0].diff
        );
        assert!(details.error.is_none());
        assert!(details.duration_ms > 0);
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn edit_takes_old_str_and_new_str_keywords() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let tool = tool(dir.path());

        let result = tool
            .execute(json!({
                "code": format!(
                    "await edit(path={:?}, old_str='world', new_str='there')",
                    path.display().to_string()
                )
            }))
            .await;

        assert!(result.outcome.is_success(), "{}", extract_text(&result));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello there\n");
        tool.shutdown().await;
    }

    struct CannedSearch;

    #[async_trait::async_trait]
    impl AgentTool for CannedSearch {
        fn name(&self) -> &str {
            "web_search"
        }
        fn label(&self) -> &str {
            "Web Search"
        }
        fn description(&self) -> &str {
            "canned"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object" })
        }
        fn output_limit(&self) -> mush_agent::tool::OutputLimit {
            mush_agent::tool::OutputLimit::Head
        }
        async fn execute(&self, args: Value) -> ToolResult {
            ToolResult::text(format!(
                "results for {} ({})",
                args["query"].as_str().unwrap_or_default(),
                args["num_results"]
            ))
        }
    }

    #[tokio::test]
    async fn websearch_runs_the_hosts_search_tool() {
        let dir = tempfile::tempdir().unwrap();
        let host = MushHost::new(dir.path().into());
        host.set_web_search(Arc::new(CannedSearch));
        let tool = IpythonTool::with_mush_host(host, dir.path().into());

        let result = tool
            .execute(json!({ "code": "print(await websearch('rust nightly', num_results=3))" }))
            .await;

        assert!(result.outcome.is_success(), "{}", extract_text(&result));
        assert_eq!(extract_text(&result).trim(), "results for rust nightly (3)");
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn edit_works_after_the_file_changes_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one two three\n").unwrap();
        let tool = tool(dir.path());
        let path_text = path.display().to_string();

        let result = tool
            .execute(json!({
                "code": format!(
                    "p = {path_text:?}\nawait edit(p, 'one', '1')\nimport pathlib, os\npathlib.Path(p).write_text(pathlib.Path(p).read_text().replace('two', '2'))\nos.utime(p, (4102444800, 4102444800))\nawait edit(p, 'three', '3')"
                )
            }))
            .await;

        assert!(result.outcome.is_success(), "{}", extract_text(&result));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1 2 3\n");
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn an_attached_image_reaches_the_result_as_an_image_part() {
        use mush_ai::types::ToolResultContentPart;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dot.png");
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[255, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        std::fs::write(&path, &png).unwrap();
        let tool = tool(dir.path());

        let result = tool
            .execute(json!({
                "code": format!("print(await attach_image({:?}))", path.display().to_string())
            }))
            .await;

        assert!(result.outcome.is_success(), "{}", extract_text(&result));
        assert!(
            extract_text(&result).starts_with(&format!("attached {}", path.display())),
            "{}",
            extract_text(&result)
        );
        let images: Vec<_> = result
            .content
            .iter()
            .filter_map(|part| match part {
                ToolResultContentPart::Image(image) => Some(image),
                _ => None,
            })
            .collect();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, mush_ai::types::ImageMimeType::Png);
        let details = CellDetails::from_value(result.details.as_ref().unwrap()).unwrap();
        assert_eq!(details.images, 1);
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn a_large_attached_image_is_shrunk_to_jpeg_and_noted() {
        use mush_ai::types::ToolResultContentPart;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2000,
            100,
            image::Rgb([0, 128, 255]),
        ))
        .save(&path)
        .unwrap();
        let tool = tool(dir.path());

        let result = tool
            .execute(json!({
                "code": format!("attach_image({:?})", path.display().to_string())
            }))
            .await;

        let text = extract_text(&result);
        assert!(result.outcome.is_success(), "{text}");
        assert!(
            text.contains(&format!(
                "[image {}: original 2000x100; attached 1200x60 JPEG at quality 82]",
                path.display()
            )),
            "{text}"
        );
        let images: Vec<_> = result
            .content
            .iter()
            .filter_map(|part| match part {
                ToolResultContentPart::Image(image) => Some(image),
                _ => None,
            })
            .collect();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, mush_ai::types::ImageMimeType::Jpeg);
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn attaching_an_image_on_a_model_without_vision_is_an_error() {
        use mush_kernel::KernelState;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dot.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4))
            .save(&path)
            .unwrap();
        let tool = tool(dir.path());
        tool.set_model("text-only", false);

        let result = tool
            .execute(json!({
                "code": format!("attach_image({:?})", path.display().to_string())
            }))
            .await;

        assert!(result.outcome.is_error());
        assert!(
            extract_text(&result).ends_with(
                "[image not attached: text-only does not support vision. Tell the user to switch \
                 to a vision-capable model to load images into context.]"
            ),
            "{}",
            extract_text(&result)
        );
        assert_eq!(result.content.len(), 1);
        tool.set_model("seeing", true);
        let seen = tool
            .execute(json!({
                "code": format!("attach_image({:?})", path.display().to_string())
            }))
            .await;
        assert!(seen.outcome.is_success(), "{}", extract_text(&seen));
        assert_eq!(seen.content.len(), 2);
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
    async fn a_kernel_edit_that_adds_a_redaction_marker_is_refused_unless_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.env");
        std::fs::write(&path, "TOKEN=abc\n").unwrap();
        let tool = tool(dir.path());
        let shown = path.display().to_string();

        let refused = tool
            .execute(json!({
                "code": format!("await edit({shown:?}, 'abc', '[REDACTED]')")
            }))
            .await;
        assert!(refused.outcome.is_error());
        assert!(
            extract_text(&refused).contains("allow_redaction_marker"),
            "{}",
            extract_text(&refused)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "TOKEN=abc\n");

        let allowed = tool
            .execute(json!({
                "code": format!("await edit({shown:?}, 'abc', '[REDACTED]', allow_redaction_marker=True)")
            }))
            .await;
        assert!(allowed.outcome.is_success(), "{}", extract_text(&allowed));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "TOKEN=[REDACTED]\n"
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
        assert!(seen[0].starts_with("tick"), "{seen:?}");
        assert!(!seen[0].contains("tick 1"), "{seen:?}");
        assert_eq!(seen.last().unwrap(), "tick 0\ntick 1\ntick 2", "{seen:?}");
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn state_round_trips_through_a_snapshot_and_a_reset() {
        use mush_kernel::KernelState;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifacts/kernel-state.pickle");
        let tool = tool(dir.path());
        let _ = tool.execute(json!({ "code": "carried = [1, 2, 3]" })).await;

        let report = tool.snapshot(&path).await.unwrap().unwrap();
        tool.reset().await;
        let gone = tool.execute(json!({ "code": "'carried' in dir()" })).await;
        let restored = tool.restore_from(&path).await;
        let back = tool.execute(json!({ "code": "sum(carried)" })).await;

        assert_eq!(report.saved, ["carried"]);
        assert_eq!(extract_text(&gone), "False");
        assert_eq!(
            restored,
            Some(Ok(mush_kernel::RestoreReport {
                restored: vec!["carried".into()],
                failed: Vec::new(),
            }))
        );
        assert_eq!(extract_text(&back), "6");
        tool.shutdown().await;
    }

    const BAD_FRAME: &str = "import os, sys\nshim = next(m for m in list(sys.modules.values()) if getattr(m, '__file__', '') and m.__file__.endswith('mush_kernel.py'))\nos.write(shim._proto_fd, b'{not json\\n')\nimport time\ntime.sleep(30)";

    #[tokio::test]
    async fn a_bad_protocol_frame_fails_the_cell_and_restarts_the_kernel_from_its_snapshot() {
        use mush_kernel::KernelState;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifacts/kernel-state.pickle");
        let tool = tool(dir.path());
        tool.attach_session(mush_kernel::KernelSession {
            session_dir: dir.path().join("artifacts"),
            state_path: path.clone(),
            depth: 0,
            max_depth: 1,
        });
        let _ = tool.execute(json!({ "code": "kept = 41" })).await;
        tool.snapshot(&path).await.unwrap().unwrap();
        let _ = tool.execute(json!({ "code": "lost = 1" })).await;

        let broken = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tool.execute(json!({ "code": BAD_FRAME })),
        )
        .await
        .expect("the bad frame fails the cell");
        let after = tool
            .execute(json!({ "code": "(kept + 1, 'lost' in dir())" }))
            .await;

        assert!(broken.outcome.is_error());
        let text = extract_text(&broken);
        assert!(
            text.starts_with("kernel: bad protocol frame: line is not a kernel event"),
            "{text}"
        );
        assert!(text.ends_with(KERNEL_REPAIR_NOTICE), "{text}");
        assert_eq!(
            extract_text(&after),
            format!(
                "[python-state-restored]\n\nYour Python kernel state was revived from your \
                 previous session. These names are available again: kept.\n{}\n\n(42, False)",
                mush_kernel::RESTART_RESETS
            )
        );
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn a_bad_protocol_frame_with_no_snapshot_restarts_the_kernel_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let _ = tool.execute(json!({ "code": "lost = 1" })).await;

        let broken = tool.execute(json!({ "code": BAD_FRAME })).await;
        let after = tool
            .execute(json!({ "code": "('lost' in dir(), callable(bash))" }))
            .await;

        assert!(broken.outcome.is_error());
        assert_eq!(extract_text(&after), "(False, True)");
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn a_snapshot_that_breaks_the_protocol_again_is_dropped_for_a_fresh_kernel() {
        use mush_kernel::KernelState;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifacts/kernel-state.pickle");
        let tool = tool(dir.path());
        tool.attach_session(mush_kernel::KernelSession {
            session_dir: dir.path().join("artifacts"),
            state_path: path.clone(),
            depth: 0,
            max_depth: 1,
        });
        let _ = tool
            .execute(json!({
                "code": "import os, sys\nshim = next(m for m in list(sys.modules.values()) if getattr(m, '__file__', '') and m.__file__.endswith('mush_kernel.py'))\nclass Bomb:\n    def __reduce__(self):\n        return (os.write, (shim._proto_fd, b'{not json\\n'))\nbomb = Bomb()\nkept = 41"
            }))
            .await;
        tool.snapshot(&path).await.unwrap().unwrap();

        let broken = tool.execute(json!({ "code": BAD_FRAME })).await;
        let after = tool.execute(json!({ "code": "'kept' in dir()" })).await;

        assert!(broken.outcome.is_error());
        assert!(after.outcome.is_success(), "{}", extract_text(&after));
        assert_eq!(
            extract_text(&after),
            format!(
                "[python-state-restored]\n\nYour previous Python kernel state could not be \
                 revived; the kernel is starting fresh, so re-create any variables, imports, or \
                 loaded data you need.\n{}\n\nFalse",
                mush_kernel::RESTART_RESETS
            )
        );
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn a_bad_frame_between_cells_is_repaired_before_the_next_cell() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let _ = tool
            .execute(json!({
                "code": "import os, sys, threading\nshim = next(m for m in list(sys.modules.values()) if getattr(m, '__file__', '') and m.__file__.endswith('mush_kernel.py'))\nlost = 1\nthreading.Timer(0.1, os.write, (shim._proto_fd, b'{not json\\n')).start()"
            }))
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let after = tool.execute(json!({ "code": "'lost' in dir()" })).await;

        assert!(after.outcome.is_success(), "{}", extract_text(&after));
        assert_eq!(extract_text(&after), "False");
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn the_kernel_process_is_journaled_while_it_lives() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("orphans.jsonl");
        let host = MushHost::new(dir.path().into());
        host.jobs()
            .set_orphan_journal(crate::orphans::OrphanJournal::at(&journal));
        let tool = IpythonTool::with_mush_host(host, dir.path().into());

        let pid = tool
            .execute(json!({ "code": "import os\nos.getpid()" }))
            .await;
        let running = std::fs::read_to_string(&journal).unwrap();
        tool.shutdown().await;
        let finished = std::fs::read_to_string(&journal).unwrap();

        let pid = extract_text(&pid);
        assert!(running.contains(&format!("\"pid\":{pid},")), "{running}");
        assert!(running.contains("\"active\":true"), "{running}");
        assert!(
            finished
                .lines()
                .last()
                .unwrap()
                .contains("\"active\":false"),
            "{finished}"
        );
    }

    #[tokio::test]
    async fn a_successful_cell_saves_the_kernel_state_after_a_pause() {
        use mush_kernel::KernelState;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifacts/kernel-state.pickle");
        let mut tool = tool(dir.path());
        tool.snapshot_debounce = std::time::Duration::from_millis(50);
        tool.attach_session(mush_kernel::KernelSession {
            session_dir: dir.path().join("artifacts"),
            state_path: path.clone(),
            depth: 0,
            max_depth: 1,
        });

        let failed = tool.execute(json!({ "code": "x = 1\n1 / 0" })).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let skipped = path.exists();
        let _ = tool.execute(json!({ "code": "kept = 41" })).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !path.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        tool.reset().await;
        tool.restore_from(&path).await;
        let back = tool.execute(json!({ "code": "kept + x" })).await;

        assert!(failed.outcome.is_error());
        assert!(!skipped);
        assert_eq!(extract_text(&back), "42");
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn restoring_without_a_snapshot_leaves_the_kernel_unstarted() {
        use mush_kernel::KernelState;
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let restored = tool.restore_from(&dir.path().join("missing.pickle")).await;

        assert_eq!(restored, None);
        assert_eq!(tool.live_names().await, None);
    }

    #[tokio::test]
    async fn restoring_a_snapshot_starts_the_kernel_right_away() {
        use mush_kernel::KernelState;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kernel-state.pickle");
        let first = tool(dir.path());
        let _ = first.execute(json!({ "code": "kept = 1" })).await;
        first.snapshot(&path).await.unwrap();
        first.shutdown().await;
        let second = tool(dir.path());

        let restored = second.restore_from(&path).await;
        let names = second.live_names().await;

        assert!(
            matches!(&restored, Some(Ok(report)) if report.restored == ["kept"]),
            "{restored:?}"
        );
        assert_eq!(names, Some(Ok(vec!["kept".to_string()])));
        second.shutdown().await;
    }

    #[tokio::test]
    async fn snapshotting_a_never_started_kernel_is_a_no_op() {
        use mush_kernel::KernelState;
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());

        let report = tool.snapshot(&dir.path().join("x.pickle")).await.unwrap();

        assert!(report.is_none());
        assert!(!dir.path().join("x.pickle").exists());
    }

    fn attached_tool(dir: &Path) -> IpythonTool {
        use mush_kernel::KernelState;
        let tool = tool(dir);
        tool.attach_session(mush_kernel::KernelSession {
            session_dir: dir.join("artifacts"),
            state_path: dir.join("artifacts/kernel-state.pickle"),
            depth: 0,
            max_depth: 1,
        });
        tool
    }

    #[tokio::test]
    async fn a_cell_does_not_wait_for_the_auto_save_of_the_cell_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pickling");
        let mut tool = attached_tool(dir.path());
        tool.snapshot_debounce = Duration::from_millis(10);
        let first = tool
            .execute(json!({ "code": format!(
                "import pathlib, time\nclass Slow:\n    def __reduce__(self):\n        pathlib.Path({:?}).touch()\n        time.sleep(30)\n        return (Slow, ())\nkeep = 41\nslow = Slow()",
                marker.to_string_lossy()
            ) }))
            .await;
        assert!(first.outcome.is_success(), "{}", extract_text(&first));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the auto-save never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let started = std::time::Instant::now();
        let next = tool.execute(json!({ "code": "del slow\nkeep + 1" })).await;

        assert_eq!(extract_text(&next), "42");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the cell waited {:?} for the auto-save",
            started.elapsed()
        );
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn releasing_an_idle_kernel_saves_its_state_and_the_next_cell_revives_it() {
        use mush_kernel::{KernelState, Release};
        let dir = tempfile::tempdir().unwrap();
        let tool = attached_tool(dir.path());
        let _ = tool
            .execute(json!({ "code": "kept = 41\ngen = (i for i in range(3))" }))
            .await;
        let pid = tool.pid().unwrap();

        let Release::Released(report) = tool.release().await else {
            panic!("the idle kernel was kept");
        };
        assert_eq!(report.saved, ["kept"]);
        assert_eq!(report.skipped, ["gen"]);
        assert_eq!(tool.pid(), None);
        assert_eq!(mush_kernel::process::memory(pid), None);
        assert_eq!(tool.release().await, Release::NotRunning);

        let revived = tool.execute(json!({ "code": "kept + 1" })).await;
        assert_eq!(
            extract_text(&revived),
            format!(
                "[python-state-restored]\n\nYour Python kernel state was revived from your \
                 previous session. These names are available again: kept.\nThese could not be \
                 restored and must be recreated if needed: gen.\n{}\n\n42",
                mush_kernel::RESTART_RESETS
            )
        );
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn a_kernel_with_a_background_job_or_a_child_process_is_not_released() {
        use mush_kernel::{KernelState, Release};
        let dir = tempfile::tempdir().unwrap();
        let tool = attached_tool(dir.path());

        let _ = tool
            .execute(json!({ "code": "h = bash('sleep 30')" }))
            .await;
        assert_eq!(tool.release().await, Release::Busy);
        let _ = tool.execute(json!({ "code": "await h.kill()" })).await;

        let _ = tool
            .execute(json!({ "code": "import subprocess\np = subprocess.Popen(['sleep', '30'])" }))
            .await;
        assert_eq!(tool.release().await, Release::Busy);
        let _ = tool.execute(json!({ "code": "p.kill()\np.wait()" })).await;

        let _ = tool
            .execute(json!({ "code": "watcher = asyncio.ensure_future(asyncio.sleep(30))" }))
            .await;
        assert_eq!(tool.release().await, Release::Busy);
        let _ = tool.execute(json!({ "code": "watcher.cancel()" })).await;

        assert!(matches!(tool.release().await, Release::Released(_)));
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn a_kernel_running_a_cell_is_not_released() {
        use mush_kernel::{KernelState, Release};
        let dir = tempfile::tempdir().unwrap();
        let tool = Arc::new(attached_tool(dir.path()));
        let _ = tool.execute(json!({ "code": "1" })).await;
        let running = tokio::spawn({
            let tool = Arc::clone(&tool);
            async move {
                tool.execute(json!({ "code": "await asyncio.sleep(30)" }))
                    .await
            }
        });
        wait_until_waiting(&tool).await;

        assert_eq!(tool.release().await, Release::Busy);
        tool.wake_waits();
        let _ = tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .unwrap();
        tool.shutdown().await;
    }

    async fn wait_until_waiting(tool: &IpythonTool) -> mush_kernel::Waiting {
        use mush_kernel::KernelState;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(waiting) = tool.waiting() {
                return waiting;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the cell never waited"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn woken(tool: &Arc<IpythonTool>, code: &str) -> (mush_kernel::Waiting, ToolResult) {
        use mush_kernel::KernelState;
        let cell = tokio::spawn({
            let tool = Arc::clone(tool);
            let code = code.to_string();
            async move { tool.execute(json!({ "code": code })).await }
        });
        let waiting = wait_until_waiting(tool).await;
        tool.wake_waits();
        let result = tokio::time::timeout(Duration::from_secs(10), cell)
            .await
            .unwrap()
            .unwrap();
        (waiting, result)
    }

    #[tokio::test]
    async fn a_message_stops_a_cell_waiting_on_bash_and_the_job_keeps_running() {
        use mush_kernel::KernelState;
        let dir = tempfile::tempdir().unwrap();
        let tool = Arc::new(tool(dir.path()));

        let (waiting, result) =
            woken(&tool, "r = await bash('sleep 30; echo late')\nr.output").await;

        assert_eq!(waiting.on, "bash job-1: sleep 30; echo late");
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(
            text.contains(
                "WaitInterrupted: a message arrived while this cell waited on bash job-1: sleep \
                 30; echo late, so the wait stopped."
            ),
            "{text}"
        );
        assert!(text.contains("saved as `_wait`"), "{text}");
        assert_eq!(tool.waiting(), None);
        let still = tool
            .execute(json!({ "code": "await _wait.running()" }))
            .await;
        assert_eq!(extract_text(&still), "True");
        let _ = tool.execute(json!({ "code": "await _wait.kill()" })).await;
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn a_message_stops_sleeps_in_cell_code_even_one_that_starts_after_it() {
        use mush_kernel::KernelState;
        let dir = tempfile::tempdir().unwrap();
        let tool = Arc::new(tool(dir.path()));

        let (waiting, result) = woken(&tool, "await asyncio.sleep(30)").await;
        assert_eq!(waiting.on, "asyncio.sleep(30)");
        assert!(
            extract_text(&result).contains("WaitInterrupted"),
            "{}",
            extract_text(&result)
        );

        let (waiting, result) = woken(&tool, "import time\ntime.sleep(30)").await;
        assert_eq!(waiting.on, "time.sleep(30)");
        assert!(
            extract_text(&result).contains("WaitInterrupted"),
            "{}",
            extract_text(&result)
        );

        let cell = tokio::spawn({
            let tool = Arc::clone(&tool);
            async move {
                tool.execute(json!({
                    "code": "import time\nend = time.monotonic() + 1.5\nwhile time.monotonic() < end:\n    pass\nawait asyncio.sleep(30)"
                }))
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        tool.wake_waits();
        let late = tokio::time::timeout(Duration::from_secs(10), cell)
            .await
            .unwrap()
            .unwrap();
        assert!(
            extract_text(&late).contains("WaitInterrupted"),
            "{}",
            extract_text(&late)
        );

        let after = tool
            .execute(json!({ "code": "await asyncio.sleep(0.1)\n'slept'" }))
            .await;
        assert_eq!(extract_text(&after), "'slept'");
        tool.shutdown().await;
    }

    #[test]
    fn output_formatting_orders_stdout_stderr_result_error() {
        let output = CellOutput {
            stdout: "out".into(),
            attachments: Vec::new(),
            background: "late\n".into(),
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
            "out\n[stderr]\nwarn\n7\nKeyboardInterrupt: interrupted\n[background output (unattributed)]\nlate\n"
        );
    }
}
