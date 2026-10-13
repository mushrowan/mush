#![expect(clippy::unwrap_used)]
#![allow(clippy::panic)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

struct Server {
    child: Child,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn base(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mush"));
    command
        .env_clear()
        .envs(std::env::var_os("PATH").map(|path| ("PATH", path)))
        .current_dir(home)
        .env("HOME", home)
        .env("MUSH_DATA_DIR", home.join("data"))
        .env("MUSH_CONFIG_DIR", home.join("config"))
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .stdin(Stdio::null());
    command
}

fn machine(root: &Path, name: &str, url: &str, key: &Path) -> PathBuf {
    let home = root.join(name);
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::write(
        home.join("config/config.toml"),
        format!(
            "rlm = false\n[sync]\nserver = \"{url}\"\nkey_file = \"{}\"\nmachine = \"{name}\"\n",
            key.display()
        ),
    )
    .unwrap();
    home
}

fn run(home: &Path, args: &[&str]) -> Output {
    base(home).args(args).output().unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn start_server(root: &Path, key_home: &Path) -> Server {
    let hash = run(key_home, &["sync", "key", "token-hash"]);
    assert!(hash.status.success(), "{}", text(&hash));
    let hash = String::from_utf8(hash.stdout).unwrap().trim().to_owned();
    let mut child = base(root)
        .args(["sync", "serve", "--listen", "127.0.0.1:0", "--root"])
        .arg(root.join("vault"))
        .args(["--token-hash", &hash])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut announced = String::new();
    BufReader::new(child.stderr.take().unwrap())
        .read_line(&mut announced)
        .unwrap();
    let address = announced
        .trim()
        .rsplit(' ')
        .next()
        .unwrap_or_default()
        .to_owned();
    assert!(address.starts_with("127.0.0.1:"), "{announced}");
    Server {
        child,
        url: format!("http://{address}"),
    }
}

fn session(home: &Path, text: &str) -> mush_session::SessionId {
    let store = mush_session::SessionStore::new(home.join("data/sessions"));
    let mut session = mush_session::Session::new("test-model", "/tmp");
    session.meta.title = Some("synced through the cli".into());
    session.push_message(mush_ai::types::Message::User(
        mush_ai::types::UserMessage::builder()
            .content(mush_ai::types::UserContent::Text(text.into()))
            .timestamp_ms(mush_ai::types::Timestamp::from_ms(1000))
            .build(),
    ));
    store.save(&session).unwrap();
    session.meta.id
}

#[test]
fn sessions_travel_between_machines_through_a_mush_sync_server() {
    let root = crate::tempdir().unwrap();
    let key = root.path().join("shared/sync.key");
    let keyring = machine(root.path(), "keyring", "http://unused", &key);
    let generated = run(&keyring, &["sync", "key", "generate"]);
    assert!(generated.status.success(), "{}", text(&generated));
    let again = run(&keyring, &["sync", "key", "generate"]);
    assert!(
        !again.status.success(),
        "a second key must never replace the first"
    );

    let server = start_server(root.path(), &keyring);
    let dot = machine(root.path(), "dot", &server.url, &key);
    let esme = machine(root.path(), "esme", &server.url, &key);
    let id = session(&dot, "written on dot");

    let pushed = run(&dot, &["sync"]);
    assert!(pushed.status.success(), "{}", text(&pushed));
    assert!(text(&pushed).contains("pushed 1"), "{}", text(&pushed));
    let pulled = run(&esme, &["sync"]);
    assert!(text(&pulled).contains("pulled 1"), "{}", text(&pulled));

    let store = mush_session::SessionStore::new(esme.join("data/sessions"));
    let loaded = store.load(&id).unwrap();
    assert_eq!(loaded.meta.title.as_deref(), Some("synced through the cli"));
    let listed = run(&esme, &["list", "--all"]);
    assert!(
        text(&listed).contains("synced through the cli"),
        "{}",
        text(&listed)
    );
    let status = run(&esme, &["sync", "status"]);
    assert!(text(&status).contains("1 synced"), "{}", text(&status));
}

#[test]
fn a_session_open_on_another_machine_refuses_to_open_here() {
    let root = crate::tempdir().unwrap();
    let key = root.path().join("shared/sync.key");
    let keyring = machine(root.path(), "keyring", "http://unused", &key);
    assert!(run(&keyring, &["sync", "key", "generate"]).status.success());
    let server = start_server(root.path(), &keyring);
    let dot = machine(root.path(), "dot", &server.url, &key);
    let esme = machine(root.path(), "esme", &server.url, &key);
    let id = session(&dot, "busy on dot");
    assert!(run(&dot, &["sync"]).status.success());
    assert!(run(&esme, &["sync"]).status.success());

    let keys = mush_sync::Keys::load(&key).unwrap();
    let remote = mush_sync::Remote::new(&server.url, keys.token(), Duration::from_secs(2)).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let claimed = runtime
        .block_on(remote.claim(
            &keys.sid(id.as_str()),
            &mush_sync::protocol::LeaseRequest {
                holder: "dot".into(),
                ttl_ms: 60_000,
                take: false,
            },
        ))
        .unwrap();
    assert!(matches!(claimed, mush_sync::remote::Claimed::Granted(_)));

    let refused = run(&esme, &["--resume", id.as_str(), "-p", "hello"]);
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("is open on dot"),
        "{}",
        text(&refused)
    );

    let taken = run(&esme, &["sync", "take", id.as_str()]);
    assert!(taken.status.success(), "{}", text(&taken));
    assert!(text(&taken).contains("from dot"), "{}", text(&taken));
    let status = run(&esme, &["sync", "status"]);
    assert!(!text(&status).contains("on dot"), "{}", text(&status));
    let renewed = runtime
        .block_on(remote.claim(
            &keys.sid(id.as_str()),
            &mush_sync::protocol::LeaseRequest {
                holder: "dot".into(),
                ttl_ms: 60_000,
                take: false,
            },
        ))
        .unwrap();
    assert!(
        matches!(&renewed, mush_sync::remote::Claimed::Held { lease, .. } if lease.holder == "esme"),
        "dot's renewal must not win the session back: {renewed:?}"
    );

    let missing = run(&esme, &["--resume", "nowhere", "-p", "hello"]);
    assert!(
        text(&missing).contains("mush sync fetches sessions"),
        "{}",
        text(&missing)
    );
}
