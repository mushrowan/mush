use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::FromRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::inbox::{identifier, private_directory, read_private, replace};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    pub capacity: usize,
    pub root_reserved: usize,
    pub external_reserved: usize,
    pub accounting_complete: bool,
    pub control_state_protected: bool,
    pub participants: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub run: PathBuf,
    pub worker: String,
    pub pid: u32,
    pub boot_id: String,
    pub processes: std::collections::HashMap<u32, u64>,
    pub state: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    claims: BTreeMap<String, Claim>,
}

pub struct Pool {
    directory: PathBuf,
    _lock: File,
    policy: Policy,
    ledger: Ledger,
}

fn open_private_lock(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    read_private(path, 0)?;
    Ok(file)
}

fn exclusive(file: File, busy: &str) -> io::Result<File> {
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => io::Error::new(io::ErrorKind::WouldBlock, busy),
        std::fs::TryLockError::Error(error) => error,
    })?;
    Ok(file)
}

pub fn execution_lock(directory: &Path, workspace: &Path) -> io::Result<File> {
    private_directory(directory)?;
    let canonical = std::fs::canonicalize(workspace)?;
    let key = crate::review::digest(canonical.as_os_str().as_encoded_bytes());
    let path = directory.join(format!("workspace-{key}.lock"));
    let file = open_private_lock(&path)?;
    exclusive(file, "workspace is busy")
}

pub fn claim_lock(directory: &Path, id: &str) -> io::Result<File> {
    if !identifier(id) {
        return Err(io::Error::other("invalid admission claim"));
    }
    private_directory(directory)?;
    let path = directory.join(format!("claim-{id}.lock"));
    let file = open_private_lock(&path)?;
    exclusive(file, "claim still owns an execution")
}

pub fn session_lock(directory: &Path, session: &str, inherited: Option<i32>) -> io::Result<File> {
    if !identifier(session) {
        return Err(io::Error::other("invalid session identity"));
    }
    private_directory(directory)?;
    let path = directory.join(format!("{session}.lease"));
    let file = open_private_lock(&path)?;
    let lock = if let Some(fd) = inherited {
        if fd < 3 {
            return Err(io::Error::other("invalid session lease descriptor"));
        }
        let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicated < 0 {
            return Err(io::Error::last_os_error());
        }
        let inherited = unsafe { File::from_raw_fd(duplicated) };
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let expected = file.metadata()?;
        let actual = inherited.metadata()?;
        if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
            return Err(io::Error::other("session lease identity changed"));
        }
        inherited
    } else {
        file
    };
    exclusive(lock, "session is busy")
}

impl Pool {
    pub fn open(directory: &Path) -> io::Result<Self> {
        private_directory(directory)?;
        let lock = open_private_lock(&directory.join("admission.lock"))?;
        lock.try_lock().map_err(io::Error::other)?;
        let policy: Policy =
            crate::review::decode(&read_private(&directory.join("policy.json"), 64 * 1024)?)?;
        if policy.version != 1
            || policy.capacity == 0
            || policy.capacity > 1024
            || policy.root_reserved == 0
            || !policy.accounting_complete
            || !policy.control_state_protected
            || policy
                .root_reserved
                .saturating_add(policy.external_reserved)
                > policy.capacity
            || policy.participants.len() > 128
        {
            return Err(io::Error::other("incomplete admission accounting"));
        }
        let ledger = match read_private(&directory.join("claims.json"), 1024 * 1024) {
            Ok(bytes) => crate::review::decode(&bytes)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ledger::default(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            directory: directory.into(),
            _lock: lock,
            policy,
            ledger,
        })
    }

    pub fn claim(&mut self, id: &str, run: &Path, worker: &str) -> io::Result<bool> {
        if !identifier(id)
            || !self
                .policy
                .participants
                .contains(&std::fs::canonicalize(run)?)
        {
            return Err(io::Error::other("run is outside admission policy"));
        }
        if self.ledger.claims.contains_key(id) {
            return Err(io::Error::other(
                "launch claim already exists, reconcile before retry",
            ));
        }
        if self
            .ledger
            .claims
            .values()
            .any(|c| c.run == run && c.worker == worker && c.state != "released")
        {
            return Err(io::Error::other("run has an unresolved launch claim"));
        }
        if self.ledger.claims.len() >= 1024 {
            return Err(io::Error::other("admission history is full"));
        }
        let active = self
            .ledger
            .claims
            .values()
            .filter(|c| c.state != "released")
            .count();
        if active + self.policy.root_reserved + self.policy.external_reserved
            >= self.policy.capacity
        {
            return Ok(false);
        }
        self.ledger.claims.insert(
            id.into(),
            Claim {
                run: run.into(),
                worker: worker.into(),
                pid: std::process::id(),
                boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
                    .trim()
                    .into(),
                processes: Default::default(),
                state: "reserved".into(),
            },
        );
        self.save()?;
        Ok(true)
    }

    pub fn release(&mut self, id: &str) -> io::Result<()> {
        let claim = self
            .ledger
            .claims
            .get_mut(id)
            .ok_or_else(|| io::Error::other("unknown launch claim"))?;
        claim.state = "released".into();
        self.save()
    }

    pub fn started(
        &mut self,
        id: &str,
        pid: u32,
        processes: &std::collections::HashMap<u32, u64>,
    ) -> io::Result<()> {
        let claim = self
            .ledger
            .claims
            .get_mut(id)
            .ok_or_else(|| io::Error::other("unknown launch claim"))?;
        claim.pid = pid;
        claim.processes = processes.clone();
        claim.state = "started".into();
        self.save()
    }

    pub fn status(&self) -> io::Result<serde_json::Value> {
        Ok(serde_json::json!({"policy":self.policy,"claims":self.ledger.claims}))
    }

    pub fn claims(&self) -> &BTreeMap<String, Claim> {
        &self.ledger.claims
    }

    fn save(&self) -> io::Result<()> {
        replace(
            &self.directory.join("claims.json"),
            &serde_json::to_vec(&self.ledger)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "the supervisor reads processes and boot id from /proc"
    )]
    fn independent_runs_share_capacity_and_unresolved_claims() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let policy = Policy {
            version: 1,
            capacity: 3,
            root_reserved: 1,
            external_reserved: 1,
            accounting_complete: true,
            control_state_protected: true,
            participants: vec![first.clone(), second.clone()],
        };
        replace(
            &dir.path().join("policy.json"),
            &serde_json::to_vec(&policy).unwrap(),
        )
        .unwrap();
        let mut pool = Pool::open(dir.path()).unwrap();
        assert!(Pool::open(dir.path()).is_err());
        assert!(pool.claim("one", &first, "producer").unwrap());
        assert!(!pool.claim("two", &second, "producer").unwrap());
        drop(pool);
        let mut pool = Pool::open(dir.path()).unwrap();
        assert!(!pool.claim("two", &second, "producer").unwrap());
        assert!(pool.claim("other", &first, "producer").is_err());
        pool.release("one").unwrap();
        assert!(pool.claim("one", &first, "producer").is_err());
        assert!(pool.claim("two", &second, "producer").unwrap());
        assert_eq!(pool.claims().len(), 2);
        drop(pool);
        std::fs::set_permissions(
            dir.path().join("policy.json"),
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        assert!(Pool::open(dir.path()).is_err());
    }

    #[test]
    fn accounting_and_authority_are_required() {
        for field in [
            "root_reserved",
            "accounting_complete",
            "control_state_protected",
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut value = serde_json::json!({"version":1,"capacity":10,"root_reserved":1,"external_reserved":9,"accounting_complete":true,"control_state_protected":true,"participants":[]});
            value[field] = if field == "root_reserved" {
                0.into()
            } else {
                false.into()
            };
            replace(
                &dir.path().join("policy.json"),
                &serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
            assert!(Pool::open(dir.path()).is_err());
        }
    }
}
