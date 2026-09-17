use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use mush_runtime::ClientCommand;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::protocol::{Frame, Request, decode, encode};

const SPAWN_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

#[must_use]
pub fn daemon_socket_path() -> PathBuf {
    mush_agent::ipc::runtime_dir().join("daemon.sock")
}

#[must_use]
pub fn workers_dir() -> PathBuf {
    mush_agent::ipc::runtime_dir().join("workers")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum OpenTarget {
    New,
    Resume(String),
    Fork(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SupervisorRequest {
    Open {
        target: OpenTarget,
        args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    },
    List,
    Stop {
        session_id: String,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SupervisorReply {
    Worker {
        descriptor: WorkerDescriptor,
        fresh: bool,
    },
    Workers(Vec<WorkerDescriptor>),
    Done,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerDescriptor {
    pub session_id: String,
    pub model_id: String,
    pub pid: u32,
    pub start_id: Option<String>,
    pub cwd: PathBuf,
    pub socket: PathBuf,
    pub started_at: u64,
}

impl WorkerDescriptor {
    #[must_use]
    pub fn alive(&self) -> bool {
        match (
            &self.start_id,
            mush_tools::orphans::process_start_id(self.pid),
        ) {
            (Some(expected), Some(actual)) => *expected == actual,
            (None, Some(_)) => true,
            (_, None) => false,
        }
    }
}

pub struct Supervisor {
    program: PathBuf,
    workers_dir: PathBuf,
    log_dir: PathBuf,
}

impl Supervisor {
    #[must_use]
    pub fn new(program: PathBuf, workers_dir: PathBuf, log_dir: PathBuf) -> Self {
        Self {
            program,
            workers_dir,
            log_dir,
        }
    }

    pub fn roster(&self) -> Vec<WorkerDescriptor> {
        let Ok(entries) = std::fs::read_dir(&self.workers_dir) else {
            return Vec::new();
        };
        let mut workers = Vec::new();
        for path in entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        {
            let descriptor = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<WorkerDescriptor>(&bytes).ok());
            match descriptor {
                Some(descriptor) if descriptor.alive() => workers.push(descriptor),
                Some(descriptor) => {
                    let _ = std::fs::remove_file(&path);
                    let _ = std::fs::remove_file(&descriptor.socket);
                }
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        workers.sort_by_key(|worker| worker.started_at);
        workers
    }

    pub async fn handle(&self, request: SupervisorRequest) -> SupervisorReply {
        match request {
            SupervisorRequest::Open {
                target,
                args,
                cwd,
                env,
            } => match self.open(target, args, cwd, env).await {
                Ok(reply) => reply,
                Err(error) => SupervisorReply::Error(error.to_string()),
            },
            SupervisorRequest::List => SupervisorReply::Workers(self.roster()),
            SupervisorRequest::Stop { session_id } => {
                let Some(worker) = self
                    .roster()
                    .into_iter()
                    .find(|worker| worker.session_id.starts_with(&session_id))
                else {
                    return SupervisorReply::Error(format!("no worker for session {session_id}"));
                };
                match stop(&worker).await {
                    Ok(()) => SupervisorReply::Done,
                    Err(error) => SupervisorReply::Error(error.to_string()),
                }
            }
            SupervisorRequest::Shutdown => {
                for worker in self.roster() {
                    if let Err(error) = stop(&worker).await {
                        tracing::warn!(%error, session = %worker.session_id, "worker did not stop");
                    }
                }
                SupervisorReply::Done
            }
        }
    }

    async fn open(
        &self,
        target: OpenTarget,
        args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    ) -> io::Result<SupervisorReply> {
        if let OpenTarget::Resume(id) = &target
            && let Some(worker) = self
                .roster()
                .into_iter()
                .find(|worker| worker.session_id.starts_with(id))
        {
            return Ok(SupervisorReply::Worker {
                descriptor: worker,
                fresh: false,
            });
        }
        let descriptor = self.spawn(&target, &args, &cwd, &env).await?;
        Ok(SupervisorReply::Worker {
            descriptor,
            fresh: true,
        })
    }

    async fn spawn(
        &self,
        target: &OpenTarget,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
    ) -> io::Result<WorkerDescriptor> {
        std::fs::create_dir_all(&self.workers_dir)?;
        std::fs::create_dir_all(&self.log_dir)?;
        let name = mush_ai::types::SessionId::new().to_string();
        let socket = self.workers_dir.join(format!("{name}.sock"));
        let descriptor_path = self.workers_dir.join(format!("{name}.json"));
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_dir.join(format!("{name}.log")))?;
        let mut command = tokio::process::Command::new(&self.program);
        command.args(args);
        match target {
            OpenTarget::New => {}
            OpenTarget::Resume(id) => {
                command.arg("--resume").arg(id);
            }
            OpenTarget::Fork(id) => {
                command.arg("--fork").arg(id);
            }
        }
        command
            .arg("worker")
            .arg("--socket")
            .arg(&socket)
            .arg("--descriptor")
            .arg(&descriptor_path)
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .process_group(0);
        let mut child = command.spawn()?;
        let deadline = tokio::time::Instant::now() + SPAWN_TIMEOUT;
        loop {
            if let Some(descriptor) = std::fs::read(&descriptor_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<WorkerDescriptor>(&bytes).ok())
                && !descriptor.session_id.is_empty()
            {
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                return Ok(descriptor);
            }
            if let Some(status) = child.try_wait()? {
                let log = std::fs::read_to_string(self.log_dir.join(format!("{name}.log")))
                    .unwrap_or_default();
                return Err(io::Error::other(format!(
                    "worker exited during startup ({status}):\n{}",
                    log.trim_end()
                )));
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.start_kill();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "worker did not announce itself",
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn stop(worker: &WorkerDescriptor) -> io::Result<()> {
    let graceful = tokio::time::timeout(STOP_TIMEOUT, async {
        let stream = UnixStream::connect(&worker.socket).await?;
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let request = Request::Command {
            id: 1,
            command: ClientCommand::Shutdown,
        };
        writer
            .write_all(&encode(&request).map_err(io::Error::other)?)
            .await?;
        while let Some(line) = lines.next_line().await? {
            if matches!(decode::<Frame>(&line), Ok(Frame::Closed)) {
                break;
            }
        }
        Ok::<(), io::Error>(())
    })
    .await;
    match graceful {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, pid = worker.pid, "graceful stop failed"),
        Err(_) => tracing::warn!(pid = worker.pid, "graceful stop timed out"),
    }
    for signal in [rustix::process::Signal::TERM, rustix::process::Signal::KILL] {
        if wait_for_exit(worker, STOP_TIMEOUT).await {
            break;
        }
        if let Some(pid) = i32::try_from(worker.pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(pid, signal);
        }
    }
    let _ = std::fs::remove_file(&worker.socket);
    Ok(())
}

async fn wait_for_exit(worker: &WorkerDescriptor, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while worker.alive() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

pub fn bind_or_reclaim(path: &Path) -> io::Result<UnixListener> {
    if path.symlink_metadata().is_ok() {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("a daemon already answers on {}", path.display()),
                ));
            }
            Err(_) => std::fs::remove_file(path)?,
        }
    }
    crate::worker::bind(path)
}

pub async fn run(socket: &Path, supervisor: Supervisor) -> io::Result<()> {
    let listener = bind_or_reclaim(socket)?;
    let result = serve(&listener, &supervisor).await;
    let _ = std::fs::remove_file(socket);
    result
}

async fn serve(listener: &UnixListener, supervisor: &Supervisor) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let Ok(Some(line)) = lines.next_line().await else {
            continue;
        };
        let request = match decode::<SupervisorRequest>(&line) {
            Ok(request) => request,
            Err(error) => {
                let reply = SupervisorReply::Error(format!("malformed request: {error}"));
                let _ = writer
                    .write_all(&encode(&reply).map_err(io::Error::other)?)
                    .await;
                continue;
            }
        };
        let shutdown = matches!(request, SupervisorRequest::Shutdown);
        let reply = supervisor.handle(request).await;
        let _ = writer
            .write_all(&encode(&reply).map_err(io::Error::other)?)
            .await;
        if shutdown {
            return Ok(());
        }
    }
}

pub async fn request(socket: &Path, request: &SupervisorRequest) -> io::Result<SupervisorReply> {
    let stream = UnixStream::connect(socket).await?;
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(&encode(request).map_err(io::Error::other)?)
        .await?;
    let mut lines = BufReader::new(reader).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed"))?;
    decode(&line).map_err(io::Error::other)
}
