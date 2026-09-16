use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Record {
    version: u8,
    pid: u32,
    owner_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start: Option<String>,
    active: bool,
    recorded_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReapReport {
    pub killed: Vec<u32>,
    pub gone: Vec<u32>,
    pub kept: usize,
}

#[derive(Debug, Clone)]
pub struct OrphanJournal {
    path: PathBuf,
    owner_pid: u32,
    owner_start: Option<String>,
}

impl OrphanJournal {
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        let owner_pid = std::process::id();
        Self {
            path: path.into(),
            owner_pid,
            owner_start: process_start_id(owner_pid),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, pid: u32, active: bool) {
        let record = Record {
            version: 1,
            pid,
            owner_pid: self.owner_pid,
            owner_start: self.owner_start.clone(),
            start: active.then(|| process_start_id(pid)).flatten(),
            active,
            recorded_at: now_secs(),
        };
        let Ok(mut line) = serde_json::to_string(&record) else {
            return;
        };
        line.push('\n');
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let appended = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
        if let Err(error) = appended {
            tracing::debug!(%error, path = %self.path.display(), "orphan journal append failed");
        }
    }

    pub fn reap_abandoned(&self) -> ReapReport {
        let mut report = ReapReport::default();
        let records = self.latest_records();
        let mut kept = Vec::new();
        for record in records.into_values() {
            if !record.active {
                continue;
            }
            if owner_alive(&record) {
                report.kept += 1;
                kept.push(record);
                continue;
            }
            match &record.start {
                Some(start) if process_start_id(record.pid).as_deref() == Some(start) => {
                    kill_tree(record.pid);
                    report.killed.push(record.pid);
                }
                _ => report.gone.push(record.pid),
            }
        }
        self.rewrite(&kept);
        report
    }

    fn latest_records(&self) -> std::collections::BTreeMap<u32, Record> {
        let mut latest = std::collections::BTreeMap::new();
        let Ok(contents) = std::fs::read_to_string(&self.path) else {
            return latest;
        };
        for line in contents.lines() {
            if let Ok(record) = serde_json::from_str::<Record>(line)
                && record.version == 1
                && record.pid > 0
            {
                latest.insert(record.pid, record);
            }
        }
        latest
    }

    fn rewrite(&self, records: &[Record]) {
        if records.is_empty() {
            let _ = std::fs::remove_file(&self.path);
            return;
        }
        let mut contents = String::new();
        for record in records {
            if let Ok(line) = serde_json::to_string(record) {
                contents.push_str(&line);
                contents.push('\n');
            }
        }
        let _ = std::fs::write(&self.path, contents);
    }
}

fn owner_alive(record: &Record) -> bool {
    match (&record.owner_start, process_start_id(record.owner_pid)) {
        (Some(expected), Some(actual)) => *expected == actual,
        (None, Some(_)) => true,
        (_, None) => false,
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[must_use]
pub fn process_start_id(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        let rest = stat.rsplit_once(')')?.1;
        let start = rest.split_whitespace().nth(19)?;
        return Some(format!("proc:{start}"));
    }
    let output = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let started = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !started.is_empty()).then(|| format!("ps:{started}"))
}

#[cfg(unix)]
fn kill_tree(pid: u32) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    unsafe {
        if libc::kill(-pid, libc::SIGKILL) != 0 {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_tree(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal() -> (tempfile::TempDir, OrphanJournal) {
        let dir = tempfile::tempdir().unwrap();
        let journal = OrphanJournal::at(dir.path().join("orphans.jsonl"));
        (dir, journal)
    }

    fn sleeper() -> std::process::Child {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn the_current_process_has_a_start_id() {
        assert!(process_start_id(std::process::id()).is_some());
        assert_eq!(process_start_id(0), None);
    }

    #[test]
    fn a_live_owner_keeps_its_records_and_a_finished_job_is_dropped() {
        let (_dir, journal) = journal();
        let mut child = sleeper();
        journal.record(child.id(), true);
        journal.record(4_000_000, true);
        journal.record(4_000_000, false);

        let report = journal.reap_abandoned();

        assert_eq!(
            report,
            ReapReport {
                killed: vec![],
                gone: vec![],
                kept: 1
            }
        );
        assert!(journal.path().is_file());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn a_dead_owner_gets_its_still_running_jobs_killed() {
        let (_dir, journal) = journal();
        let mut child = sleeper();
        let mut abandoned = OrphanJournal::at(journal.path());
        abandoned.owner_pid = 4_000_001;
        abandoned.owner_start = Some("proc:1".into());
        abandoned.record(child.id(), true);
        abandoned.record(4_000_002, true);

        let report = journal.reap_abandoned();

        assert_eq!(report.killed, vec![child.id()]);
        assert_eq!(report.gone, vec![4_000_002]);
        assert_eq!(report.kept, 0);
        assert!(!journal.path().exists());
        let status = child.wait().unwrap();
        assert!(!status.success());
    }
}
