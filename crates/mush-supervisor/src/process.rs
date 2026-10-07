use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::read_json;

#[derive(Clone, Copy)]
pub(crate) struct Process {
    parent: u32,
    pub(crate) start: u64,
    pub(crate) group: u32,
}

pub(crate) fn processes() -> HashMap<u32, Process> {
    let mut out = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(") ") else {
            continue;
        };
        let fields: Vec<_> = rest.split_whitespace().collect();
        if fields.first() == Some(&"Z") {
            continue;
        }
        if let (Some(parent), Some(start)) = (
            fields.get(1).and_then(|s| s.parse().ok()),
            fields.get(19).and_then(|s| s.parse().ok()),
        ) {
            out.insert(
                pid,
                Process {
                    parent,
                    start,
                    group: fields.get(2).and_then(|s| s.parse().ok()).unwrap_or(0),
                },
            );
        }
    }
    out
}

pub(crate) fn descendants(root: u32, tracked: &mut HashMap<u32, u64>) {
    let all = processes();
    if let Some(process) = all.get(&root) {
        tracked.entry(root).or_insert(process.start);
    }
    loop {
        let mut added = false;
        for (pid, process) in &all {
            if !tracked.contains_key(pid)
                && tracked.get(&process.parent).is_some_and(|start| {
                    all.get(&process.parent).is_some_and(|p| p.start == *start)
                })
            {
                tracked.insert(*pid, process.start);
                added = true;
            }
        }
        if !added {
            break;
        }
    }
}

pub(crate) async fn signal_tree(tracked: &HashMap<u32, u64>, signal: &str) {
    let current = processes();
    let pids: Vec<_> = tracked
        .iter()
        .filter(|(pid, start)| current.get(pid).is_some_and(|p| p.start == **start))
        .map(|(pid, _)| pid.to_string())
        .collect();
    if !pids.is_empty() {
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            Command::new("kill")
                .arg(signal)
                .arg("--")
                .args(pids)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
    }
}

pub(crate) async fn kill_recorded(dir: &Path) {
    if let Some(tracked) = read_json(&dir.join("process.json"))
        .and_then(|value| serde_json::from_value::<HashMap<u32, u64>>(value).ok())
    {
        signal_tree(&tracked, "-KILL").await;
    }
}

pub(crate) async fn terminate(child: &mut tokio::process::Child, tracked: &mut HashMap<u32, u64>) {
    if let Some(pid) = child.id() {
        descendants(pid, tracked);
    }
    signal_tree(tracked, "-TERM").await;
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    signal_tree(tracked, "-KILL").await;
    let _ = child.kill().await;
    let _ = child.wait().await;
}
