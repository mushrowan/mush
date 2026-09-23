use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

use crate::KernelError;
use crate::protocol::{Event, PROTOCOL_VERSION, Request};

const SHIM: &str = include_str!("../shim/mush_kernel.py");
const STDERR_TAIL_LINES: usize = 200;
const BUSY_INTERRUPT_INTERVAL: Duration = Duration::from_millis(500);
pub const MAX_BACKGROUND_OUTPUT_CHARS: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct KernelConfig {
    pub python: Option<PathBuf>,
    pub managed_venv: Option<crate::venv::ManagedVenv>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub ready_timeout: Duration,
    pub busy_wait: Duration,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            python: None,
            managed_venv: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env: Vec::new(),
            ready_timeout: Duration::from_secs(20),
            busy_wait: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HostError(pub String);

impl From<String> for HostError {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<&str> for HostError {
    fn from(message: &str) -> Self {
        Self(message.to_string())
    }
}

#[async_trait::async_trait]
pub trait HostRequestHandler: Send + Sync {
    async fn handle(&self, kind: &str, payload: Value) -> Result<Value, HostError>;

    fn cancellable(&self, _kind: &str) -> bool {
        false
    }
}

pub struct NoHost;

#[async_trait::async_trait]
impl HostRequestHandler for NoHost {
    async fn handle(&self, kind: &str, _payload: Value) -> Result<Value, HostError> {
        Err(HostError(format!("no host handler for {kind}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellError {
    pub ename: String,
    pub evalue: String,
    pub traceback: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellOutput {
    pub stdout: String,
    pub stderr: String,
    pub result: Option<String>,
    pub error: Option<CellError>,
    pub attachments: Vec<Attachment>,
    pub background: String,
    pub ok: bool,
    pub duration: Duration,
}

pub const ATTACHMENT_MIME: &str = "application/vnd.mush.attachment+json";
const MAX_ATTACHMENT_CHARS: usize = 30_000_000;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Attachment {
    pub mime_type: String,
    pub data: String,
    #[serde(default)]
    pub path: Option<String>,
}

impl CellOutput {
    #[must_use]
    pub fn interrupted(&self) -> bool {
        self.error
            .as_ref()
            .is_some_and(|error| error.ename == "KeyboardInterrupt")
    }
}

pub const DEFAULT_MAX_OUTPUT_CHARS: usize = 64 * 1024;

#[derive(Debug, Default)]
struct Capped {
    text: String,
    chars: usize,
    truncated: bool,
}

impl Capped {
    fn push(&mut self, text: &str, cap: usize) {
        if self.truncated {
            return;
        }
        let room = cap.saturating_sub(self.chars);
        match text.char_indices().nth(room) {
            Some((cut, _)) => {
                self.text.push_str(&text[..cut]);
                self.chars = cap;
                self.truncated = true;
            }
            None => {
                self.text.push_str(text);
                self.chars += text.chars().count();
            }
        }
    }

    fn finish(self, what: &str, cap: usize) -> String {
        let mut text = self.text;
        if self.truncated {
            text.push_str(&format!("\n[... {what} truncated at {cap} chars ...]"));
        }
        text
    }
}

fn capped(text: String, cap: usize) -> String {
    let mut out = Capped::default();
    out.push(&text, cap);
    out.finish("output", cap)
}

struct Shared {
    cells: Mutex<HashMap<String, mpsc::UnboundedSender<Event>>>,
    background: Mutex<Capped>,
    current: Mutex<Option<String>>,
    running: Mutex<Option<String>>,
    idle: tokio::sync::Notify,
    stderr_tail: Mutex<VecDeque<String>>,
    handler: Arc<dyn HostRequestHandler>,
    requests: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
}

pub struct Kernel {
    child: Mutex<Option<Child>>,
    shared: Arc<Shared>,
    exec_lock: tokio::sync::Mutex<()>,
    next_id: AtomicU64,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    _shim_dir: tempfile::TempDir,
    busy_wait: Duration,
    pub python: PathBuf,
    pub python_version: String,
}

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernel")
            .field("python", &self.python)
            .field("python_version", &self.python_version)
            .finish_non_exhaustive()
    }
}

impl Kernel {
    pub async fn start(
        config: KernelConfig,
        handler: Arc<dyn HostRequestHandler>,
    ) -> Result<Self, KernelError> {
        Self::start_with_progress(config, handler, &|_| {}).await
    }

    pub async fn start_with_progress(
        config: KernelConfig,
        handler: Arc<dyn HostRequestHandler>,
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<Self, KernelError> {
        let python = crate::python::PythonPlan::resolve(
            config.python.as_deref(),
            config.managed_venv.as_ref(),
        )?
        .realise(progress)
        .await?;
        let shim_dir = tempfile::Builder::new().prefix("mush-kernel-").tempdir()?;
        let shim_path = shim_dir.path().join("mush_kernel.py");
        std::fs::write(&shim_path, SHIM)?;

        let mut command = tokio::process::Command::new(&python);
        command
            .arg(&shim_path)
            .current_dir(&config.cwd)
            .env("PYTHONUNBUFFERED", "1")
            .envs(config.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or(KernelError::Pipes)?;
        let stdout = child.stdout.take().ok_or(KernelError::Pipes)?;
        let stderr = child.stderr.take().ok_or(KernelError::Pipes)?;

        let shared = Arc::new(Shared {
            cells: Mutex::new(HashMap::new()),
            background: Mutex::new(Capped::default()),
            current: Mutex::new(None),
            running: Mutex::new(None),
            idle: tokio::sync::Notify::new(),
            stderr_tail: Mutex::new(VecDeque::new()),
            handler,
            requests: Mutex::new(HashMap::new()),
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
        });

        tokio::spawn(tail_stderr(stderr, shared.clone()));

        let mut frames = Frames::new(stdout);
        let ready = tokio::time::timeout(config.ready_timeout, frames.next()).await;
        let python_version = match ready {
            Ok(Ok(Some(Frame::Oversized))) => {
                return Err(KernelError::Protocol(
                    "first line was larger than a protocol frame".into(),
                ));
            }
            Ok(Ok(Some(Frame::Line(line)))) => match serde_json::from_str::<Event>(&line) {
                Ok(Event::Ready { protocol, python }) if protocol == PROTOCOL_VERSION => python,
                Ok(Event::Ready { protocol, .. }) => {
                    return Err(KernelError::Protocol(format!(
                        "kernel speaks protocol {protocol}, host speaks {PROTOCOL_VERSION}"
                    )));
                }
                Ok(other) => {
                    return Err(KernelError::Protocol(format!(
                        "expected ready, got {other:?}"
                    )));
                }
                Err(error) => {
                    return Err(KernelError::Protocol(format!(
                        "first line was not an event: {error}: {line}"
                    )));
                }
            },
            Ok(Ok(None)) => {
                return Err(KernelError::Exited(shared.stderr_tail_text()));
            }
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => return Err(KernelError::ReadyTimeout(config.ready_timeout)),
        };

        let reader = tokio::spawn(read_events(frames, shared.clone()));

        Ok(Self {
            child: Mutex::new(Some(child)),
            shared,
            exec_lock: tokio::sync::Mutex::new(()),
            next_id: AtomicU64::new(1),
            reader: Mutex::new(Some(reader)),
            _shim_dir: shim_dir,
            busy_wait: config.busy_wait,
            python,
            python_version,
        })
    }

    #[must_use]
    pub fn stderr_tail(&self) -> String {
        self.shared.stderr_tail_text()
    }

    pub async fn execute(&self, code: &str) -> Result<CellOutput, KernelError> {
        self.execute_streaming(code, |_| {}).await
    }

    pub async fn execute_streaming(
        &self,
        code: &str,
        on_event: impl FnMut(&Event) + Send,
    ) -> Result<CellOutput, KernelError> {
        self.execute_from(code, None, on_event).await
    }

    pub async fn execute_from(
        &self,
        code: &str,
        origin: Option<&str>,
        mut on_event: impl FnMut(&Event) + Send,
    ) -> Result<CellOutput, KernelError> {
        let _guard = self.exec_lock.lock().await;
        self.wait_until_idle().await?;
        let id = self.fresh_id("cell");
        let mut rx = self.shared.register(&id);
        self.shared.set_current(Some(id.clone()));
        self.shared.set_running(Some(id.clone()));
        let started = Instant::now();
        let sent = self
            .send(&Request::Execute {
                id: id.clone(),
                code: code.to_string(),
                origin: origin.map(str::to_owned),
            })
            .await;
        if let Err(error) = sent {
            self.shared.unregister(&id);
            self.shared.set_running(None);
            return Err(error);
        }

        let mut output = CellOutput::default();
        let mut stdout = Capped::default();
        let mut stderr = Capped::default();
        let mut finished = false;
        let mut abandon = InterruptOnDrop {
            shared: Some((self.shared.clone(), id.clone())),
        };
        while let Some(event) = rx.recv().await {
            on_event(&event);
            match event {
                Event::Stdout { id: None, .. } | Event::Stderr { id: None, .. } => {}
                Event::Stdout { text, .. } => stdout.push(&text, DEFAULT_MAX_OUTPUT_CHARS),
                Event::Stderr { text, .. } => stderr.push(&text, DEFAULT_MAX_OUTPUT_CHARS),
                Event::Result { text, .. } => {
                    output.result = Some(capped(text, DEFAULT_MAX_OUTPUT_CHARS));
                }
                Event::Display { data, .. } => {
                    if let Some(payload) = data.get(ATTACHMENT_MIME) {
                        match serde_json::from_value::<Attachment>(payload.clone()) {
                            Ok(attachment) if attachment.data.len() > MAX_ATTACHMENT_CHARS => {
                                stderr.push(
                                    "attachment dropped: larger than the host accepts\n",
                                    DEFAULT_MAX_OUTPUT_CHARS,
                                );
                            }
                            Ok(attachment) => output.attachments.push(attachment),
                            Err(error) => tracing::debug!(%error, "malformed attachment display"),
                        }
                    }
                }
                Event::Error {
                    ename,
                    evalue,
                    traceback,
                    ..
                } => {
                    output.error = Some(CellError {
                        ename,
                        evalue,
                        traceback,
                    });
                }
                Event::Done { ok, .. } => {
                    output.ok = ok;
                    output.background = self.shared.take_background();
                    finished = true;
                    break;
                }
                Event::Ready { .. } | Event::HostRequest { .. } | Event::HostCancel { .. } => {}
            }
        }
        abandon.shared.take();
        self.shared.unregister(&id);
        output.stdout = stdout.finish("output", DEFAULT_MAX_OUTPUT_CHARS);
        output.stderr = stderr.finish("output", DEFAULT_MAX_OUTPUT_CHARS);
        output.duration = started.elapsed();
        if finished {
            Ok(output)
        } else {
            Err(KernelError::Exited(self.shared.stderr_tail_text()))
        }
    }

    pub async fn interrupt(&self) -> Result<(), KernelError> {
        self.send(&Request::Interrupt { id: None }).await
    }

    pub async fn interrupt_cell(&self, id: &str) -> Result<(), KernelError> {
        self.send(&Request::Interrupt {
            id: Some(id.to_string()),
        })
        .await
    }

    #[must_use]
    pub fn busy(&self) -> bool {
        self.shared.running().is_some()
    }

    async fn wait_until_idle(&self) -> Result<(), KernelError> {
        let started = Instant::now();
        while self.busy() {
            if started.elapsed() >= self.busy_wait {
                return Err(KernelError::Busy);
            }
            if let Some(id) = self.shared.running() {
                let _ = self.interrupt_cell(&id).await;
            }
            let _ =
                tokio::time::timeout(BUSY_INTERRUPT_INTERVAL, self.shared.idle.notified()).await;
        }
        Ok(())
    }

    pub async fn kill(self) {
        let child = self
            .child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mut child) = child {
            let _ = child.kill().await;
        }
        if let Some(reader) = self
            .reader
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            reader.abort();
        }
    }

    pub async fn snapshot(
        &self,
        path: &std::path::Path,
        max_variable_bytes: Option<u64>,
    ) -> Result<SnapshotReport, KernelError> {
        let id = self.fresh_id("snapshot");
        let extra = self
            .request_done(
                &id,
                &Request::Snapshot {
                    id: id.clone(),
                    path: path.to_string_lossy().into_owned(),
                    max_variable_bytes,
                },
            )
            .await?;
        Ok(SnapshotReport {
            saved: names(&extra, "saved"),
            skipped: names(&extra, "skipped"),
        })
    }

    pub async fn restore(&self, path: &std::path::Path) -> Result<RestoreReport, KernelError> {
        let id = self.fresh_id("restore");
        let extra = self
            .request_done(
                &id,
                &Request::Restore {
                    id: id.clone(),
                    path: path.to_string_lossy().into_owned(),
                },
            )
            .await?;
        Ok(RestoreReport {
            restored: names(&extra, "restored"),
            failed: names(&extra, "failed"),
        })
    }

    pub async fn list_names(&self) -> Result<Vec<String>, KernelError> {
        let id = self.fresh_id("names");
        let extra = self
            .request_done(&id, &Request::ListNames { id: id.clone() })
            .await?;
        Ok(names(&extra, "names"))
    }

    pub async fn shutdown(self) -> Result<(), KernelError> {
        let _ = self.send(&Request::Shutdown).await;
        let child = self
            .child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mut child) = child {
            match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                Ok(_) => {}
                Err(_) => {
                    let _ = child.kill().await;
                }
            }
        }
        if let Some(reader) = self
            .reader
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            reader.abort();
        }
        Ok(())
    }

    async fn request_done(
        &self,
        id: &str,
        request: &Request,
    ) -> Result<serde_json::Map<String, Value>, KernelError> {
        let _guard = self.exec_lock.lock().await;
        let mut rx = self.shared.register(id);
        if let Err(error) = self.send(request).await {
            self.shared.unregister(id);
            return Err(error);
        }
        let outcome = loop {
            match rx.recv().await {
                Some(Event::Done { ok, extra, .. }) => {
                    if ok {
                        break Ok(extra);
                    }
                    let reason = extra
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("request failed")
                        .to_string();
                    break Err(KernelError::Request(reason));
                }
                Some(Event::Error { evalue, .. }) => break Err(KernelError::Request(evalue)),
                Some(_) => continue,
                None => break Err(KernelError::Exited(self.shared.stderr_tail_text())),
            }
        };
        self.shared.unregister(id);
        outcome
    }

    fn fresh_id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    async fn send(&self, request: &Request) -> Result<(), KernelError> {
        write_request(&self.shared.stdin, request).await
    }
}

struct InterruptOnDrop {
    shared: Option<(Arc<Shared>, String)>,
}

impl Drop for InterruptOnDrop {
    fn drop(&mut self) {
        if let Some((shared, id)) = self.shared.take() {
            shared.unregister(&id);
            tokio::spawn(async move {
                let _ = write_request(&shared.stdin, &Request::Interrupt { id: Some(id) }).await;
            });
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotReport {
    pub saved: Vec<String>,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub restored: Vec<String>,
    pub failed: Vec<String>,
}

fn names(extra: &serde_json::Map<String, Value>, key: &str) -> Vec<String> {
    extra
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

async fn write_request(
    stdin: &tokio::sync::Mutex<ChildStdin>,
    request: &Request,
) -> Result<(), KernelError> {
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    let mut stdin = stdin.lock().await;
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

impl Shared {
    fn register(&self, id: &str) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.cells
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.to_string(), tx);
        rx
    }

    fn unregister(&self, id: &str) {
        self.cells
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if current.as_deref() == Some(id) {
            *current = None;
        }
    }

    fn set_current(&self, id: Option<String>) {
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = id;
    }

    fn running(&self) -> Option<String> {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_running(&self, id: Option<String>) {
        *self.running.lock().unwrap_or_else(PoisonError::into_inner) = id;
    }

    fn finish_running(&self, id: &str) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if running.as_deref() == Some(id) {
            *running = None;
            self.idle.notify_waiters();
        }
    }

    fn owns(&self, id: Option<&str>) -> bool {
        id.is_some_and(|id| {
            self.cells
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(id)
        })
    }

    fn push_background(&self, text: &str) {
        self.background
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(text, MAX_BACKGROUND_OUTPUT_CHARS);
    }

    fn take_background(&self) -> String {
        std::mem::take(
            &mut *self
                .background
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
        .finish("background output", MAX_BACKGROUND_OUTPUT_CHARS)
    }

    fn route(&self, event: Event) {
        if let Event::Done { id, .. } = &event {
            self.finish_running(id);
        }
        let event = match event {
            Event::Stdout { id, text } if !self.owns(id.as_deref()) => {
                self.push_background(&text);
                Event::Stdout { id: None, text }
            }
            Event::Stderr { id, text } if !self.owns(id.as_deref()) => {
                self.push_background(&text);
                Event::Stderr { id: None, text }
            }
            other => other,
        };
        let target = event.cell_id().map(str::to_string).or_else(|| {
            self.current
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        });
        let Some(target) = target else {
            tracing::debug!(?event, "kernel event with no cell to deliver to");
            return;
        };
        let sender = self
            .cells
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&target)
            .cloned();
        match sender {
            Some(sender) => {
                let _ = sender.send(event);
            }
            None => tracing::debug!(target, ?event, "kernel event for an unknown cell"),
        }
    }

    fn fail_running(&self, reason: String) {
        let Some(id) = self.running() else {
            tracing::warn!(reason, "bad kernel frame with no running cell");
            return;
        };
        let sender = self
            .cells
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned();
        if let Some(sender) = sender {
            let _ = sender.send(Event::Error {
                id: Some(id.clone()),
                ename: "ProtocolError".into(),
                evalue: reason,
                traceback: Vec::new(),
            });
            let _ = sender.send(Event::Done {
                id: id.clone(),
                ok: false,
                extra: serde_json::Map::new(),
            });
        }
        let stdin = self.stdin.clone();
        tokio::spawn(async move {
            let _ = write_request(&stdin, &Request::Interrupt { id: Some(id) }).await;
        });
    }

    fn stderr_tail_text(&self) -> String {
        self.stderr_tail
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

const MAX_PROTOCOL_LINE_BYTES: usize = 32 * 1024 * 1024;

enum Frame {
    Line(String),
    Oversized,
}

struct Frames<R> {
    reader: BufReader<R>,
    buf: Vec<u8>,
}

impl<R: tokio::io::AsyncRead + Unpin> Frames<R> {
    fn new(stdout: R) -> Self {
        Self {
            reader: BufReader::new(stdout),
            buf: Vec::new(),
        }
    }

    async fn next(&mut self) -> std::io::Result<Option<Frame>> {
        let mut oversized = false;
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                if oversized {
                    return Ok(Some(Frame::Oversized));
                }
                if self.buf.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(self.take_line()));
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let chunk = newline.unwrap_or(available.len());
            if !oversized && self.buf.len() + chunk > MAX_PROTOCOL_LINE_BYTES {
                oversized = true;
                self.buf = Vec::new();
            }
            if !oversized {
                self.buf.extend_from_slice(&available[..chunk]);
            }
            match newline {
                Some(at) => {
                    self.reader.consume(at + 1);
                    if oversized {
                        return Ok(Some(Frame::Oversized));
                    }
                    return Ok(Some(self.take_line()));
                }
                None => self.reader.consume(chunk),
            }
        }
    }

    fn take_line(&mut self) -> Frame {
        let line = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        Frame::Line(line)
    }
}

async fn read_events(mut frames: Frames<tokio::process::ChildStdout>, shared: Arc<Shared>) {
    while let Ok(Some(frame)) = frames.next().await {
        let line = match frame {
            Frame::Line(line) => line,
            Frame::Oversized => {
                shared.fail_running(format!(
                    "oversized protocol line: exceeds {MAX_PROTOCOL_LINE_BYTES} bytes"
                ));
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let event = match serde_json::from_str::<Event>(&line) {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(%error, line, "unparseable kernel event");
                shared.fail_running(format!("unparseable protocol frame: {error}"));
                continue;
            }
        };
        if let Event::HostCancel { id } = &event {
            let request = shared
                .requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(id);
            if let Some(request) = request {
                request.abort();
            }
            continue;
        }
        if let Event::HostRequest {
            id, kind, payload, ..
        } = event
        {
            let cancellable = shared.handler.cancellable(&kind);
            let mut requests = shared
                .requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let task = tokio::spawn({
                let shared = shared.clone();
                let id = id.clone();
                async move {
                    let reply = shared.handler.handle(&kind, payload).await;
                    shared
                        .requests
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&id);
                    reply_to_host(&shared, id, reply).await;
                }
            });
            if cancellable {
                requests.insert(id, task.abort_handle());
            }
            continue;
        }
        shared.route(event);
    }
    shared
        .cells
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    shared.set_running(None);
    shared.idle.notify_waiters();
}

async fn reply_to_host(shared: &Shared, id: String, reply: Result<Value, HostError>) {
    let reply = match reply {
        Ok(value) => Request::HostReply {
            id,
            ok: true,
            value: Some(value),
            error: None,
        },
        Err(HostError(message)) => Request::HostReply {
            id,
            ok: false,
            value: None,
            error: Some(message),
        },
    };
    if let Err(error) = write_request(&shared.stdin, &reply).await {
        tracing::warn!(%error, "failed to answer a kernel host request");
    }
}

async fn tail_stderr(stderr: tokio::process::ChildStderr, shared: Arc<Shared>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut tail = shared
            .stderr_tail
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if tail.len() >= STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_oversized_line_is_skipped_whole_and_the_next_line_still_reads() {
        let mut input = vec![b'x'; MAX_PROTOCOL_LINE_BYTES + 10];
        input.extend_from_slice(b"\nnext\ntail");
        let mut frames = Frames::new(input.as_slice());

        assert!(matches!(
            frames.next().await.unwrap(),
            Some(Frame::Oversized)
        ));
        assert!(matches!(frames.next().await.unwrap(), Some(Frame::Line(line)) if line == "next"));
        assert!(matches!(frames.next().await.unwrap(), Some(Frame::Line(line)) if line == "tail"));
        assert!(frames.next().await.unwrap().is_none());
    }
}
