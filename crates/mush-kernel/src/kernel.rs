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

#[derive(Debug, Clone)]
pub struct KernelConfig {
    pub python: Option<PathBuf>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub ready_timeout: Duration,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            python: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env: Vec::new(),
            ready_timeout: Duration::from_secs(20),
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
    pub ok: bool,
    pub duration: Duration,
}

impl CellOutput {
    #[must_use]
    pub fn interrupted(&self) -> bool {
        self.error
            .as_ref()
            .is_some_and(|error| error.ename == "KeyboardInterrupt")
    }
}

struct Shared {
    cells: Mutex<HashMap<String, mpsc::UnboundedSender<Event>>>,
    current: Mutex<Option<String>>,
    stderr_tail: Mutex<VecDeque<String>>,
    handler: Arc<dyn HostRequestHandler>,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
}

pub struct Kernel {
    child: Mutex<Option<Child>>,
    shared: Arc<Shared>,
    exec_lock: tokio::sync::Mutex<()>,
    next_id: AtomicU64,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    _shim_dir: tempfile::TempDir,
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
        let python = crate::python::resolve_python(config.python.as_deref())?;
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
            current: Mutex::new(None),
            stderr_tail: Mutex::new(VecDeque::new()),
            handler,
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
        });

        tokio::spawn(tail_stderr(stderr, shared.clone()));

        let mut lines = BufReader::new(stdout).lines();
        let ready = tokio::time::timeout(config.ready_timeout, lines.next_line()).await;
        let python_version = match ready {
            Ok(Ok(Some(line))) => match serde_json::from_str::<Event>(&line) {
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

        let reader = tokio::spawn(read_events(lines, shared.clone()));

        Ok(Self {
            child: Mutex::new(Some(child)),
            shared,
            exec_lock: tokio::sync::Mutex::new(()),
            next_id: AtomicU64::new(1),
            reader: Mutex::new(Some(reader)),
            _shim_dir: shim_dir,
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
        mut on_event: impl FnMut(&Event) + Send,
    ) -> Result<CellOutput, KernelError> {
        let _guard = self.exec_lock.lock().await;
        let id = self.fresh_id("cell");
        let mut rx = self.shared.register(&id);
        self.shared.set_current(Some(id.clone()));
        let started = Instant::now();
        let sent = self
            .send(&Request::Execute {
                id: id.clone(),
                code: code.to_string(),
            })
            .await;
        if let Err(error) = sent {
            self.shared.unregister(&id);
            return Err(error);
        }

        let mut output = CellOutput::default();
        let mut finished = false;
        while let Some(event) = rx.recv().await {
            on_event(&event);
            match event {
                Event::Stdout { text, .. } => output.stdout.push_str(&text),
                Event::Stderr { text, .. } => output.stderr.push_str(&text),
                Event::Result { text, .. } => output.result = Some(text),
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
                    finished = true;
                    break;
                }
                Event::Ready { .. } | Event::HostRequest { .. } => {}
            }
        }
        self.shared.unregister(&id);
        output.duration = started.elapsed();
        if finished {
            Ok(output)
        } else {
            Err(KernelError::Exited(self.shared.stderr_tail_text()))
        }
    }

    pub async fn interrupt(&self) -> Result<(), KernelError> {
        self.send(&Request::Interrupt).await
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

    fn route(&self, event: Event) {
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

async fn read_events(
    mut lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    shared: Arc<Shared>,
) {
    while let Ok(Some(line)) = lines.next_line().await {
        let event = match serde_json::from_str::<Event>(&line) {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(%error, line, "unparseable kernel event");
                continue;
            }
        };
        if let Event::HostRequest {
            id, kind, payload, ..
        } = event
        {
            let shared = shared.clone();
            tokio::spawn(async move {
                let reply = match shared.handler.handle(&kind, payload).await {
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
            });
            continue;
        }
        shared.route(event);
    }
    shared
        .cells
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
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
