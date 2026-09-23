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
        let socket = dir.path().join("mush").join("daemon.sock");
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

    fn open_saved(&self, target: OpenTarget) -> SupervisorRequest {
        let SupervisorRequest::Open {
            target,
            args,
            cwd,
            env,
        } = self.open(target)
        else {
            unreachable!()
        };
        SupervisorRequest::Open {
            target,
            args: args
                .into_iter()
                .filter(|arg| arg != "--no-session")
                .collect(),
            cwd,
            env,
        }
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

    fn cli(&self) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mush"));
        command.env_clear();
        for (key, value) in env(self.dir.path()) {
            command.env(key, value);
        }
        command.current_dir(self.dir.path()).stdin(Stdio::null());
        command
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let socket = self.socket.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let _ = tokio::time::timeout(
                    Duration::from_secs(10),
                    mush_daemon::supervisor::request(&socket, &SupervisorRequest::Shutdown),
                )
                .await;
            });
        })
        .join()
        .ok();
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
    assert_eq!(workers, vec![*descriptor.clone()]);

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

    tokio::time::sleep(Duration::from_secs(2)).await;
    let SupervisorReply::Workers(workers) = daemon.request(SupervisorRequest::List).await else {
        panic!("expected a roster")
    };
    assert!(
        workers.is_empty(),
        "a stopped worker came back: {workers:?}"
    );
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

#[tokio::test]
async fn mush_send_queues_a_message_for_a_running_session() {
    let daemon = Daemon::start();
    let config = daemon.dir.path().join("config/mush");
    std::fs::create_dir_all(&config).unwrap();
    let model = mush_ai::types::Model::builder()
        .id("gpt-send-fixture".into())
        .name("fixture".into())
        .provider(mush_ai::types::Provider::Custom("fixture".into()))
        .api(mush_ai::types::Api::OpenaiResponses)
        .base_url("http://127.0.0.1:9".into())
        .context_window(mush_ai::types::TokenCount::new(100_000))
        .max_output_tokens(mush_ai::types::TokenCount::new(64))
        .build();
    std::fs::write(
        config.join("models.json"),
        serde_json::to_vec(&[model]).unwrap(),
    )
    .unwrap();
    std::fs::write(
        config.join("config.toml"),
        "[api_keys]\nfixture = 'fixture-key'\n",
    )
    .unwrap();
    let SupervisorReply::Worker { descriptor, .. } = daemon
        .request(SupervisorRequest::Open {
            target: OpenTarget::New,
            args: ["--no-tools", "--model", "gpt-send-fixture"]
                .into_iter()
                .map(String::from)
                .collect(),
            cwd: daemon.dir.path().to_path_buf(),
            env: env(daemon.dir.path()),
        })
        .await
    else {
        panic!("expected a worker")
    };

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mush"));
    command.env_clear();
    for (key, value) in env(daemon.dir.path()) {
        command.env(key, value);
    }
    let sent = command
        .args(["send", &descriptor.session_id[..8], "hello from the shell"])
        .output()
        .unwrap();
    assert!(
        sent.status.success(),
        "{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&sent.stdout).trim(),
        format!("queued for {}", &descriptor.session_id[..8])
    );

    let mut connection = mush_daemon::DaemonConnection::connect(&descriptor.socket)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let history = connection.history().await;
        if history.iter().any(|message| {
            matches!(message, mush_ai::types::Message::Event(event) if event.content.contains("hello from the shell"))
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the message never reached the session: {history:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mush"));
    command.env_clear();
    for (key, value) in env(daemon.dir.path()) {
        command.env(key, value);
    }
    let missing = command.args(["send", "nope", "hello"]).output().unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no running session matches nope"));
}

#[tokio::test]
async fn a_session_owned_by_a_worker_refuses_a_second_opener() {
    let daemon = Daemon::start();
    let config = daemon.dir.path().join("config/mush");
    std::fs::create_dir_all(&config).unwrap();
    let model = mush_ai::types::Model::builder()
        .id("gpt-lease-fixture".into())
        .name("fixture".into())
        .provider(mush_ai::types::Provider::Custom("fixture".into()))
        .api(mush_ai::types::Api::OpenaiResponses)
        .base_url("http://127.0.0.1:9".into())
        .context_window(mush_ai::types::TokenCount::new(100_000))
        .max_output_tokens(mush_ai::types::TokenCount::new(64))
        .build();
    std::fs::write(
        config.join("models.json"),
        serde_json::to_vec(&[model]).unwrap(),
    )
    .unwrap();
    std::fs::write(
        config.join("config.toml"),
        "rlm = false\n[api_keys]\nfixture = 'fixture-key'\n",
    )
    .unwrap();
    let SupervisorReply::Worker { descriptor, .. } = daemon
        .request(SupervisorRequest::Open {
            target: OpenTarget::New,
            args: ["--no-tools", "--model", "gpt-lease-fixture"]
                .into_iter()
                .map(String::from)
                .collect(),
            cwd: daemon.dir.path().to_path_buf(),
            env: env(daemon.dir.path()),
        })
        .await
    else {
        panic!("expected a worker")
    };
    let store = mush_session::SessionStore::new(daemon.dir.path().join("data/sessions"));
    let id = mush_ai::types::SessionId::from(descriptor.session_id.as_str());
    assert!(store.is_leased(&id));
    let mut placeholder = mush_session::Session::new(
        "gpt-lease-fixture",
        &daemon.dir.path().display().to_string(),
    );
    placeholder.meta.id = id.clone();
    store.save(&placeholder).unwrap();

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mush"));
    command.env_clear();
    for (key, value) in env(daemon.dir.path()) {
        command.env(key, value);
    }
    let refused = command
        .args([
            "--no-tools",
            "--model",
            "gpt-lease-fixture",
            "--resume",
            &descriptor.session_id,
            "-p",
            "hello",
        ])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("is open in another mush"), "{stderr}");

    assert!(matches!(
        daemon
            .request(SupervisorRequest::Stop {
                session_id: descriptor.session_id.clone(),
            })
            .await,
        SupervisorReply::Done
    ));
    assert!(!store.is_leased(&id));
}

#[tokio::test]
async fn the_cli_lists_and_renames_a_live_agent() {
    let daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };

    let renamed = daemon
        .cli()
        .args(["rename", &descriptor.session_id[..8], "scout", "one"])
        .output()
        .unwrap();
    assert!(renamed.status.success(), "{renamed:?}");
    assert!(String::from_utf8_lossy(&renamed.stdout).contains("renamed"));

    let listed = daemon.cli().args(["list", "--json"]).output().unwrap();
    assert!(listed.status.success(), "{listed:?}");
    let rows: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(rows[0]["name"], "scout one");
    assert_eq!(rows[0]["id"], descriptor.session_id[..8]);
    assert_eq!(rows[0]["status"], "idle");
}

#[tokio::test]
async fn doctor_reports_a_live_worker_and_clears_a_dead_one() {
    let daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };

    let workers = daemon.dir.path().join("mush").join("workers");
    let dead = mush_daemon::WorkerDescriptor {
        session_id: "deadbeefdeadbeef".into(),
        pid: 0x7fff_fffe,
        start_id: Some("proc:never".into()),
        socket: workers.join("deadbeefdeadbeef.sock"),
        ..*descriptor.clone()
    };
    std::fs::write(
        workers.join("deadbeefdeadbeef.json"),
        serde_json::to_vec(&dead).unwrap(),
    )
    .unwrap();
    std::fs::write(&dead.socket, b"").unwrap();

    let looked = daemon
        .cli()
        .args(["doctor", "--fix", "--json"])
        .output()
        .unwrap();
    assert!(looked.status.success(), "{looked:?}");
    let findings: serde_json::Value = serde_json::from_slice(&looked.stdout).unwrap();
    let by = |what: &str| -> serde_json::Value {
        findings
            .as_array()
            .unwrap()
            .iter()
            .find(|finding| finding["what"] == what)
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(by("daemon")["health"], "ok");
    assert_eq!(
        by(&format!("worker {}", &descriptor.session_id[..8]))["health"],
        "ok"
    );
    assert_eq!(by("worker deadbeef")["health"], "stale");
    assert_eq!(by("worker deadbeef")["fixed"], true);
    assert!(!workers.join("deadbeefdeadbeef.json").exists());
    assert!(!dead.socket.exists());
}

#[tokio::test]
async fn shutdown_needs_a_force_flag_when_agents_are_running() {
    let daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };

    let refused = daemon.cli().arg("shutdown").output().unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("pass --force"),
        "{refused:?}"
    );
    assert!(alive(descriptor.pid));

    let forced = daemon.cli().args(["shutdown", "--force"]).output().unwrap();
    assert!(forced.status.success(), "{forced:?}");
    assert!(!alive(descriptor.pid));
}

#[tokio::test]
async fn a_killed_worker_comes_back_on_the_same_socket() {
    let daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open_saved(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };
    assert!(alive(descriptor.pid));

    let pid = rustix::process::Pid::from_raw(i32::try_from(descriptor.pid).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let recovered = loop {
        let SupervisorReply::Workers(workers) = daemon.request(SupervisorRequest::List).await
        else {
            panic!("expected a roster")
        };
        if let Some(worker) = workers
            .iter()
            .find(|worker| worker.pid != descriptor.pid && worker.failed.is_none())
        {
            break worker.clone();
        }
        assert!(Instant::now() < deadline, "the worker never came back");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    assert_eq!(recovered.socket, descriptor.socket);
    assert_eq!(recovered.session_id, descriptor.session_id);
    assert!(alive(recovered.pid));

    let mut client = mush_daemon::DaemonConnection::connect(&recovered.socket)
        .await
        .unwrap();
    let transcript = client.transcript().await;
    assert!(
        transcript.iter().any(|message| matches!(
            message,
            mush_ai::types::Message::Event(event)
                if event.kind == mush_ai::types::EventKind::Recovery
        )),
        "{transcript:?}"
    );
}

#[tokio::test]
async fn a_worker_that_cannot_restart_is_marked_failed() {
    let daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };

    let pid = rustix::process::Pid::from_raw(i32::try_from(descriptor.pid).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    let failed = loop {
        let SupervisorReply::Workers(workers) = daemon.request(SupervisorRequest::List).await
        else {
            panic!("expected a roster")
        };
        if let Some(worker) = workers.iter().find(|worker| worker.failed.is_some()) {
            break worker.clone();
        }
        assert!(
            Instant::now() < deadline,
            "the worker was never marked failed"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(failed.session_id, descriptor.session_id);
    assert_eq!(failed.pid, 0);

    let listed = daemon.cli().args(["list", "--json"]).output().unwrap();
    let rows: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(rows[0]["status"], "failed");

    let stopped = daemon
        .cli()
        .args(["stop", &descriptor.session_id[..8]])
        .output()
        .unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    let SupervisorReply::Workers(workers) = daemon.request(SupervisorRequest::List).await else {
        panic!("expected a roster")
    };
    assert!(workers.is_empty());
}

#[tokio::test]
async fn an_ambiguous_selector_stops_nothing() {
    let daemon = Daemon::start();
    let mut pids = Vec::new();
    for _ in 0..2 {
        let SupervisorReply::Worker { descriptor, .. } =
            daemon.request(daemon.open(OpenTarget::New)).await
        else {
            panic!("expected a worker")
        };
        pids.push(descriptor.pid);
    }

    let SupervisorReply::Error(error) = daemon
        .request(SupervisorRequest::Stop {
            session_id: String::new(),
        })
        .await
    else {
        panic!("expected an error")
    };
    assert!(error.contains("more than one"), "{error}");
    let SupervisorReply::Error(error) = daemon
        .request(daemon.open(OpenTarget::Resume(String::new())))
        .await
    else {
        panic!("expected an error")
    };
    assert!(error.contains("more than one"), "{error}");
    assert!(pids.into_iter().all(alive));
}

#[tokio::test]
async fn resuming_a_failed_agent_starts_a_fresh_worker() {
    let daemon = Daemon::start();
    let store = mush_session::SessionStore::new(daemon.dir.path().join("data/sessions"));
    let session =
        mush_session::Session::new("claude-opus-4-7", &daemon.dir.path().display().to_string());
    store.save(&session).unwrap();
    let id = session.meta.id.to_string();
    let workers = daemon.dir.path().join("mush").join("workers");
    std::fs::create_dir_all(&workers).unwrap();
    let failed = mush_daemon::WorkerDescriptor {
        session_id: id.clone(),
        model_id: String::new(),
        build: mush_daemon::build_id(),
        pid: 0,
        start_id: None,
        cwd: daemon.dir.path().to_path_buf(),
        socket: workers.join("gone.sock"),
        started_at: 1,
        title: None,
        turn_active: false,
        paused: false,
        messages: 0,
        updated_at: 1,
        clients: 0,
        failed: Some("gave up".into()),
    };
    std::fs::write(
        workers.join("gone.json"),
        serde_json::to_vec(&failed).unwrap(),
    )
    .unwrap();

    let reply = daemon
        .request(daemon.open_saved(OpenTarget::Resume(id[..8].to_string())))
        .await;
    let SupervisorReply::Worker { descriptor, fresh } = reply else {
        panic!("expected a worker, got {reply:?}")
    };
    assert!(fresh);
    assert_eq!(descriptor.session_id, id);
    assert!(alive(descriptor.pid));
    mush_daemon::DaemonConnection::connect(&descriptor.socket)
        .await
        .unwrap();
    let SupervisorReply::Workers(listed) = daemon.request(SupervisorRequest::List).await else {
        panic!("expected a roster")
    };
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].pid, descriptor.pid);
    assert!(!workers.join("gone.json").exists());
}

#[tokio::test]
async fn a_worker_elects_a_replacement_daemon_and_it_adopts_the_worker() {
    let mut daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open_saved(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };

    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    let _ = std::fs::remove_file(&daemon.socket);

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(SupervisorReply::Status { workers, .. }) = tokio::time::timeout(
            Duration::from_secs(5),
            mush_daemon::supervisor::request(&daemon.socket, &SupervisorRequest::Status),
        )
        .await
        .unwrap_or(Err(std::io::Error::other("no daemon")))
        {
            assert_eq!(workers, 1);
            break;
        }
        assert!(Instant::now() < deadline, "no replacement daemon appeared");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(alive(descriptor.pid));
    let pid = rustix::process::Pid::from_raw(i32::try_from(descriptor.pid).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let SupervisorReply::Workers(workers) = daemon.request(SupervisorRequest::List).await
        else {
            panic!("expected a roster")
        };
        if workers
            .iter()
            .any(|worker| worker.pid != descriptor.pid && worker.failed.is_none())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the adopted worker was never restarted: {workers:?}\n{}",
            std::fs::read_to_string(daemon.dir.path().join("data/workers/daemon.log"))
                .unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
