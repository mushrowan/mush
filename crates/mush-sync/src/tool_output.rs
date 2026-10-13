use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::engine::{Direction, Engine, RemoteEntry, Report, SyncError};
use crate::local::{SessionState, Stamp, stamp};
use crate::protocol::{Commit, FileRef, manifest_aad, now_ms};
use crate::remote::Committed;

pub const TOOL_OUTPUT: &str = "_tool_output";
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const PREFIX: &str = "tool-output/";
const DIR_KEY: &str = ".";

#[must_use]
pub fn saved_output(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".txt") else {
        return false;
    };
    let digits = stem
        .strip_prefix("tool_")
        .or_else(|| stem.strip_prefix("rejected_"))
        .unwrap_or_default();
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn modified_ms(stamp: &Stamp) -> u64 {
    u64::try_from(stamp.mtime_ns / 1_000_000).unwrap_or(0)
}

fn scan(
    dir: &Path,
    cutoff: u64,
    limit: u64,
) -> std::io::Result<BTreeMap<String, (PathBuf, Stamp)>> {
    let mut found = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !saved_output(&name) {
            continue;
        }
        if let Some(now) = stamp(&entry.path())?
            && modified_ms(&now) >= cutoff
            && now.len <= limit
        {
            found.insert(name, (entry.path(), now));
        }
    }
    Ok(found)
}

fn set_modified(path: &Path, modified_ms: u64) -> std::io::Result<()> {
    let at = std::time::UNIX_EPOCH + Duration::from_millis(modified_ms);
    std::fs::File::options()
        .write(true)
        .open(path)?
        .set_modified(at)
}

impl Engine {
    pub(crate) async fn reconcile_tool_output(
        &self,
        fed: Option<RemoteEntry>,
        direction: Direction,
        report: &mut Report,
    ) -> Result<(), SyncError> {
        let dir = self.layout().tool_output.clone();
        let sid = self.keys().sid(TOOL_OUTPUT);
        let mut state = SessionState::load(self.layout(), TOOL_OUTPUT)?.unwrap_or_default();
        let mut theirs = fed;
        if theirs.is_none() && state.remote_version > state.version {
            theirs = self.fetch_entry(&sid).await?;
        }
        let dir_stamp = stamp_dir(&dir)?;
        if theirs.is_none()
            && state.remote_version <= state.version
            && dir_stamp.is_some()
            && state.stamps.get(DIR_KEY) == dir_stamp.as_ref()
        {
            return Ok(());
        }
        let retention = u64::try_from(RETENTION.as_millis()).unwrap_or(u64::MAX);
        for _ in 0..4 {
            if theirs
                .as_ref()
                .is_some_and(|entry| entry.head.version <= state.version)
            {
                theirs = None;
            }
            let cutoff = now_ms().saturating_sub(retention);
            let local = scan(&dir, cutoff, self.limits().tool_output)?;
            let remote = theirs
                .as_ref()
                .map(|entry| entry.manifest.clone())
                .or_else(|| state.manifest.clone())
                .unwrap_or_default();
            let version = theirs
                .as_ref()
                .map_or(state.version, |entry| entry.head.version);
            let mut pulled = 0usize;
            if direction == Direction::Both {
                for (key, file) in &remote.files {
                    let Some(name) = key.strip_prefix(PREFIX).filter(|name| saved_output(name))
                    else {
                        continue;
                    };
                    let fresh = file.modified_ms.is_some_and(|at| at >= cutoff);
                    let path = dir.join(name);
                    if !fresh || local.contains_key(name) || path.exists() {
                        continue;
                    }
                    if file.len > self.limits().tool_output {
                        continue;
                    }
                    self.fetch_blob_into(TOOL_OUTPUT, &sid, file, &path).await?;
                    if let Some(at) = file.modified_ms {
                        set_modified(&path, at)?;
                    }
                    pulled += 1;
                }
            }
            let mut manifest = remote.clone();
            manifest.session = TOOL_OUTPUT.to_owned();
            manifest
                .files
                .retain(|_, file| file.modified_ms.is_some_and(|at| at >= cutoff));
            let mut stamps: BTreeMap<String, Stamp> = BTreeMap::new();
            let mut uploads = Vec::new();
            for (name, (path, now)) in &local {
                let key = format!("{PREFIX}{name}");
                if manifest.files.contains_key(&key) {
                    stamps.insert(key, *now);
                    continue;
                }
                let (blob, len) = match (state.stamps.get(&key), state.file(&key)) {
                    (Some(seen), Some(file)) if seen == now => (file.blob.clone(), file.len),
                    _ => self.keys().blob_id_of(path)?,
                };
                uploads.push((path.clone(), blob.clone(), len));
                manifest.files.insert(
                    key.clone(),
                    FileRef {
                        blob,
                        len,
                        modified_ms: Some(modified_ms(now)),
                    },
                );
                stamps.insert(key, *now);
            }
            let changed = manifest.files != remote.files;
            if !changed {
                state.version = version;
                state.remote_version = version;
                state.manifest = Some(remote);
                state.stamps = stamps;
                if let Some(now) = dir_stamp {
                    state.stamps.insert(DIR_KEY.to_owned(), now);
                }
                state.save(self.layout(), TOOL_OUTPUT)?;
                if pulled > 0 {
                    report.pulled.push(TOOL_OUTPUT.to_owned());
                }
                return Ok(());
            }
            for (path, blob, len) in &uploads {
                self.upload_file(&sid, blob, path, *len).await?;
            }
            manifest.writer = self.machine().to_owned();
            manifest.written_at = now_ms();
            let sealed = self.keys().seal(
                &manifest_aad(&sid, version + 1),
                &serde_json::to_vec(&manifest)?,
            )?;
            let commit = Commit {
                expect: version,
                holder: self.machine().to_owned(),
                upload: "tooloutput".to_owned(),
                generation: 0,
                seq: 0,
                blobs: manifest
                    .files
                    .values()
                    .map(|file| file.blob.clone())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                manifest: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    sealed,
                ),
                deleted: false,
            };
            match self.remote().commit(&sid, &commit).await? {
                Committed::Accepted(head) => {
                    state.version = head.version;
                    state.remote_version = head.version;
                    state.manifest = Some(manifest);
                    state.stamps = stamps;
                    if let Some(now) = dir_stamp {
                        state.stamps.insert(DIR_KEY.to_owned(), now);
                    }
                    state.save(self.layout(), TOOL_OUTPUT)?;
                    report.pushed.push(TOOL_OUTPUT.to_owned());
                    if pulled > 0 {
                        report.pulled.push(TOOL_OUTPUT.to_owned());
                    }
                    return Ok(());
                }
                Committed::Conflict(head) => theirs = self.open_head(head)?,
                Committed::Locked { .. } => {
                    return Err(SyncError::Io(std::io::Error::other(
                        "the shared tool output list is locked on the server",
                    )));
                }
            }
        }
        report.waiting.push((
            TOOL_OUTPUT.to_owned(),
            "kept changing during the pass".to_owned(),
        ));
        Ok(())
    }
}

fn stamp_dir(dir: &Path) -> std::io::Result<Option<Stamp>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => Ok(Some(Stamp {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_saved_tool_output_names_are_synced() {
        for name in ["tool_1791357088823.txt", "rejected_1791357088823.txt"] {
            assert!(saved_output(name), "{name}");
        }
        for name in [
            "oauth.json",
            "tool_.txt",
            "tool_12a.txt",
            "tool_123.txt.sync-in-ab",
            "notes.txt",
            "../tool_1.txt",
        ] {
            assert!(!saved_output(name), "{name}");
        }
    }
}
