use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::protocol::{ARTIFACTS, FileRef, META, Manifest, valid_session};

const TAIL: u64 = 64 * 1024;
const KERNEL_STATE: &str = "kernel-state.pickle";

#[derive(Debug, Clone)]
pub struct Layout {
    pub sessions: PathBuf,
    pub artifacts: PathBuf,
    pub harness: PathBuf,
    pub state: PathBuf,
}

impl Layout {
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            sessions: data_dir.join("sessions"),
            artifacts: data_dir.join("session-artifacts"),
            harness: data_dir.join("harness"),
            state: data_dir.join("sync"),
        }
    }

    #[must_use]
    pub fn meta(&self, id: &str) -> PathBuf {
        self.sessions.join(format!("{id}.json"))
    }

    #[must_use]
    pub fn history(&self, id: &str) -> PathBuf {
        self.sessions.join(format!("{id}.history.jsonl"))
    }

    #[must_use]
    pub fn artifact_dir(&self, id: &str) -> PathBuf {
        self.artifacts.join(id)
    }

    #[must_use]
    pub fn file(&self, id: &str, key: &str) -> Option<PathBuf> {
        if key == META {
            return Some(self.meta(id));
        }
        let relative = Path::new(key.strip_prefix(ARTIFACTS)?);
        let plain = relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
        (plain && !relative.as_os_str().is_empty()).then(|| self.artifact_dir(id).join(relative))
    }

    fn state_dir(&self) -> PathBuf {
        self.state.join("sessions")
    }

    #[must_use]
    pub fn state_file(&self, id: &str) -> PathBuf {
        self.state_dir().join(format!("{id}.json"))
    }

    #[must_use]
    pub fn cursor_file(&self) -> PathBuf {
        self.state.join("cursor.json")
    }

    #[must_use]
    pub fn store(&self) -> mush_session::SessionStore {
        mush_session::SessionStore::new(self.sessions.clone())
    }

    pub fn local_ids(&self) -> std::io::Result<BTreeSet<String>> {
        let mut ids = BTreeSet::new();
        let entries = match std::fs::read_dir(&self.sessions) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let name = entry?.file_name().to_string_lossy().into_owned();
            let id = name
                .strip_suffix(".history.jsonl")
                .or_else(|| name.strip_suffix(".json"));
            if let Some(id) = id
                && valid_session(id)
            {
                ids.insert(id.to_owned());
            }
        }
        Ok(ids)
    }

    pub fn state_ids(&self) -> std::io::Result<BTreeSet<String>> {
        let mut ids = BTreeSet::new();
        let entries = match std::fs::read_dir(self.state_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".json")
                && valid_session(id)
            {
                ids.insert(id.to_owned());
            }
        }
        Ok(ids)
    }

    pub fn forget_states(&self) -> std::io::Result<()> {
        match std::fs::remove_dir_all(self.state_dir()) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    pub fn session_lock(&self, id: &str) -> std::io::Result<Option<std::fs::File>> {
        let dir = self.state.join("locks");
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{id}.lock")))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub len: u64,
    pub mtime_ns: i64,
    pub ino: u64,
    pub dev: u64,
}

pub fn stamp(path: &Path) -> std::io::Result<Option<Stamp>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Some(Stamp {
            len: meta.len(),
            mtime_ns: meta.mtime().saturating_mul(1_000_000_000) + meta.mtime_nsec(),
            ino: meta.ino(),
            dev: meta.dev(),
        })),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cursor {
    pub server: String,
    pub rev: u64,
}

impl Cursor {
    pub fn load(layout: &Layout) -> std::io::Result<Self> {
        match std::fs::read(layout.cursor_file()) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, layout: &Layout) -> std::io::Result<()> {
        mush_ai::private_io::write_private(&layout.cursor_file(), serde_json::to_vec(self)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryStamp {
    pub stamp: Stamp,
    pub tail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionState {
    pub version: u64,
    #[serde(default)]
    pub remote_version: u64,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub manifest: Option<Manifest>,
    #[serde(default)]
    pub skipped: BTreeMap<String, Option<FileRef>>,
    #[serde(default)]
    pub history: Option<HistoryStamp>,
    #[serde(default)]
    pub stamps: BTreeMap<String, Stamp>,
}

impl SessionState {
    pub fn load(layout: &Layout, id: &str) -> std::io::Result<Option<Self>> {
        match std::fs::read(layout.state_file(id)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, layout: &Layout, id: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(layout.state_dir())?;
        mush_ai::private_io::write_private(&layout.state_file(id), serde_json::to_vec(self)?)
    }

    #[must_use]
    pub fn file(&self, key: &str) -> Option<&FileRef> {
        match self.skipped.get(key) {
            Some(local) => local.as_ref(),
            None => self.manifest.as_ref()?.files.get(key),
        }
    }
}

pub fn tail_hash(path: &Path, end: u64) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let start = end.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(usize::try_from(end - start).unwrap_or(0));
    file.take(end - start).read_to_end(&mut bytes)?;
    if (bytes.len() as u64) < end - start {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "history shrank while it was read",
        ));
    }
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

pub fn complete_lines(path: &Path, floor: u64, len: u64) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    let mut end = len;
    let mut block = vec![0u8; 64 * 1024];
    while end > floor {
        let start = end.saturating_sub(block.len() as u64).max(floor);
        let size = usize::try_from(end - start).unwrap_or(0);
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut block[..size])?;
        if let Some(newline) = block[..size].iter().rposition(|byte| *byte == b'\n') {
            return Ok(start + newline as u64 + 1);
        }
        end = start;
    }
    Ok(floor)
}

pub fn read_range(file: &std::fs::File, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut bytes = vec![0u8; usize::try_from(end - start).unwrap_or(0)];
    file.read_exact_at(&mut bytes, start)?;
    Ok(bytes)
}

pub fn prefix_equal(path: &Path, start: u64, other: &Path, len: u64) -> std::io::Result<bool> {
    let mut ours = std::fs::File::open(path)?;
    ours.seek(SeekFrom::Start(start))?;
    let mut ours = ours.take(len);
    let mut theirs = std::fs::File::open(other)?.take(len);
    let (mut left, mut right) = (vec![0u8; 256 * 1024], vec![0u8; 256 * 1024]);
    loop {
        let read = theirs.read(&mut right)?;
        if read == 0 {
            return Ok(ours.read(&mut left[..1])? == 0);
        }
        if ours.read_exact(&mut left[..read]).is_err() || left[..read] != right[..read] {
            return Ok(false);
        }
    }
}

pub fn append(path: &Path, keep: u64, extra: &Path) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(keep)?;
    file.seek(SeekFrom::End(0))?;
    std::io::copy(&mut std::fs::File::open(extra)?, &mut file)?;
    file.flush()?;
    file.sync_all()
}

pub fn temp_beside(path: &Path, purpose: &str) -> std::io::Result<PathBuf> {
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(std::io::Error::other)?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{purpose}-{:016x}", u64::from_le_bytes(nonce)));
    Ok(path.with_file_name(name))
}

pub fn remove(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub file: u64,
    pub kernel_state: u64,
    pub kernel_state_age: std::time::Duration,
    pub chunk: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            file: 256 << 20,
            kernel_state: 64 << 20,
            kernel_state_age: std::time::Duration::from_secs(14 * 24 * 60 * 60),
            chunk: 4 << 20,
        }
    }
}

impl Limits {
    #[must_use]
    pub fn allows(&self, key: &str, len: u64) -> bool {
        len <= self.file && (!live_only(key) || len <= self.kernel_state)
    }

    #[must_use]
    pub fn current(&self, key: &str, modified: &Stamp) -> bool {
        if !live_only(key) {
            return true;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
            });
        let age = u64::try_from(now.saturating_sub(modified.mtime_ns)).unwrap_or(0);
        u128::from(age) <= self.kernel_state_age.as_nanos()
    }
}

#[must_use]
pub fn live_only(key: &str) -> bool {
    key.strip_prefix(ARTIFACTS) == Some(KERNEL_STATE)
}

fn transient(name: &str) -> bool {
    name.contains(".tmp-")
        || name.contains(".part-")
        || name.contains(".sync-in-")
        || name.starts_with(&format!("{KERNEL_STATE}.sync-conflict-"))
}

pub fn artifact_files(dir: &Path) -> std::io::Result<BTreeMap<String, (PathBuf, Stamp)>> {
    let mut found = BTreeMap::new();
    let mut pending = vec![(dir.to_owned(), String::new())];
    while let Some((path, prefix)) = pending.pop() {
        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let kind = entry.file_type()?;
            let relative = format!("{prefix}{name}");
            if kind.is_dir() {
                pending.push((entry.path(), format!("{relative}/")));
            } else if kind.is_file()
                && !transient(&name)
                && let Some(stamp) = stamp(&entry.path())?
            {
                found.insert(format!("{ARTIFACTS}{relative}"), (entry.path(), stamp));
            }
        }
    }
    Ok(found)
}

pub fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    mush_ai::private_io::write_private(path, bytes)
}

pub fn copy_durably(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let partial = temp_beside(to, "sync-in")?;
    let copied = (|| {
        let mut source = std::fs::File::open(from)?;
        let mut target = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&partial)?;
        std::fs::set_permissions(
            &partial,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )?;
        std::io::copy(&mut source, &mut target)?;
        target.sync_all()?;
        std::fs::rename(&partial, to)
    })();
    if copied.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    copied
}
