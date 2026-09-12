use std::collections::BTreeMap;
use std::future::Future;
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::{
    Activity, ActivitySource, Clock, Config, Dispatcher, Mode, SessionActivity, Snapshot, Transport,
};

const LIMIT: u64 = 8192;

struct Monotonic;

impl Clock for Monotonic {
    fn now(&self) -> Option<Duration> {
        let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        Some(Duration::new(
            time.tv_sec.try_into().ok()?,
            time.tv_nsec.try_into().ok()?,
        ))
    }
}

fn runtime_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}"))
}

#[derive(Clone, Copy)]
enum PathKind {
    Directory,
    Config,
    Socket,
}

impl PathKind {
    fn matches(self, meta: &std::fs::Metadata) -> bool {
        match self {
            Self::Directory => meta.is_dir(),
            Self::Config => meta.is_file() && meta.nlink() == 1,
            Self::Socket => meta.file_type().is_socket(),
        }
    }
}

fn private_path(path: &Path, root: &Path, uid: u32, kind: PathKind) -> bool {
    if !path.is_absolute() || !path.starts_with(root) || path.components().count() > 32 {
        return false;
    }
    if path.components().any(|c| {
        !matches!(
            c,
            std::path::Component::RootDir | std::path::Component::Normal(_)
        )
    }) {
        return false;
    }
    for ancestor in path.ancestors() {
        let Ok(meta) = std::fs::symlink_metadata(ancestor) else {
            return false;
        };
        if meta.file_type().is_symlink() || meta.mode() & 0o022 != 0 {
            return false;
        }
        if ancestor.starts_with(root) {
            if meta.uid() != uid || meta.mode() & 0o077 != 0 {
                return false;
            }
        } else if meta.uid() != 0 && meta.uid() != uid {
            return false;
        }
        if ancestor == path {
            if !kind.matches(&meta) {
                return false;
            }
        } else if !meta.is_dir() {
            return false;
        }
    }
    true
}

fn load_config(root: &Path, uid: u32) -> Option<Config> {
    if !private_path(root, root, uid, PathKind::Directory) {
        return None;
    }
    let path = root.join("mush-notify/config.toml");
    if !private_path(&path, root, uid, PathKind::Config) {
        return None;
    }
    let file = std::fs::File::open(&path).ok()?;
    let opened = file.metadata().ok()?;
    let checked = std::fs::symlink_metadata(&path).ok()?;
    if opened.dev() != checked.dev()
        || opened.ino() != checked.ino()
        || opened.nlink() != 1
        || opened.uid() != uid
        || opened.mode() & 0o077 != 0
    {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > LIMIT {
        return None;
    }
    let config: Config = toml::from_str(std::str::from_utf8(&bytes).ok()?).ok()?;
    config.validate().ok()?;
    if config.mode != Mode::Disabled
        && !config
            .socket
            .as_ref()
            .is_some_and(|socket| socket.starts_with(root) && socket != root)
    {
        return None;
    }
    Some(config)
}

pub(super) fn user_dispatcher() -> Option<Dispatcher> {
    let uid = rustix::process::getuid().as_raw();
    if uid == 0 || uid != rustix::process::geteuid().as_raw() {
        return None;
    }
    let root = runtime_dir(uid);
    let config = load_config(&root, uid)?;
    if config.mode == Mode::Disabled {
        return None;
    }
    let clock: Arc<dyn Clock> = Arc::new(Monotonic);
    let activity = Arc::new(Logind {
        uid,
        clock: Arc::clone(&clock),
    });
    let transport = Arc::new(SocketTransport {
        config: config.clone(),
        root,
        uid,
    });
    Dispatcher::new(config, clock, activity, transport).ok()
}

struct SocketTransport {
    config: Config,
    root: PathBuf,
    uid: u32,
}

impl SocketTransport {
    async fn checked_path(&self) -> Option<PathBuf> {
        let path = self.config.socket.clone()?;
        let root = self.root.clone();
        let uid = self.uid;
        let checked = path.clone();
        if !tokio::task::spawn_blocking(move || {
            private_path(&checked, &root, uid, PathKind::Socket)
        })
        .await
        .ok()?
        {
            return None;
        }
        Some(path)
    }

    async fn deliver(&self) -> Option<()> {
        let path = self.checked_path().await?;
        let mut stream = tokio::net::UnixStream::connect(path).await.ok()?;
        let peer = stream.peer_cred().ok()?;
        if peer.uid() != self.uid {
            return None;
        }
        let mut request = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "send",
            "params": {
                "account": self.config.account,
                "recipient": self.config.recipients,
                "message": "awaiting input"
            }
        }))
        .ok()?;
        request.push(b'\n');
        stream.write_all(&request).await.ok()?;
        let mut bytes = Vec::new();
        BufReader::new(stream.take(LIMIT + 1))
            .read_until(b'\n', &mut bytes)
            .await
            .ok()?;
        if bytes.len() as u64 > LIMIT || bytes.last() != Some(&b'\n') {
            return None;
        }
        let response: Reply = serde_json::from_slice(&bytes).ok()?;
        if response.jsonrpc != "2.0" || response.id != 1 {
            return None;
        }
        match response.result {
            Some(result) if response.error.is_none() && result.timestamp > 0 => Some(()),
            _ => None,
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    jsonrpc: String,
    id: u64,
    result: Option<SendResult>,
    error: Option<serde::de::IgnoredAny>,
}

#[derive(serde::Deserialize)]
struct SendResult {
    timestamp: u64,
}

impl Transport for SocketTransport {
    fn available(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async move { self.checked_path().await.is_some() })
    }

    fn send(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async move { self.deliver().await.is_some() })
    }
}

struct Logind {
    uid: u32,
    clock: Arc<dyn Clock>,
}

fn loginctl() -> Option<PathBuf> {
    for path in ["/run/current-system/sw/bin/loginctl", "/usr/bin/loginctl"] {
        let Ok(path) = std::fs::canonicalize(path) else {
            continue;
        };
        if path.ancestors().all(|p| {
            std::fs::symlink_metadata(p).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o022 == 0)
        }) {
            return Some(path);
        }
    }
    None
}

async fn properties(
    executable: &Path,
    command: &str,
    id: &str,
    names: &[&str],
) -> Option<BTreeMap<String, String>> {
    let mut cmd = tokio::process::Command::new(executable);
    cmd.env_clear()
        .env("LC_ALL", "C")
        .args(["--no-pager", "--no-ask-password", "--all"]);
    for name in names {
        cmd.arg(format!("--property={name}"));
    }
    let mut child = cmd
        .args([command, id])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    if bytes.len() as u64 > LIMIT {
        return None;
    }
    if !child.wait().await.ok()?.success() {
        return None;
    }
    parse_properties(std::str::from_utf8(&bytes).ok()?, names)
}

fn parse_properties(text: &str, names: &[&str]) -> Option<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line.split_once('=')?;
        if !names.contains(&key) || values.insert(key.into(), value.into()).is_some() {
            return None;
        }
    }
    if values.len() != names.len() {
        return None;
    }
    Some(values)
}

fn session_activity(p: &BTreeMap<String, String>, uid: u32) -> Option<SessionActivity> {
    if p.get("User")?.parse::<u32>().ok()? != uid {
        return None;
    }
    let local = match p.get("Remote")?.as_str() {
        "yes" => false,
        "no" => true,
        _ => return None,
    };
    let mut activity = Activity::Unknown;
    if p.get("Class")? == "user"
        && !p.get("Seat")?.is_empty()
        && matches!(p.get("State")?.as_str(), "active" | "online")
    {
        activity = match (p.get("LockedHint")?.as_str(), p.get("IdleHint")?.as_str()) {
            ("yes", "yes" | "no") => Activity::Locked,
            ("no", "yes") => Activity::IdleSince(Duration::from_micros(
                p.get("IdleSinceHintMonotonic")?.parse().ok()?,
            )),
            ("no", "no") => Activity::Active,
            _ => return None,
        };
    }
    Some(SessionActivity { local, activity })
}

impl ActivitySource for Logind {
    fn snapshot(&self) -> Pin<Box<dyn Future<Output = Option<Snapshot>> + Send + '_>> {
        Box::pin(async move {
            let executable = tokio::task::spawn_blocking(loginctl).await.ok()??;
            let observed_at = self.clock.now()?;
            let user = properties(
                &executable,
                "show-user",
                &self.uid.to_string(),
                &["Sessions"],
            )
            .await?;
            let ids: Vec<_> = user.get("Sessions")?.split_whitespace().collect();
            if ids.is_empty() || ids.len() > 32 {
                return None;
            }
            let mut sessions = Vec::with_capacity(ids.len());
            for id in ids {
                if id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    return None;
                }
                let p = properties(
                    &executable,
                    "show-session",
                    id,
                    &[
                        "User",
                        "Remote",
                        "Class",
                        "Seat",
                        "State",
                        "LockedHint",
                        "IdleHint",
                        "IdleSinceHintMonotonic",
                    ],
                )
                .await?;
                sessions.push(session_activity(&p, self.uid)?);
            }
            Some(Snapshot {
                observed_at,
                sessions,
            })
        })
    }
}

#[cfg(test)]
mod tests;
