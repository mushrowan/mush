#![expect(clippy::expect_used, clippy::unwrap_used)]

use std::os::unix::process::CommandExt;
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
        let dir = crate::tempdir().unwrap();
        let socket = dir.path().join("mush").join("daemon.sock");
        let child = spawn_daemon(dir.path(), &socket);
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

fn spawn_daemon(dir: &Path, socket: &Path) -> std::process::Child {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mush"));
    command.env_clear();
    for (key, value) in env(dir) {
        command.env(key, value);
    }
    let child = command
        .args(["daemon", "--socket"])
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        assert!(Instant::now() < deadline, "daemon socket never answered");
        std::thread::sleep(Duration::from_millis(10));
    }
    child
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
    rustix::process::Pid::from_raw(pid as i32)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
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

    assert!(matches!(
        daemon
            .request(SupervisorRequest::Stop {
                session_id: descriptor.session_id.clone(),
            })
            .await,
        SupervisorReply::Done
    ));
    drop(connection);
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
        "[api_keys]\nfixture = 'fixture-key'\n",
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
async fn a_daemon_worker_holds_the_sync_lease_while_open_and_releases_it_when_stopped() {
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
    let generated = daemon
        .cli()
        .args(["sync", "key", "generate"])
        .output()
        .unwrap();
    assert!(generated.status.success());
    let hash = daemon
        .cli()
        .args(["sync", "key", "token-hash"])
        .output()
        .unwrap();
    let hash = String::from_utf8(hash.stdout).unwrap().trim().to_owned();
    struct Killed(std::process::Child);
    impl Drop for Killed {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut server = Killed(
        daemon
            .cli()
            .args(["sync", "serve", "--listen", "127.0.0.1:0", "--root"])
            .arg(daemon.dir.path().join("vault"))
            .args(["--token-hash", &hash])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut announced = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(server.0.stderr.take().unwrap()),
        &mut announced,
    )
    .unwrap();
    let url = format!("http://{}", announced.trim().rsplit(' ').next().unwrap());
    std::fs::write(
        config.join("config.toml"),
        format!(
            "[api_keys]\nfixture = 'fixture-key'\n[sync]\nserver = \"{url}\"\nmachine = \"dot\"\n"
        ),
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
    let keys = mush_sync::Keys::load(&config.join("sync.key")).unwrap();
    let remote = mush_sync::Remote::new(&url, keys.token(), Duration::from_secs(2)).unwrap();
    let sid = keys.sid(&descriptor.session_id);
    let holders = |leases: mush_sync::protocol::Leases| -> Vec<String> {
        leases
            .held
            .into_iter()
            .filter(|(held, _)| *held == sid)
            .map(|(_, lease)| lease.holder)
            .collect()
    };
    assert_eq!(
        holders(remote.leases(Duration::from_secs(5)).await.unwrap()),
        ["dot"]
    );

    assert!(matches!(
        daemon
            .request(SupervisorRequest::Stop {
                session_id: descriptor.session_id.clone(),
            })
            .await,
        SupervisorReply::Done
    ));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !holders(remote.leases(Duration::from_secs(5)).await.unwrap()).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the stopped worker kept its lease"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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

    let deadline = Instant::now() + Duration::from_secs(10);
    let rows = loop {
        let listed = daemon.cli().args(["list", "--json"]).output().unwrap();
        assert!(listed.status.success(), "{listed:?}");
        let rows: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
        if rows[0]["name"] == "scout one" || Instant::now() >= deadline {
            break rows;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(rows[0]["name"], "scout one");
    assert_eq!(rows[0]["id"], descriptor.session_id[..8]);
    assert_eq!(rows[0]["status"], "idle");

    let renamed = daemon
        .cli()
        .args([
            "rename",
            &descriptor.session_id[..8],
            "scout",
            "two",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(renamed.status.success(), "{renamed:?}");
    let renamed: serde_json::Value = serde_json::from_slice(&renamed.stdout).unwrap();
    assert_eq!(renamed["id"], descriptor.session_id);
    assert_eq!(renamed["name"], "scout two");

    let stopped = daemon
        .cli()
        .args(["stop", &descriptor.session_id[..8], "--json"])
        .output()
        .unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    let stopped: serde_json::Value = serde_json::from_slice(&stopped.stdout).unwrap();
    assert_eq!(stopped["stopped"], descriptor.session_id);
    assert!(!alive(descriptor.pid));
}

#[tokio::test]
async fn stop_archives_the_session_and_resuming_it_brings_it_back() {
    let daemon = Daemon::start();
    let store = mush_session::SessionStore::new(daemon.dir.path().join("data/sessions"));
    let mut saved =
        mush_session::Session::new("claude-opus-4-7", &daemon.dir.path().display().to_string());
    saved.meta.title = Some("keeper".into());
    store.save(&saved).unwrap();
    let id = saved.meta.id.to_string();
    let SupervisorReply::Worker { descriptor, .. } = daemon
        .request(daemon.open_saved(OpenTarget::Resume(id.clone())))
        .await
    else {
        panic!("expected a worker")
    };

    let stopped = daemon.cli().args(["stop", &id[..8]]).output().unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(!alive(descriptor.pid));
    let meta = store.resolve(&id).unwrap();
    assert_eq!(meta.lifecycle, mush_session::Lifecycle::Archived);

    let listed = daemon.cli().args(["list", "--json"]).output().unwrap();
    let rows: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(rows, serde_json::json!([]));
    let listed = daemon
        .cli()
        .args(["list", "--all", "--json"])
        .output()
        .unwrap();
    let rows: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(rows[0]["id"], id[..8]);
    assert_eq!(rows[0]["status"], "archived");

    let SupervisorReply::Worker { .. } = daemon
        .request(daemon.open_saved(OpenTarget::Resume(id.clone())))
        .await
    else {
        panic!("expected a worker")
    };
    assert_eq!(
        store.resolve(&id).unwrap().lifecycle,
        mush_session::Lifecycle::Live
    );
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
async fn doctor_leaves_the_socket_of_a_daemon_it_cannot_understand() {
    let dir = crate::tempdir().unwrap();
    let socket = dir.path().join("mush").join("daemon.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let _ = std::io::Write::write_all(&mut stream, b"not a reply\n");
        }
    });

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mush"));
    command.env_clear();
    for (key, value) in env(dir.path()) {
        command.env(key, value);
    }
    let looked = command
        .args(["doctor", "--fix", "--json"])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(looked.status.success(), "{looked:?}");
    let findings: serde_json::Value = serde_json::from_slice(&looked.stdout).unwrap();
    let daemon = findings
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["what"] == "daemon")
        .cloned()
        .unwrap();
    assert_eq!(daemon["health"], "unreachable");
    assert_eq!(daemon["fixed"], false);
    assert!(socket.exists());
}

fn start_unsupervised_worker(
    dir: &Path,
) -> (
    std::sync::mpsc::Receiver<std::process::ExitStatus>,
    mush_daemon::WorkerDescriptor,
) {
    let workers = dir.join("mush").join("workers");
    std::fs::create_dir_all(&workers).unwrap();
    let descriptor = workers.join("lone.json");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mush"));
    command.env_clear();
    for (key, value) in env(dir) {
        command.env(key, value);
    }
    let mut child = command
        .args(["--no-tools", "--no-session", "--model", "claude-opus-4-7"])
        .args(["worker", "--socket"])
        .arg(workers.join("lone.sock"))
        .arg("--descriptor")
        .arg(&descriptor)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    let (exited, exit) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok(status) = child.wait() {
            let _ = exited.send(status);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(worker) = std::fs::read(&descriptor)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<mush_daemon::WorkerDescriptor>(&bytes).ok())
            .filter(|worker| !worker.session_id.is_empty())
        {
            return (exit, worker);
        }
        let late = Instant::now() >= deadline;
        if late {
            let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
        assert!(!late, "the worker never announced itself");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn cli_in(dir: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mush"));
    command.env_clear();
    for (key, value) in env(dir) {
        command.env(key, value);
    }
    command.current_dir(dir).stdin(Stdio::null());
    command
}

#[tokio::test]
async fn shutdown_without_a_daemon_reports_and_kills_running_workers() {
    let dir = crate::tempdir().unwrap();
    let (exit, worker) = start_unsupervised_worker(dir.path());

    let refused = cli_in(dir.path()).arg("shutdown").output().unwrap();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("1 agent(s) are still running"), "{stderr}");
    assert!(alive(worker.pid));

    let forced = cli_in(dir.path())
        .args(["shutdown", "--force"])
        .output()
        .unwrap();
    assert!(forced.status.success(), "{forced:?}");
    exit.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!worker.socket.with_extension("json").exists());
}

#[tokio::test]
async fn stop_without_a_daemon_stops_the_matching_worker() {
    let dir = crate::tempdir().unwrap();
    let (exit, worker) = start_unsupervised_worker(dir.path());

    let stopped = cli_in(dir.path())
        .args(["stop", &worker.session_id[..8]])
        .output()
        .unwrap();
    assert!(stopped.status.success(), "{stopped:?}");
    exit.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!worker.socket.with_extension("json").exists());
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

    let refused = daemon.cli().args(["shutdown", "--json"]).output().unwrap();
    assert!(!refused.status.success());
    assert!(alive(descriptor.pid));

    let forced = daemon
        .cli()
        .args(["shutdown", "--force", "--json"])
        .output()
        .unwrap();
    assert!(forced.status.success(), "{forced:?}");
    let forced: serde_json::Value = serde_json::from_slice(&forced.stdout).unwrap();
    assert_eq!(forced["stopped"][0], descriptor.session_id);
    assert!(!alive(descriptor.pid));
}

async fn kill_and_await_recovery(
    daemon: &Daemon,
    descriptor: &mush_daemon::WorkerDescriptor,
) -> Vec<mush_ai::types::Message> {
    let pid = rustix::process::Pid::from_raw(i32::try_from(descriptor.pid).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let recovered = loop {
        let reply = daemon.request(SupervisorRequest::List).await;
        let SupervisorReply::Workers(workers) = &reply else {
            unreachable!("expected a roster, got {reply:?}")
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
    client.transcript().await
}

fn recovery_markers(transcript: &[mush_ai::types::Message]) -> Vec<String> {
    transcript
        .iter()
        .filter_map(|message| match message {
            mush_ai::types::Message::Event(event)
                if event.kind == mush_ai::types::EventKind::Recovery =>
            {
                Some(event.content.clone())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_killed_idle_worker_comes_back_on_the_same_socket_without_a_marker() {
    let daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open_saved(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };
    assert!(alive(descriptor.pid));

    let transcript = kill_and_await_recovery(&daemon, &descriptor).await;
    assert!(recovery_markers(&transcript).is_empty(), "{transcript:?}");
}

#[tokio::test]
async fn a_worker_killed_mid_turn_comes_back_with_an_interrupted_marker() {
    let daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open_saved(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };
    let busy = mush_daemon::WorkerDescriptor {
        turn_active: true,
        ..*descriptor.clone()
    };
    std::fs::write(
        descriptor.socket.with_extension("json"),
        serde_json::to_vec(&busy).unwrap(),
    )
    .unwrap();

    let transcript = kill_and_await_recovery(&daemon, &descriptor).await;
    let markers = recovery_markers(&transcript);
    assert_eq!(markers.len(), 1, "{transcript:?}");
    assert!(markers[0].contains("Inspect external side effects"));
}

#[tokio::test]
async fn a_worker_that_switched_sessions_recovers_the_session_it_had() {
    let daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open_saved(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };
    let store = mush_session::SessionStore::new(daemon.dir.path().join("data/sessions"));
    let other =
        mush_session::Session::new("claude-opus-4-7", &daemon.dir.path().display().to_string());
    store.save(&other).unwrap();
    let switched = mush_daemon::WorkerDescriptor {
        session_id: other.meta.id.to_string(),
        ..*descriptor.clone()
    };
    std::fs::write(
        descriptor.socket.with_extension("json"),
        serde_json::to_vec(&switched).unwrap(),
    )
    .unwrap();

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
    assert_eq!(recovered.session_id, other.meta.id.as_str());
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
    let SupervisorReply::Status { workers, .. } = daemon.request(SupervisorRequest::Status).await
    else {
        panic!("expected a status")
    };
    assert_eq!(workers, 0, "a failed descriptor is not a live worker");

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
        build: mush_daemon::build_id(),
        cwd: daemon.dir.path().to_path_buf(),
        socket: workers.join("gone.sock"),
        started_at: 1,
        updated_at: 1,
        failed: Some("gave up".into()),
        ..mush_daemon::WorkerDescriptor::default()
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
    let _attached = mush_daemon::DaemonConnection::connect(&descriptor.socket)
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
async fn resuming_without_a_model_flag_runs_the_sessions_model() {
    let daemon = Daemon::start();
    let store = mush_session::SessionStore::new(daemon.dir.path().join("data/sessions"));
    let session = mush_session::Session::new(
        "claude-sonnet-4-6",
        &daemon.dir.path().display().to_string(),
    );
    store.save(&session).unwrap();
    let id = session.meta.id.to_string();

    let reply = daemon
        .request(SupervisorRequest::Open {
            target: OpenTarget::Resume(id.clone()),
            args: vec!["--no-tools".into()],
            cwd: daemon.dir.path().to_path_buf(),
            env: env(daemon.dir.path()),
        })
        .await;
    let SupervisorReply::Worker { descriptor, .. } = reply else {
        panic!("expected a worker, got {reply:?}")
    };
    assert_eq!(descriptor.session_id, id);
    assert_eq!(descriptor.model_id, "claude-sonnet-4-6");
}

#[tokio::test]
async fn an_empty_worker_stops_when_its_last_client_detaches() {
    let daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };
    let first = mush_daemon::DaemonConnection::connect(&descriptor.socket)
        .await
        .unwrap();
    let second = mush_daemon::DaemonConnection::connect(&descriptor.socket)
        .await
        .unwrap();
    drop(first);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(alive(descriptor.pid), "one client is still attached");

    drop(second);
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(descriptor.pid) {
        assert!(Instant::now() < deadline, "the empty worker kept running");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let SupervisorReply::Workers(workers) = daemon.request(SupervisorRequest::List).await else {
        panic!("expected a roster")
    };
    assert!(
        workers.is_empty(),
        "the evicted worker came back: {workers:?}"
    );
}

#[tokio::test]
async fn an_empty_worker_leaves_no_session_behind() {
    let daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open_saved(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };
    let saved = daemon
        .dir
        .path()
        .join("data/sessions")
        .join(format!("{}.json", descriptor.session_id));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !saved.exists() {
        assert!(
            Instant::now() < deadline,
            "the live session was never saved"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let client = mush_daemon::DaemonConnection::connect(&descriptor.socket)
        .await
        .unwrap();

    drop(client);

    let deadline = Instant::now() + Duration::from_secs(10);
    while saved.exists() {
        assert!(
            Instant::now() < deadline,
            "the empty session was left behind"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_named_worker_stays_after_its_last_client_detaches() {
    let daemon = Daemon::start();
    let SupervisorReply::Worker { descriptor, .. } =
        daemon.request(daemon.open(OpenTarget::New)).await
    else {
        panic!("expected a worker")
    };
    let mut client = mush_daemon::DaemonConnection::connect(&descriptor.socket)
        .await
        .unwrap();
    client
        .send(mush_runtime::ClientCommand::Session(
            mush_runtime::SessionCommand::Name {
                title: Some("keeper".into()),
            },
        ))
        .await
        .unwrap();
    drop(client);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(alive(descriptor.pid));
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

#[tokio::test]
async fn sessions_kept_across_a_daemon_restart_come_back_on_the_next_daemon() {
    let mut daemon = Daemon::start();
    let store = mush_session::SessionStore::new(daemon.dir.path().join("data/sessions"));
    let mut saved =
        mush_session::Session::new("claude-opus-4-7", &daemon.dir.path().display().to_string());
    saved.meta.title = Some("keeper".into());
    store.save(&saved).unwrap();
    let id = saved.meta.id.to_string();
    let SupervisorReply::Worker { descriptor, .. } = daemon
        .request(daemon.open_saved(OpenTarget::Resume(id.clone())))
        .await
    else {
        panic!("expected a worker")
    };

    let relaunch = mush_daemon::Relaunch::save(&daemon.socket.with_file_name("workers"));
    assert!(matches!(
        daemon.request(SupervisorRequest::Shutdown).await,
        SupervisorReply::Done
    ));
    daemon.child.wait().unwrap();
    assert!(!alive(descriptor.pid));
    relaunch.restore();
    daemon.child = spawn_daemon(daemon.dir.path(), &daemon.socket);

    let SupervisorReply::Worker {
        descriptor: resumed,
        fresh,
    } = daemon
        .request(daemon.open_saved(OpenTarget::Resume(id.clone())))
        .await
    else {
        panic!("expected a worker")
    };
    assert!(!fresh);
    assert_eq!(resumed.session_id, id);
    assert_eq!(resumed.socket, descriptor.socket);
    assert_ne!(resumed.pid, descriptor.pid);
    let SupervisorReply::Workers(workers) = daemon.request(SupervisorRequest::List).await else {
        panic!("expected a roster")
    };
    assert_eq!(workers.len(), 1, "{workers:?}");
}
