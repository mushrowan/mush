#![expect(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use mush_daemon::{OpenTarget, SupervisorReply, SupervisorRequest};
use mush_runtime::AgentConnection;

struct Daemon {
    dir: tempfile::TempDir,
    socket: PathBuf,
    child: std::process::Child,
}

impl Daemon {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mush"));
        command.env_clear();
        for (key, value) in env(dir.path()) {
            command.env(key, value);
        }
        let child = command
            .args(["daemon", "--socket"])
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(Instant::now() < deadline, "daemon socket never appeared");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self { dir, socket, child }
    }

    async fn request(&self, request: SupervisorRequest) -> SupervisorReply {
        tokio::time::timeout(
            Duration::from_secs(60),
            mush_daemon::supervisor::request(&self.socket, &request),
        )
        .await
        .expect("the daemon answers")
        .unwrap()
    }

    fn open(&self, target: OpenTarget) -> SupervisorRequest {
        SupervisorRequest::Open {
            target,
            args: ["--no-tools", "--no-session", "--model", "claude-opus-4-7"]
                .into_iter()
                .map(String::from)
                .collect(),
            cwd: self.dir.path().to_path_buf(),
            env: env(self.dir.path()),
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn env(dir: &Path) -> Vec<(String, String)> {
    vec![
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ("HOME".into(), dir.display().to_string()),
        ("XDG_RUNTIME_DIR".into(), dir.display().to_string()),
        (
            "MUSH_DATA_DIR".into(),
            dir.join("data").display().to_string(),
        ),
        (
            "MUSH_CONFIG_DIR".into(),
            dir.join("config/mush").display().to_string(),
        ),
    ]
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[tokio::test]
async fn the_daemon_spawns_lists_reuses_and_stops_workers() {
    let daemon = Daemon::start();

    let reply = daemon.request(daemon.open(OpenTarget::New)).await;
    let SupervisorReply::Worker { descriptor, fresh } = reply else {
        panic!("expected a worker, got {reply:?}")
    };
    assert!(fresh);
    assert!(!descriptor.session_id.is_empty());
    assert_eq!(descriptor.model_id, "claude-opus-4-7");
    assert!(descriptor.socket.exists());
    assert!(alive(descriptor.pid));

    let SupervisorReply::Workers(workers) = daemon.request(SupervisorRequest::List).await else {
        panic!("expected a roster")
    };
    assert_eq!(workers, vec![descriptor.clone()]);

    let SupervisorReply::Worker {
        descriptor: again,
        fresh,
    } = daemon
        .request(daemon.open(OpenTarget::Resume(descriptor.session_id[..8].to_string())))
        .await
    else {
        panic!("expected the same worker")
    };
    assert!(!fresh);
    assert_eq!(again, descriptor);

    let connection = mush_daemon::DaemonConnection::connect(&descriptor.socket)
        .await
        .unwrap();
    assert_eq!(
        connection.state().session_id.to_string(),
        descriptor.session_id
    );
    drop(connection);

    assert!(matches!(
        daemon
            .request(SupervisorRequest::Stop {
                session_id: descriptor.session_id.clone(),
            })
            .await,
        SupervisorReply::Done
    ));
    assert!(!alive(descriptor.pid));
    assert!(!descriptor.socket.exists());
    let SupervisorReply::Workers(workers) = daemon.request(SupervisorRequest::List).await else {
        panic!("expected a roster")
    };
    assert!(workers.is_empty());
}

#[tokio::test]
async fn shutdown_stops_every_worker_and_the_daemon() {
    let mut daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };

    assert!(matches!(
        daemon.request(SupervisorRequest::Shutdown).await,
        SupervisorReply::Done
    ));

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(Instant::now() < deadline, "the daemon kept running");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!alive(descriptor.pid));
    assert!(!daemon.socket.exists());
}

#[tokio::test]
async fn a_worker_that_dies_during_startup_is_reported() {
    let daemon = Daemon::start();
    let SupervisorReply::Error(error) = daemon
        .request(SupervisorRequest::Open {
            target: OpenTarget::New,
            args: vec!["--model".into(), "no-such-model".into()],
            cwd: daemon.dir.path().to_path_buf(),
            env: env(daemon.dir.path()),
        })
        .await
    else {
        panic!("expected an error")
    };
    assert!(error.contains("worker exited during startup"), "{error}");
}

#[tokio::test]
async fn stopping_a_worker_with_a_client_attached_closes_the_client_cleanly() {
    let daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };
    let mut client = mush_daemon::DaemonConnection::connect(&descriptor.socket)
        .await
        .unwrap();

    let stop = daemon.request(SupervisorRequest::Stop {
        session_id: descriptor.session_id.clone(),
    });
    let (stopped, closed) = tokio::join!(
        stop,
        tokio::time::timeout(Duration::from_secs(10), client.next_event())
    );

    assert!(matches!(stopped, SupervisorReply::Done));
    assert!(closed.unwrap().is_none());
    assert!(!alive(descriptor.pid));
    assert!(!descriptor.socket.exists());
    assert!(!descriptor.socket.with_extension("json").exists());
}

#[tokio::test]
async fn status_reports_the_build_and_the_worker_count() {
    let daemon = Daemon::start();
    let SupervisorReply::Status { build, workers } =
        daemon.request(SupervisorRequest::Status).await
    else {
        panic!("expected a status")
    };
    assert_eq!(workers, 0);
    assert!(build.contains('@'));

    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };
    assert_eq!(descriptor.build, build);
    let SupervisorReply::Status { workers, .. } = daemon.request(SupervisorRequest::Status).await
    else {
        panic!("expected a status")
    };
    assert_eq!(workers, 1);
}
