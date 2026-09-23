use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::client::{CONNECT_TIMEOUT, REQUEST_TIMEOUT};
use crate::protocol::{decode, encode};

const SPAWN_TIMEOUT: Duration = Duration::from_secs(60);
const OPEN_TIMEOUT: Duration = SPAWN_TIMEOUT.saturating_add(Duration::from_secs(5));
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const ADOPTION_POLL: Duration = Duration::from_millis(500);
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
    pub paused: bool,
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

#[derive(Clone)]
pub struct Supervisor {
    nursery: std::sync::Arc<Nursery>,
}

struct Nursery {
    program: PathBuf,
    socket: PathBuf,
    workers_dir: PathBuf,
    log_dir: PathBuf,
    residents: std::sync::Mutex<std::collections::HashMap<String, Resident>>,
}

#[derive(Clone, Serialize, Deserialize)]
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
    pub fn new(program: PathBuf, socket: PathBuf, workers_dir: PathBuf, log_dir: PathBuf) -> Self {
        Self {
            nursery: std::sync::Arc::new(Nursery {
                program,
                socket,
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
                let worker = match select(self.roster(), &session_id) {
                    Ok(Some(worker)) => worker,
                    Ok(None) => {
                        return SupervisorReply::Error(format!(
                            "no worker for session {session_id}"
                        ));
                    }
                    Err(error) => return SupervisorReply::Error(error),
                };
                match self.nursery.stop(&worker).await {
                    Ok(()) => SupervisorReply::Done,
                    Err(error) => SupervisorReply::Error(error.to_string()),
                }
            }
            SupervisorRequest::Shutdown => {
                let mut stopping = tokio::task::JoinSet::new();
                for worker in self.roster() {
                    let nursery = std::sync::Arc::clone(&self.nursery);
                    stopping.spawn(async move {
                        if let Err(error) = nursery.stop(&worker).await {
                            tracing::warn!(%error, session = %worker.session_id, "worker did not stop");
                        }
                    });
                }
                stopping.join_all().await;
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
        if let OpenTarget::Resume(id) = &target {
            let running = self
                .roster()
                .into_iter()
                .filter(|worker| worker.failed.is_none());
            if let Some(worker) = select(running, id).map_err(io::Error::other)? {
                return Ok(SupervisorReply::Worker {
                    descriptor: Box::new(worker),
                    fresh: false,
                });
            }
        }
        let name = mush_ai::types::SessionId::new().to_string();
        let descriptor = self
            .nursery
            .clone()
            .start(&name, &target, &args, &cwd, &env, false)
            .await?;
        self.nursery.forget_failed(&descriptor.session_id);
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

    fn recipe_path(&self, name: &str) -> PathBuf {
        self.workers_dir.join(format!("{name}.recipe"))
    }

    fn write_recipe(&self, name: &str, recipe: &Recipe) {
        if let Ok(bytes) = serde_json::to_vec(recipe) {
            let _ = mush_ai::private_io::write_private(&self.recipe_path(name), bytes);
        }
    }

    fn read_descriptor(&self, name: &str) -> Option<WorkerDescriptor> {
        let bytes = std::fs::read(self.descriptor_path(name)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn read_recipe(&self, name: &str) -> Option<Recipe> {
        let bytes = std::fs::read(self.recipe_path(name)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn adopt(self: &std::sync::Arc<Self>) {
        for (name, descriptor) in self.descriptors() {
            if self.residents().contains_key(&name) || descriptor.failed.is_some() {
                continue;
            }
            match (descriptor.alive(), self.read_recipe(&name)) {
                (true, Some(recipe)) => {
                    tracing::info!(name, pid = descriptor.pid, "adopted a running worker");
                    self.remember(&name, recipe, descriptor.pid);
                    let nursery = std::sync::Arc::clone(self);
                    tokio::spawn(async move {
                        nursery.watch_adopted(name, descriptor).await;
                    });
                }
                (false, Some(mut recipe)) => {
                    tracing::warn!(name, "a worker died while no daemon was running");
                    if !descriptor.session_id.is_empty() {
                        recipe.session_id.clone_from(&descriptor.session_id);
                    }
                    let _ = std::fs::remove_file(&descriptor.socket);
                    self.remember(&name, recipe.clone(), descriptor.pid);
                    let nursery = std::sync::Arc::clone(self);
                    tokio::spawn(async move {
                        nursery
                            .recover(
                                name,
                                recipe,
                                "no daemon was watching".into(),
                                descriptor.turn_active,
                            )
                            .await;
                    });
                }
                (false, None) => self.forget(&name, &descriptor),
                (true, None) => {}
            }
        }
    }

    async fn watch_adopted(self: std::sync::Arc<Self>, name: String, descriptor: WorkerDescriptor) {
        while descriptor.alive() {
            tokio::time::sleep(ADOPTION_POLL).await;
            let known = {
                let residents = self.residents();
                residents
                    .get(&name)
                    .is_some_and(|resident| !resident.stopping)
            };
            if !known {
                return;
            }
        }
        if !self.descriptor_path(&name).exists() {
            self.residents().remove(&name);
            return;
        }
        self.worker_exited(&name, Err(io::Error::other("an adopted worker vanished")))
            .await;
    }

    fn residents(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Resident>> {
        self.residents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn remember(&self, name: &str, recipe: Recipe, pid: u32) {
        self.write_recipe(name, &recipe);
        self.residents().insert(
            name.to_string(),
            Resident {
                recipe,
                pid,
                stopping: false,
            },
        );
    }

    fn descriptors(&self) -> Vec<(String, WorkerDescriptor)> {
        let Ok(entries) = std::fs::read_dir(&self.workers_dir) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for path in entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        {
            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string);
            let descriptor = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<WorkerDescriptor>(&bytes).ok());
            match (name, descriptor) {
                (Some(name), Some(descriptor)) => found.push((name, descriptor)),
                _ => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        found.sort_by_key(|(_, worker)| worker.started_at);
        found
    }

    fn roster(&self) -> Vec<WorkerDescriptor> {
        self.descriptors()
            .into_iter()
            .map(|(_, descriptor)| descriptor)
            .filter(|descriptor| descriptor.alive() || descriptor.failed.is_some())
            .collect()
    }

    fn forget(&self, name: &str, descriptor: &WorkerDescriptor) {
        let _ = std::fs::remove_file(self.descriptor_path(name));
        let _ = std::fs::remove_file(self.recipe_path(name));
        let _ = std::fs::remove_file(&descriptor.socket);
    }

    fn forget_failed(&self, session_id: &str) {
        for (name, descriptor) in self.descriptors() {
            if descriptor.failed.is_some() && descriptor.session_id == session_id {
                self.forget(&name, &descriptor);
            }
        }
    }

    fn follow(&self, name: &str, mut recipe: Recipe, last: Option<&WorkerDescriptor>) -> Recipe {
        let Some(current) = last
            .map(|descriptor| &descriptor.session_id)
            .filter(|id| !id.is_empty() && **id != recipe.session_id)
        else {
            return recipe;
        };
        recipe.session_id.clone_from(current);
        self.write_recipe(name, &recipe);
        if let Some(resident) = self.residents().get_mut(name) {
            resident.recipe = recipe.clone();
        }
        recipe
    }

    async fn stop(&self, worker: &WorkerDescriptor) -> io::Result<()> {
        if let Some(name) = worker.socket.file_stem().and_then(|stem| stem.to_str()) {
            if let Some(resident) = self.residents().get_mut(name) {
                resident.stopping = true;
            }
            let _ = std::fs::remove_file(self.recipe_path(name));
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
        interrupted: bool,
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
        if interrupted {
            command.arg("--recovered");
        }
        command
            .arg("worker")
            .arg("--socket")
            .arg(&socket)
            .arg("--descriptor")
            .arg(&descriptor_path)
            .arg("--daemon-socket")
            .arg(&self.socket)
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
                    let _ = std::fs::remove_file(self.recipe_path(name));
                    None
                }
                Some(resident) => Some((resident.recipe.clone(), resident.pid)),
                None => None,
            }
        }) else {
            return;
        };
        if matches!(&status, Ok(status) if status.success()) {
            let _ = std::fs::remove_file(self.recipe_path(name));
            self.residents().remove(name);
            return;
        }
        let reason = match &status {
            Ok(status) => status.to_string(),
            Err(error) => error.to_string(),
        };
        tracing::warn!(name, %reason, "worker died, recovering");
        reap_group(pid);
        let last = self.read_descriptor(name);
        let interrupted = last
            .as_ref()
            .is_some_and(|descriptor| descriptor.turn_active);
        let recipe = self.follow(name, recipe, last.as_ref());
        self.recover(name.to_string(), recipe, reason, interrupted)
            .await;
    }

    fn recover(
        self: std::sync::Arc<Self>,
        name: String,
        recipe: Recipe,
        died: String,
        interrupted: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            self.recover_inner(&name, &recipe, &died, interrupted).await;
        })
    }

    async fn recover_inner(
        self: std::sync::Arc<Self>,
        name: &str,
        recipe: &Recipe,
        died: &str,
        interrupted: bool,
    ) {
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
                    interrupted,
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
        let _ = std::fs::remove_file(self.recipe_path(name));
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
            paused: false,
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

fn select(
    workers: impl IntoIterator<Item = WorkerDescriptor>,
    selector: &str,
) -> Result<Option<WorkerDescriptor>, String> {
    let mut found = workers
        .into_iter()
        .filter(|worker| worker.session_id.starts_with(selector));
    let first = found.next();
    if found.next().is_some() {
        return Err(format!("{selector} matches more than one agent"));
    }
    Ok(first)
}

#[must_use]
pub fn running_workers(dir: &Path) -> Vec<WorkerDescriptor> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut running: Vec<WorkerDescriptor> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| serde_json::from_slice(&std::fs::read(path).ok()?).ok())
        .filter(WorkerDescriptor::alive)
        .collect();
    running.sort_by_key(|worker| worker.started_at);
    running
}

pub async fn stop_unsupervised(worker: &WorkerDescriptor) -> io::Result<()> {
    let _ = std::fs::remove_file(worker.socket.with_extension("recipe"));
    stop(worker).await?;
    let _ = std::fs::remove_file(descriptor_of(worker));
    Ok(())
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
    supervisor.nursery.adopt();
    let result = serve(&listener, &supervisor).await;
    let _ = std::fs::remove_file(socket);
    result
}

async fn serve(listener: &UnixListener, supervisor: &Supervisor) -> io::Result<()> {
    let shutdown = tokio_util::sync::CancellationToken::new();
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => accepted?.0,
            () = shutdown.cancelled() => return Ok(()),
        };
        let supervisor = supervisor.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if serve_one(stream, &supervisor).await {
                shutdown.cancel();
            }
        });
    }
}

async fn serve_one(stream: UnixStream, supervisor: &Supervisor) -> bool {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let Ok(Ok(Some(line))) = tokio::time::timeout(CONNECT_TIMEOUT, lines.next_line()).await else {
        return false;
    };
    let (reply, shutdown) = match decode::<SupervisorRequest>(&line) {
        Ok(request) => {
            let shutdown = matches!(request, SupervisorRequest::Shutdown);
            (supervisor.handle(request).await, shutdown)
        }
        Err(error) => (
            SupervisorReply::Error(format!("malformed request: {error}")),
            false,
        ),
    };
    if let Ok(bytes) = encode(&reply) {
        let _ = writer.write_all(&bytes).await;
    }
    shutdown
}

pub async fn request(socket: &Path, request: &SupervisorRequest) -> io::Result<SupervisorReply> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(socket))
        .await
        .map_err(|_| timed_out("connect", CONNECT_TIMEOUT))??;
    let bound = match request {
        SupervisorRequest::Open { .. } => OPEN_TIMEOUT,
        SupervisorRequest::List
        | SupervisorRequest::Status
        | SupervisorRequest::Stop { .. }
        | SupervisorRequest::Shutdown => REQUEST_TIMEOUT,
    };
    tokio::time::timeout(bound, exchange(stream, request))
        .await
        .map_err(|_| timed_out("answer", bound))?
}

fn timed_out(what: &str, after: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("the daemon did not {what} within {after:?}"),
    )
}

async fn exchange(stream: UnixStream, request: &SupervisorRequest) -> io::Result<SupervisorReply> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(session_id: &str) -> WorkerDescriptor {
        WorkerDescriptor {
            session_id: session_id.into(),
            model_id: String::new(),
            build: String::new(),
            pid: 0,
            start_id: None,
            cwd: PathBuf::new(),
            socket: PathBuf::new(),
            started_at: 0,
            title: None,
            turn_active: false,
            paused: false,
            messages: 0,
            updated_at: 0,
            clients: 0,
            failed: None,
        }
    }

    #[tokio::test]
    async fn a_silent_client_does_not_hold_up_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let supervisor = Supervisor::new(
            dir.path().join("no-such-mush"),
            socket.clone(),
            dir.path().join("workers"),
            dir.path().join("logs"),
        );
        let running = tokio::spawn({
            let socket = socket.clone();
            async move { run(&socket, supervisor).await }
        });
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let _silent = UnixStream::connect(&socket).await.unwrap();
        let reply = tokio::time::timeout(
            Duration::from_secs(1),
            request(&socket, &SupervisorRequest::Status),
        )
        .await
        .expect("the daemon answers while another client is silent")
        .unwrap();
        assert!(matches!(reply, SupervisorReply::Status { workers: 0, .. }));

        let reply = request(&socket, &SupervisorRequest::Shutdown)
            .await
            .unwrap();
        assert!(matches!(reply, SupervisorReply::Done));
        running.await.unwrap().unwrap();
    }

    #[test]
    fn a_selector_picks_the_one_worker_it_prefixes() {
        let workers = [worker("abc1"), worker("def2")];
        let found = select(workers, "ab").unwrap().unwrap();
        assert_eq!(found.session_id, "abc1");
    }

    #[test]
    fn an_ambiguous_selector_is_an_error() {
        let workers = [worker("abc1"), worker("abd2")];
        let error = select(workers, "ab").unwrap_err();
        assert!(error.contains("more than one"), "{error}");
    }

    #[test]
    fn an_unknown_selector_finds_nothing() {
        assert!(select([worker("abc1")], "x").unwrap().is_none());
    }
}
