use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::protocol::{decode, encode};

const SPAWN_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(5),
];

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
    Status,
    Stop {
        session_id: String,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SupervisorReply {
    Worker {
        descriptor: Box<WorkerDescriptor>,
        fresh: bool,
    },
    Workers(Vec<WorkerDescriptor>),
    Status {
        build: String,
        workers: usize,
    },
    Done,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerDescriptor {
    pub session_id: String,
    pub model_id: String,
    #[serde(default)]
    pub build: String,
    pub pid: u32,
    pub start_id: Option<String>,
    pub cwd: PathBuf,
    pub socket: PathBuf,
    pub started_at: u64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub turn_active: bool,
    #[serde(default)]
    pub messages: usize,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub clients: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
}

impl WorkerDescriptor {
    #[must_use]
    pub fn alive(&self) -> bool {
        if self.failed.is_some() || self.pid == 0 {
            return false;
        }
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
    nursery: std::sync::Arc<Nursery>,
}

struct Nursery {
    program: PathBuf,
    workers_dir: PathBuf,
    log_dir: PathBuf,
    residents: std::sync::Mutex<std::collections::HashMap<String, Resident>>,
}

#[derive(Clone)]
struct Recipe {
    args: Vec<String>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    session_id: String,
}

struct Resident {
    recipe: Recipe,
    pid: u32,
    stopping: bool,
}

impl Supervisor {
    #[must_use]
    pub fn new(program: PathBuf, workers_dir: PathBuf, log_dir: PathBuf) -> Self {
        Self {
            nursery: std::sync::Arc::new(Nursery {
                program,
                workers_dir,
                log_dir,
                residents: std::sync::Mutex::new(std::collections::HashMap::new()),
            }),
        }
    }

    pub fn roster(&self) -> Vec<WorkerDescriptor> {
        self.nursery.roster()
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
            SupervisorRequest::Status => SupervisorReply::Status {
                build: crate::build::build_id(),
                workers: self.roster().len(),
            },
            SupervisorRequest::Stop { session_id } => {
                let Some(worker) = self
                    .roster()
                    .into_iter()
                    .find(|worker| worker.session_id.starts_with(&session_id))
                else {
                    return SupervisorReply::Error(format!("no worker for session {session_id}"));
                };
                match self.nursery.stop(&worker).await {
                    Ok(()) => SupervisorReply::Done,
                    Err(error) => SupervisorReply::Error(error.to_string()),
                }
            }
            SupervisorRequest::Shutdown => {
                for worker in self.roster() {
                    if let Err(error) = self.nursery.stop(&worker).await {
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
                descriptor: Box::new(worker),
                fresh: false,
            });
        }
        let name = mush_ai::types::SessionId::new().to_string();
        let descriptor = self
            .nursery
            .clone()
            .start(&name, &target, &args, &cwd, &env, false)
            .await?;
        self.nursery.remember(
            &name,
            Recipe {
                args,
                cwd,
                env,
                session_id: descriptor.session_id.clone(),
            },
            descriptor.pid,
        );
        Ok(SupervisorReply::Worker {
            descriptor: Box::new(descriptor),
            fresh: true,
        })
    }
}

impl Nursery {
    fn socket_path(&self, name: &str) -> PathBuf {
        self.workers_dir.join(format!("{name}.sock"))
    }

    fn descriptor_path(&self, name: &str) -> PathBuf {
        self.workers_dir.join(format!("{name}.json"))
    }

    fn log_path(&self, name: &str) -> PathBuf {
        self.log_dir.join(format!("{name}.log"))
    }

    fn residents(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Resident>> {
        self.residents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn remember(&self, name: &str, recipe: Recipe, pid: u32) {
        self.residents().insert(
            name.to_string(),
            Resident {
                recipe,
                pid,
                stopping: false,
            },
        );
    }

    fn roster(&self) -> Vec<WorkerDescriptor> {
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
                Some(descriptor) if descriptor.alive() || descriptor.failed.is_some() => {
                    workers.push(descriptor);
                }
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

    fn name_of(&self, session_id: &str) -> Option<String> {
        self.residents()
            .iter()
            .find(|(_, resident)| resident.recipe.session_id == session_id)
            .map(|(name, _)| name.clone())
    }

    async fn stop(&self, worker: &WorkerDescriptor) -> io::Result<()> {
        if let Some(name) = self.name_of(&worker.session_id) {
            let mut residents = self.residents();
            if let Some(resident) = residents.get_mut(&name) {
                resident.stopping = true;
            }
        }
        if worker.failed.is_some() || worker.pid == 0 {
            let _ = std::fs::remove_file(descriptor_of(worker));
            let _ = std::fs::remove_file(&worker.socket);
            return Ok(());
        }
        stop(worker).await
    }

    async fn start(
        self: std::sync::Arc<Self>,
        name: &str,
        target: &OpenTarget,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        recovered: bool,
    ) -> io::Result<WorkerDescriptor> {
        std::fs::create_dir_all(&self.workers_dir)?;
        std::fs::create_dir_all(&self.log_dir)?;
        let socket = self.socket_path(name);
        let descriptor_path = self.descriptor_path(name);
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(&descriptor_path);
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path(name))?;
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
        if recovered {
            command.arg("--recovered");
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
                let nursery = std::sync::Arc::clone(&self);
                let name = name.to_string();
                tokio::spawn(async move {
                    let status = child.wait().await;
                    nursery.worker_exited(&name, status).await;
                });
                return Ok(descriptor);
            }
            if let Some(status) = child.try_wait()? {
                let log = std::fs::read_to_string(self.log_path(name)).unwrap_or_default();
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

    async fn worker_exited(self: std::sync::Arc<Self>, name: &str, status: io::Result<ExitStatus>) {
        let Some((recipe, pid)) = ({
            let mut residents = self.residents();
            match residents.get(name) {
                Some(resident) if resident.stopping => {
                    residents.remove(name);
                    None
                }
                Some(resident) => Some((resident.recipe.clone(), resident.pid)),
                None => None,
            }
        }) else {
            return;
        };
        if matches!(&status, Ok(status) if status.success()) {
            self.residents().remove(name);
            return;
        }
        let reason = match &status {
            Ok(status) => status.to_string(),
            Err(error) => error.to_string(),
        };
        tracing::warn!(name, %reason, "worker died, recovering");
        reap_group(pid);
        self.recover(name.to_string(), recipe, reason).await;
    }

    fn recover(
        self: std::sync::Arc<Self>,
        name: String,
        recipe: Recipe,
        died: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            self.recover_inner(&name, &recipe, &died).await;
        })
    }

    async fn recover_inner(self: std::sync::Arc<Self>, name: &str, recipe: &Recipe, died: &str) {
        let mut last = died.to_string();
        for delay in RETRY_DELAYS {
            tokio::time::sleep(delay).await;
            let abandoned = {
                let residents = self.residents();
                residents.get(name).is_none_or(|resident| resident.stopping)
            };
            if abandoned {
                return;
            }
            let started = std::sync::Arc::clone(&self)
                .start(
                    name,
                    &OpenTarget::Resume(recipe.session_id.clone()),
                    &recipe.args,
                    &recipe.cwd,
                    &recipe.env,
                    true,
                )
                .await;
            match started {
                Ok(descriptor) => {
                    tracing::info!(name, pid = descriptor.pid, "worker recovered");
                    {
                        let mut residents = self.residents();
                        if let Some(resident) = residents.get_mut(name) {
                            resident.pid = descriptor.pid;
                        }
                    }
                    return;
                }
                Err(error) => last = error.to_string(),
            }
        }
        tracing::error!(name, %last, "worker gave up after three restarts");
        self.mark_failed(name, recipe, &last);
        self.residents().remove(name);
    }

    fn mark_failed(&self, name: &str, recipe: &Recipe, reason: &str) {
        let descriptor = WorkerDescriptor {
            session_id: recipe.session_id.clone(),
            model_id: String::new(),
            build: crate::build::build_id(),
            pid: 0,
            start_id: None,
            cwd: recipe.cwd.clone(),
            socket: self.socket_path(name),
            started_at: now_secs(),
            title: None,
            turn_active: false,
            messages: 0,
            updated_at: now_secs(),
            clients: 0,
            failed: Some(reason.lines().next().unwrap_or(reason).to_string()),
        };
        if let Ok(bytes) = serde_json::to_vec(&descriptor) {
            let _ = mush_ai::private_io::write_private(&self.descriptor_path(name), bytes);
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn descriptor_of(worker: &WorkerDescriptor) -> PathBuf {
    worker.socket.with_extension("json")
}

fn reap_group(pid: u32) {
    if pid == 0 {
        return;
    }
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };
    if rustix::process::getpgid(Some(pid)).ok() == rustix::process::getpgid(None).ok() {
        return;
    }
    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
}

async fn stop(worker: &WorkerDescriptor) -> io::Result<()> {
    for signal in [rustix::process::Signal::TERM, rustix::process::Signal::KILL] {
        if let Some(pid) = i32::try_from(worker.pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(pid, signal);
        }
        if wait_for_exit(worker, STOP_TIMEOUT).await {
            break;
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
