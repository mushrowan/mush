use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub rss_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_bytes: Option<u64>,
}

#[must_use]
pub fn memory(pid: u32) -> Option<Memory> {
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        let kib = |field: &str| {
            status.lines().find_map(|line| {
                line.strip_prefix(field)?
                    .trim()
                    .strip_suffix("kB")?
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
        };
        return Some(Memory {
            rss_bytes: kib("VmRSS:")? * 1024,
            swap_bytes: kib("VmSwap:").map(|swap| swap * 1024),
        });
    }
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let rss = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()?;
    output.status.success().then_some(Memory {
        rss_bytes: rss * 1024,
        swap_bytes: None,
    })
}

#[must_use]
pub fn has_children(pid: u32) -> Option<bool> {
    proc_children(pid).or_else(|| pgrep_children(pid))
}

fn proc_children(pid: u32) -> Option<bool> {
    let mut found = false;
    for task in std::fs::read_dir(format!("/proc/{pid}/task")).ok()? {
        let children = std::fs::read_to_string(task.ok()?.path().join("children")).ok()?;
        found |= !children.trim().is_empty();
    }
    Some(found)
}

fn pgrep_children(pid: u32) -> Option<bool> {
    let status = std::process::Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .status()
        .ok()?;
    match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_reports_its_resident_memory() {
        let memory = memory(std::process::id()).unwrap();
        assert!(memory.rss_bytes > 0);
    }

    #[test]
    fn a_process_with_a_live_child_has_children_and_one_without_does_not() {
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        assert_eq!(has_children(std::process::id()), Some(true));
        assert_eq!(has_children(sleeper.id()), Some(false));
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
    }

    #[test]
    fn a_missing_process_has_no_memory_reading() {
        assert_eq!(memory(u32::MAX), None);
    }
}
