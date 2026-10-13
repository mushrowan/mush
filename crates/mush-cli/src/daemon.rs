use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use color_eyre::eyre::{Result, eyre};
use mush_daemon::{OpenTarget, SupervisorReply, SupervisorRequest, WorkerDescriptor};

use crate::agents::Selected;

const START_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct StaleDaemon {
    pub socket: PathBuf,
    pub busy: Option<usize>,
}

impl std::fmt::Display for StaleDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.busy {
            Some(busy) => write!(
                f,
                "a daemon from another build of mush is running with {busy} busy session(s)"
            ),
            None => write!(
                f,
                "a daemon from another build of mush is running and its sessions could not be listed"
            ),
        }
    }
}

impl std::error::Error for StaleDaemon {}

#[derive(Debug, PartialEq, Eq)]
enum Disposition {
    Absent,
    Current,
    Stopped,
    Busy(Option<usize>),
}

async fn probe(socket: &Path) -> io::Result<Disposition> {
    if !listening(socket) {
        return Ok(Disposition::Absent);
    }
    match mush_daemon::supervisor::request(socket, &SupervisorRequest::Status).await {
        Ok(SupervisorReply::Status { build, .. }) if build == mush_daemon::build_id() => {
            return Ok(Disposition::Current);
        }
        Err(error) if error.kind() == io::ErrorKind::TimedOut => {
            return Err(io::Error::other(format!(
                "the daemon on {} accepted a connection but did not answer, it was left running so no work is lost: {error}",
                socket.display()
            )));
        }
        Ok(_) | Err(_) => {}
    }
    let busy = match mush_daemon::supervisor::request(socket, &SupervisorRequest::List).await {
        Ok(SupervisorReply::Workers(workers)) => workers
            .iter()
            .filter(|worker| {
                worker.failed.is_none()
                    && (worker.turn_active || worker.children.iter().any(|child| child.working))
            })
            .count(),
        Ok(_) | Err(_) => return Ok(Disposition::Busy(None)),
    };
    if busy > 0 {
        return Ok(Disposition::Busy(Some(busy)));
    }
    Ok(stop_stale(socket).await)
}

async fn stop_stale(socket: &Path) -> Disposition {
    let relaunch = mush_daemon::Relaunch::save(&socket.with_file_name("workers"));
    let _ = mush_daemon::supervisor::request(socket, &SupervisorRequest::Shutdown).await;
    if wait_until_gone(socket).await {
        relaunch.restore();
        Disposition::Stopped
    } else {
        Disposition::Busy(None)
    }
}

pub async fn ensure_running() -> Result<PathBuf> {
    let socket = mush_daemon::daemon_socket_path();
    match probe(&socket).await? {
        Disposition::Current => return Ok(socket),
        Disposition::Busy(busy) => return Err(StaleDaemon { socket, busy }.into()),
        Disposition::Absent | Disposition::Stopped => {}
    }
    mush_daemon::election::elect(
        &socket,
        &std::env::current_exe()?,
        &mush_session::data_dir(),
    )
    .await?;
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if listening(&socket) {
            return Ok(socket);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(eyre!("the daemon did not answer on {}", socket.display()));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub async fn take_over(stale: &StaleDaemon) -> Result<()> {
    if !atty::is(atty::Stream::Stdin) {
        return Err(eyre!(
            "{stale}. run mush shutdown to stop it, then try again"
        ));
    }
    if !confirmed(format_args!(
        "{stale}. stopping it interrupts that work. stop it and continue?"
    ))? {
        return Err(eyre!("left the other daemon running"));
    }
    match stop_stale(&stale.socket).await {
        Disposition::Stopped => Ok(()),
        Disposition::Absent | Disposition::Current | Disposition::Busy(_) => Err(eyre!(
            "could not stop the daemon on {}, run mush shutdown and try again",
            stale.socket.display()
        )),
    }
}

pub(crate) fn listening(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

async fn wait_until_gone(socket: &Path) -> bool {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    while listening(socket) {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

pub async fn open(target: OpenTarget, args: Vec<String>) -> Result<WorkerDescriptor> {
    let cwd = match &target {
        OpenTarget::Resume(id) => session_home(id),
        _ => None,
    };
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir()?,
    };
    open_in(target, args, cwd).await
}

async fn open_in(
    target: OpenTarget,
    args: Vec<String>,
    cwd: std::path::PathBuf,
) -> Result<WorkerDescriptor> {
    let socket = ensure_running().await?;
    let request = SupervisorRequest::Open {
        target,
        args,
        cwd,
        env: std::env::vars().collect(),
    };
    match mush_daemon::supervisor::request(&socket, &request).await? {
        SupervisorReply::Worker { descriptor, .. } => Ok(*descriptor),
        SupervisorReply::Error(error) => Err(eyre!("{error}")),
        other => Err(eyre!("unexpected daemon reply: {other:?}")),
    }
}

pub async fn stop(selector: &str, json: bool) -> Result<()> {
    let socket = mush_daemon::daemon_socket_path();
    let supervised = listening(&socket);
    let workers = if supervised {
        crate::agents::live().await?
    } else {
        mush_daemon::supervisor::running_workers(&mush_daemon::workers_dir())
    };
    let Some(target) = crate::agents::selected(&workers, selector)? else {
        if !supervised && !json {
            println!("no daemon running");
            return Ok(());
        }
        return Err(eyre!("no worker for session {selector}"));
    };
    match target {
        Selected::Child(worker, child) => {
            let mut connection = crate::agents::connect(worker).await?;
            mush_runtime::AgentConnection::send(
                &mut connection,
                mush_runtime::ClientCommand::StopChild {
                    session_id: child.session_id.clone(),
                },
            )
            .await?;
        }
        Selected::Root(worker) => {
            if supervised {
                let request = SupervisorRequest::Stop {
                    session_id: worker.session_id.clone(),
                };
                match mush_daemon::supervisor::request(&socket, &request).await? {
                    SupervisorReply::Done => {}
                    other => return Err(reply_error(other)),
                }
            } else {
                mush_daemon::supervisor::stop_unsupervised(worker).await?;
            }
            archive(&worker.session_id)?;
        }
    }
    if json {
        println!("{}", serde_json::json!({ "stopped": target.session_id() }));
    }
    Ok(())
}

fn archive(session_id: &str) -> Result<()> {
    let store = mush_session::SessionStore::new(mush_session::SessionStore::default_dir());
    match store.set_lifecycle(
        &mush_session::SessionId::from(session_id.to_string()),
        mush_session::Lifecycle::Archived,
    ) {
        Ok(()) | Err(mush_session::StoreError::NotFound(_)) => Ok(()),
        Err(error) => Err(eyre!("stopped, but could not archive the session: {error}")),
    }
}

pub async fn shutdown(force: bool, json: bool) -> Result<()> {
    let socket = mush_daemon::daemon_socket_path();
    let stopped = if listening(&socket) {
        let workers = crate::agents::live().await?;
        if !force && !confirm_shutdown(workers.len(), json)? {
            println!("left running");
            return Ok(());
        }
        match mush_daemon::supervisor::request(&socket, &SupervisorRequest::Shutdown).await? {
            SupervisorReply::Done => workers,
            other => return Err(reply_error(other)),
        }
    } else {
        shutdown_unsupervised(force, json).await?
    };
    if json {
        let stopped: Vec<&str> = stopped
            .iter()
            .map(|worker| worker.session_id.as_str())
            .collect();
        println!("{}", serde_json::json!({ "stopped": stopped }));
    }
    Ok(())
}

async fn shutdown_unsupervised(force: bool, json: bool) -> Result<Vec<WorkerDescriptor>> {
    let running = mush_daemon::supervisor::running_workers(&mush_daemon::workers_dir());
    if running.is_empty() {
        if !json {
            println!("no daemon running");
        }
        return Ok(running);
    }
    if !force {
        return Err(eyre!(
            "no daemon answers but {} agent(s) are still running, pass --force to kill them",
            running.len()
        ));
    }
    for worker in &running {
        mush_daemon::supervisor::stop_unsupervised(worker).await?;
    }
    Ok(running)
}

fn confirm_shutdown(workers: usize, json: bool) -> Result<bool> {
    if workers == 0 {
        return Ok(true);
    }
    if json || !atty::is(atty::Stream::Stdin) {
        return Err(eyre!(
            "{workers} agent(s) are running, pass --force to stop them"
        ));
    }
    Ok(confirmed(format_args!("stop {workers} running agent(s)?"))?)
}

fn confirmed(question: std::fmt::Arguments<'_>) -> io::Result<bool> {
    eprint!("{question} [y/N] ");
    std::io::Write::flush(&mut std::io::stderr())?;
    let mut answer = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

pub(crate) fn reply_error(reply: SupervisorReply) -> color_eyre::Report {
    match reply {
        SupervisorReply::Error(error) => eyre!("{error}"),
        other => eyre!("unexpected daemon reply: {other:?}"),
    }
}

pub async fn send(session: &str, text: &str) -> Result<()> {
    use mush_agent::ipc::control::{Event, Operation, Reply, Request, Target};
    use mush_agent::{IpcMessage, IpcMessageKind};

    let control_dir = mush_agent::ipc::runtime_dir().join("control");
    let sockets: Vec<PathBuf> = std::fs::read_dir(&control_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sock"))
        .collect();
    let mut found: Option<(PathBuf, Target)> = None;
    for socket in sockets {
        let Ok(response) = mush_agent::ipc::listener::exchange(
            &socket,
            &IpcMessage {
                id: mush_agent::ipc::control::new_id(),
                from: "mush-send".into(),
                kind: IpcMessageKind::GetTarget,
            },
        )
        .await
        else {
            continue;
        };
        let IpcMessageKind::Target(target) = response.kind else {
            continue;
        };
        if target.session.starts_with(session) {
            if found.is_some() {
                return Err(eyre!("{session} matches more than one running session"));
            }
            found = Some((socket, target));
        }
    }
    let Some((socket, target)) = found else {
        return Err(eyre!("no running session matches {session}"));
    };
    let response = mush_agent::ipc::listener::exchange(
        &socket,
        &IpcMessage {
            id: mush_agent::ipc::control::new_id(),
            from: "mush-send".into(),
            kind: IpcMessageKind::Control(Request {
                target: target.clone(),
                operation: Operation::Send {
                    event: Event::Message {
                        text: text.to_string(),
                    },
                },
            }),
        },
    )
    .await?;
    match response.kind {
        IpcMessageKind::ControlReply(Reply::Accepted { .. }) => {
            println!("queued for {}", crate::output::short_id(&target.session));
            Ok(())
        }
        IpcMessageKind::ControlReply(Reply::Error { code }) => Err(eyre!("{code}")),
        other => Err(eyre!("unexpected reply: {other:?}")),
    }
}

fn session_home(id: &str) -> Option<std::path::PathBuf> {
    let store = mush_session::SessionStore::new(mush_session::SessionStore::default_dir());
    mush_session::place::local_place(&store.resolve(id).ok()?.cwd)
}

pub fn worker_opener(worker_args: Vec<String>) -> mush_tui::WorkerOpener {
    std::sync::Arc::new(move |target: mush_tui::WorkerTarget| {
        let args = worker_args.clone();
        Box::pin(async move {
            let target = match target {
                mush_tui::WorkerTarget::New => OpenTarget::New,
                mush_tui::WorkerTarget::Resume(id) => OpenTarget::Resume(id.to_string()),
            };
            let descriptor = open(target, args)
                .await
                .map_err(|error| error.to_string())?;
            let connection = mush_daemon::DaemonConnection::connect(&descriptor.socket)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Box::new(connection) as Box<dyn mush_runtime::AgentConnection>)
        })
    })
}

pub fn session_spawner(worker_args: Vec<String>) -> mush_runtime::SessionSpawner {
    std::sync::Arc::new(move |request: mush_runtime::CreateSessionRequest| {
        let mut args = worker_args.clone();
        Box::pin(async move {
            if let Some(model) = &request.model {
                args = with_model(args, model);
            }
            let cwd = match request.cwd {
                Some(cwd) => {
                    let cwd = std::env::current_dir()
                        .map_err(|error| error.to_string())?
                        .join(cwd);
                    if !cwd.is_dir() {
                        return Err(format!("cwd {} is not a directory", cwd.display()));
                    }
                    cwd
                }
                None => std::env::current_dir().map_err(|error| error.to_string())?,
            };
            let descriptor = open_in(OpenTarget::New, args, cwd)
                .await
                .map_err(|error| error.to_string())?;
            let mut connection = mush_daemon::DaemonConnection::connect(&descriptor.socket)
                .await
                .map_err(|error| error.to_string())?;
            use mush_runtime::AgentConnection;
            if let Some(name) = &request.name {
                connection
                    .send(mush_runtime::ClientCommand::Session(
                        mush_runtime::SessionCommand::Name {
                            title: Some(name.clone()),
                        },
                    ))
                    .await
                    .map_err(|error| error.to_string())?;
            }
            if let Some(level) = request.thinking {
                connection
                    .send(mush_runtime::ClientCommand::SetThinking(level))
                    .await
                    .map_err(|error| error.to_string())?;
            }
            connection
                .send(mush_runtime::ClientCommand::Prompt {
                    text: request.prompt,
                    images: Vec::new(),
                })
                .await
                .map_err(|error| error.to_string())?;
            Ok(mush_runtime::CreatedSession {
                session_id: descriptor.session_id,
                name: request.name,
                model: descriptor.model_id,
            })
        })
    })
}

fn with_model(args: Vec<String>, model: &str) -> Vec<String> {
    let mut kept = Vec::with_capacity(args.len() + 2);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--model" {
            args.next();
            continue;
        }
        kept.push(arg);
    }
    kept.push("--model".into());
    kept.push(model.into());
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(turn_active: bool, failed: bool) -> WorkerDescriptor {
        WorkerDescriptor {
            session_id: "abc".into(),
            build: "other".into(),
            pid: 1,
            turn_active,
            failed: failed.then(|| "gone".to_string()),
            ..WorkerDescriptor::default()
        }
    }

    fn fake_daemon(
        socket: PathBuf,
        build: String,
        list: Option<Vec<WorkerDescriptor>>,
    ) -> tokio::task::JoinHandle<Vec<String>> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let mut seen = Vec::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let Ok(Some(line)) = tokio::io::BufReader::new(reader).lines().next_line().await
                else {
                    continue;
                };
                let request: SupervisorRequest = serde_json::from_str(&line).unwrap();
                let reply = match &request {
                    SupervisorRequest::Status => serde_json::to_string(&SupervisorReply::Status {
                        build: build.clone(),
                        workers: 1,
                    })
                    .unwrap(),
                    SupervisorRequest::List => match &list {
                        Some(workers) => {
                            serde_json::to_string(&SupervisorReply::Workers(workers.clone()))
                                .unwrap()
                        }
                        None => "{\"type\":\"something_new\"}".into(),
                    },
                    other => {
                        seen.push(format!("{other:?}"));
                        serde_json::to_string(&SupervisorReply::Done).unwrap()
                    }
                };
                let _ = writer.write_all(format!("{reply}\n").as_bytes()).await;
                if matches!(request, SupervisorRequest::Shutdown) {
                    let _ = std::fs::remove_dir_all(socket.with_file_name("workers"));
                    drop(listener);
                    let _ = std::fs::remove_file(&socket);
                    return seen;
                }
            }
        })
    }

    #[tokio::test]
    async fn no_daemon_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            probe(&dir.path().join("daemon.sock")).await.unwrap(),
            Disposition::Absent
        );
    }

    #[tokio::test]
    async fn a_daemon_of_this_build_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let fake = fake_daemon(socket.clone(), mush_daemon::build_id(), Some(Vec::new()));
        assert_eq!(probe(&socket).await.unwrap(), Disposition::Current);
        assert!(listening(&socket));
        fake.abort();
    }

    #[tokio::test]
    async fn an_idle_daemon_of_another_build_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let workers = vec![descriptor(false, false), descriptor(true, true)];
        let fake = fake_daemon(socket.clone(), "other".into(), Some(workers));
        assert_eq!(probe(&socket).await.unwrap(), Disposition::Stopped);
        assert_eq!(fake.await.unwrap(), ["Shutdown"]);
        assert!(!listening(&socket));
    }

    #[tokio::test]
    async fn replacing_a_daemon_of_another_build_keeps_its_sessions_for_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let workers = dir.path().join("workers");
        std::fs::create_dir_all(&workers).unwrap();
        let pid = std::process::id();
        let live = WorkerDescriptor {
            pid,
            start_id: mush_tools::orphans::process_start_id(pid),
            ..descriptor(false, false)
        };
        let described = serde_json::to_vec(&live).unwrap();
        std::fs::write(workers.join("w.json"), &described).unwrap();
        std::fs::write(workers.join("w.recipe"), b"recipe").unwrap();
        let fake = fake_daemon(socket.clone(), "other".into(), Some(vec![live]));

        assert_eq!(probe(&socket).await.unwrap(), Disposition::Stopped);

        assert_eq!(fake.await.unwrap(), ["Shutdown"]);
        assert_eq!(std::fs::read(workers.join("w.json")).unwrap(), described);
        assert_eq!(std::fs::read(workers.join("w.recipe")).unwrap(), b"recipe");
    }

    #[tokio::test]
    async fn a_busy_daemon_of_another_build_is_left_running() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let workers = vec![descriptor(true, false), descriptor(false, false)];
        let fake = fake_daemon(socket.clone(), "other".into(), Some(workers));
        assert_eq!(probe(&socket).await.unwrap(), Disposition::Busy(Some(1)));
        assert!(listening(&socket));
        fake.abort();
    }

    #[tokio::test]
    async fn a_daemon_of_another_build_with_working_children_is_left_running() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let waiting_parent = WorkerDescriptor {
            children: vec![mush_daemon::ChildDescriptor {
                working: true,
                ..mush_daemon::ChildDescriptor::default()
            }],
            ..descriptor(false, false)
        };
        let fake = fake_daemon(socket.clone(), "other".into(), Some(vec![waiting_parent]));
        assert_eq!(probe(&socket).await.unwrap(), Disposition::Busy(Some(1)));
        assert!(listening(&socket));
        fake.abort();
    }

    #[tokio::test]
    async fn a_daemon_whose_sessions_cannot_be_listed_counts_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let fake = fake_daemon(socket.clone(), "other".into(), None);
        assert_eq!(probe(&socket).await.unwrap(), Disposition::Busy(None));
        assert!(listening(&socket));
        fake.abort();
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_new_model_replaces_the_flag_and_its_value() {
        let args = strings(&["--thinking", "--model", "old", "--max-turns", "4"]);
        assert_eq!(
            with_model(args, "new"),
            strings(&["--thinking", "--max-turns", "4", "--model", "new"])
        );
    }

    #[test]
    fn a_model_is_added_when_none_was_set() {
        assert_eq!(
            with_model(strings(&["--no-tools"]), "new"),
            strings(&["--no-tools", "--model", "new"])
        );
    }
}
