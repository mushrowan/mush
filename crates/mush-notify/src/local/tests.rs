use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;

fn directory() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::tempdir_in(root.canonicalize().unwrap()).unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn configured(root: &Path) -> Config {
    Config {
        mode: Mode::Send,
        account: "+12025550100".into(),
        recipients: vec!["+12025550101".into()],
        socket: Some(root.join("socket")),
        ..Config::default()
    }
}

fn loaded(root: &Path) -> Config {
    let dir = root.join("mush-notify");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, toml::to_string(&configured(root)).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    load_config(root, rustix::process::getuid().as_raw()).unwrap()
}

#[test]
fn private_config_rejects_project_paths_permissions_symlinks_and_oversize() {
    let d = directory();
    let root = d.path();
    let dir = root.join("mush-notify");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = dir.join("config.toml");
    std::fs::write(&file, toml::to_string(&configured(root)).unwrap()).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let uid = rustix::process::getuid().as_raw();
    assert!(load_config(root, uid).is_some());
    assert!(load_config(root, uid + 1).is_none());
    assert!(!private_path(
        &file,
        &root.join("other"),
        uid,
        PathKind::Config
    ));
    let linked = root.join("linked");
    std::fs::hard_link(&file, &linked).unwrap();
    assert!(load_config(root, uid).is_none());
    std::fs::remove_file(linked).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(load_config(root, uid).is_none());
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&file, vec![b' '; LIMIT as usize + 1]).unwrap();
    assert!(load_config(root, uid).is_none());
    std::fs::remove_file(&file).unwrap();
    symlink(root.join("other"), &file).unwrap();
    assert!(load_config(root, uid).is_none());
}

#[test]
fn metadata_projection_and_session_classification_fail_closed() {
    let names = [
        "User",
        "Remote",
        "Class",
        "Seat",
        "State",
        "LockedHint",
        "IdleHint",
        "IdleSinceHintMonotonic",
    ];
    let text = "User=1000\nRemote=no\nClass=user\nSeat=seat0\nState=active\nLockedHint=no\nIdleHint=yes\nIdleSinceHintMonotonic=1000000\n";
    let mut p = parse_properties(text, &names).unwrap();
    assert_eq!(
        session_activity(&p, 1000).unwrap().activity,
        Activity::IdleSince(Duration::from_secs(1))
    );
    assert!(session_activity(&p, 1001).is_none());
    p.insert("LockedHint".into(), "yes".into());
    assert_eq!(
        session_activity(&p, 1000).unwrap().activity,
        Activity::Locked
    );
    p.insert("Class".into(), "greeter".into());
    assert_eq!(
        session_activity(&p, 1000).unwrap().activity,
        Activity::Unknown
    );
    assert!(parse_properties(&(text.to_owned() + "User=1000\n"), &names).is_none());
    assert!(parse_properties(&(text.to_owned() + "RemoteHost=private\n"), &names).is_none());
    assert!(parse_properties("User=1000\n", &names).is_none());
}

#[tokio::test]
async fn fake_socket_contract_success_errors_unexpected_frames_and_disconnects() {
    for (reply, accepted) in [
        (
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"timestamp\":999}}\n",
            true,
        ),
        (
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-1,\"message\":\"private failure\"}}\n",
            false,
        ),
        (
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"timestamp\":999}}\n",
            false,
        ),
        (
            "{\"jsonrpc\":\"2.0\",\"method\":\"receive\",\"params\":{}}\n",
            false,
        ),
        ("{invalid\n", false),
        ("", false),
    ] {
        let d = directory();
        let config = loaded(d.path());
        let socket = config.socket.as_ref().unwrap();
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (read_tx, read_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut request = String::new();
            BufReader::new(&mut s)
                .read_line(&mut request)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&request).unwrap(),
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "send", "params": {"account": "+12025550100", "recipient": ["+12025550101"], "message": "awaiting input"}
                })
            );
            read_tx.send(()).unwrap();
            s.write_all(reply.as_bytes()).await.unwrap();
        });
        let t = SocketTransport {
            config,
            root: d.path().into(),
            uid: rustix::process::getuid().as_raw(),
        };
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), t.send())
                .await
                .unwrap(),
            accepted
        );
        read_rx.await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn missing_or_public_socket_never_connects() {
    let d = directory();
    let config = loaded(d.path());
    let t = SocketTransport {
        config: config.clone(),
        root: d.path().into(),
        uid: rustix::process::getuid().as_raw(),
    };
    assert!(!t.send().await);
    let socket = config.socket.as_ref().unwrap();
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(!t.send().await);
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let wrong_owner = SocketTransport {
        config: config.clone(),
        root: d.path().into(),
        uid: rustix::process::getuid().as_raw() + 1,
    };
    assert!(!wrong_owner.send().await);
    let link = d.path().join("socket-link");
    symlink(socket, &link).unwrap();
    let linked = SocketTransport {
        config: Config {
            socket: Some(link),
            ..config.clone()
        },
        root: d.path().into(),
        uid: rustix::process::getuid().as_raw(),
    };
    assert!(!linked.send().await);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn fake_socket_oversize_and_timeout_are_bounded() {
    for hang in [false, true] {
        let d = directory();
        let config = loaded(d.path());
        let socket = config.socket.as_ref().unwrap();
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let read = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_read = Arc::clone(&read);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut request)
                .await
                .unwrap();
            server_read.store(true, std::sync::atomic::Ordering::SeqCst);
            if hang {
                std::future::pending::<()>().await;
            } else {
                let _ = stream.write_all(&vec![b'x'; LIMIT as usize + 1]).await;
            }
        });
        let t = SocketTransport {
            config,
            root: d.path().into(),
            uid: rustix::process::getuid().as_raw(),
        };
        let result = tokio::time::timeout(Duration::from_millis(100), t.send()).await;
        assert!(read.load(std::sync::atomic::Ordering::SeqCst));
        if hang {
            assert!(result.is_err());
        } else {
            assert_eq!(result, Ok(false));
        }
        server.abort();
        let _ = server.await;
    }
}

#[test]
fn user_config_root_has_no_environment_or_project_selection() {
    assert_eq!(runtime_dir(1234), Path::new("/run/user/1234"));
    let d = directory();
    let project = d.path().join(".mush");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        project.join("config.toml"),
        toml::to_string(&configured(d.path())).unwrap(),
    )
    .unwrap();
    assert!(load_config(d.path(), rustix::process::getuid().as_raw()).is_none());
}

#[test]
fn runtime_directory_file_and_socket_types_are_distinct() {
    let d = directory();
    let root = d.path();
    let uid = rustix::process::getuid().as_raw();
    let config = loaded(root);
    let file = root.join("mush-notify/config.toml");
    let socket = config.socket.unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    for (path, expected) in [(root, 0), (file.as_path(), 1), (socket.as_path(), 2)] {
        for (index, kind) in [PathKind::Directory, PathKind::Config, PathKind::Socket]
            .into_iter()
            .enumerate()
        {
            assert_eq!(private_path(path, root, uid, kind), index == expected);
        }
    }
    assert!(load_config(root, uid).is_some());
}

#[test]
fn private_runtime_ownership_modes_and_parent_symlinks_are_enforced() {
    let d = directory();
    let root = d.path();
    let uid = rustix::process::getuid().as_raw();
    loaded(root);
    assert!(!private_path(root, root, uid + 1, PathKind::Directory));
    for mode in [0o755, 0o770, 0o777] {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(load_config(root, uid).is_none());
    }
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(load_config(root, uid).is_some());
    let old = root.join("mush-notify");
    let moved = root.join("moved");
    std::fs::rename(&old, &moved).unwrap();
    symlink(&moved, &old).unwrap();
    assert!(load_config(root, uid).is_none());
}

#[test]
fn runtime_config_cannot_select_an_outside_socket() {
    let d = directory();
    let root = d.path();
    let mut config = loaded(root);
    let uid = rustix::process::getuid().as_raw();
    for path in [
        root.to_path_buf(),
        root.parent().unwrap().join("socket"),
        PathBuf::from("/run/user/0/socket"),
    ] {
        config.socket = Some(path);
        std::fs::write(
            root.join("mush-notify/config.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        assert!(load_config(root, uid).is_none());
    }
}
