use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, openat, renameat, unlinkat};

use super::files::{Budget, Directory, open_error, valid_path};
use super::{MAX_ROOTS, ReadError, Registration};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationStore {
    data_dir: PathBuf,
    session_id: String,
    cwd: PathBuf,
}

impl RegistrationStore {
    pub fn new(data_dir: PathBuf, session_id: String, cwd: PathBuf) -> Result<Self, ReadError> {
        if !crate::inbox::identifier(&session_id) || !valid_path(&data_dir) || !valid_path(&cwd) {
            return Err(ReadError::Unsafe);
        }
        Ok(Self {
            data_dir,
            session_id,
            cwd,
        })
    }

    pub fn empty(&self) -> Registration {
        Registration {
            version: 1,
            session_id: self.session_id.clone(),
            cwd: self.cwd.clone(),
            roots: Vec::new(),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.data_dir.join("subagent-panels").join(self.filename())
    }

    fn filename(&self) -> String {
        format!("{}.json", self.session_id)
    }

    pub fn load(&self) -> Result<Registration, ReadError> {
        let directory = Directory::open(&self.data_dir)?.child("subagent-panels")?;
        let registration = directory.json(&self.filename(), 64 * 1024, &mut Budget::new())?;
        self.validate(&registration)?;
        Ok(registration)
    }

    fn validate(&self, value: &Registration) -> Result<(), ReadError> {
        if value.version != 1 || value.session_id != self.session_id || value.cwd != self.cwd {
            return Err(ReadError::Identity);
        }
        if value.roots.len() > MAX_ROOTS {
            return Err(ReadError::Oversized);
        }
        let mut roots = HashSet::new();
        if value.roots.iter().any(|r| {
            !valid_path(&r.directory)
                || !roots.insert(&r.directory)
                || r.review_queue
                    .as_ref()
                    .is_some_and(|q| !valid_path(&q.path))
        }) {
            return Err(ReadError::Unsafe);
        }
        value.validate_bindings()
    }

    pub fn update(
        &self,
        edit: impl FnOnce(&mut Registration) -> Result<(), ReadError>,
    ) -> Result<Registration, ReadError> {
        let data = Directory::open(&self.data_dir)?;
        match mkdirat(&data.0, "subagent-panels", Mode::RWXU) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(open_error(error)),
        }
        let directory = data.child("subagent-panels")?;
        let lock_name = format!("{}.lock", self.session_id);
        let lock = File::from(
            openat(
                &directory.0,
                &lock_name,
                OFlags::RDWR
                    | OFlags::CREATE
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(open_error)?,
        );
        use std::os::unix::fs::MetadataExt;
        let meta = lock.metadata()?;
        if !meta.is_file()
            || meta.nlink() != 1
            || meta.uid() != rustix::process::geteuid().as_raw()
            || meta.mode() & 0o077 != 0
        {
            return Err(ReadError::Unsafe);
        }
        lock.try_lock().map_err(|_| ReadError::Unavailable)?;
        let mut registration = match directory.json(&self.filename(), 64 * 1024, &mut Budget::new())
        {
            Ok(value) => value,
            Err(ReadError::Missing) => self.empty(),
            Err(error) => return Err(error),
        };
        self.validate(&registration)?;
        edit(&mut registration)?;
        self.validate(&registration)?;
        let bytes = serde_json::to_vec(&registration).map_err(|_| ReadError::Malformed)?;
        if bytes.len() > 64 * 1024 {
            return Err(ReadError::Oversized);
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let temporary = format!(
            "{}.{}.{}.pending",
            self.session_id,
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let fd = openat(
            &directory.0,
            &temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(open_error)?;
        let result = (|| {
            let mut file = File::from(fd);
            file.write_all(&bytes)?;
            file.sync_all()?;
            renameat(&directory.0, &temporary, &directory.0, self.filename())
                .map_err(open_error)?;
            directory.0.sync_all()?;
            Ok::<_, ReadError>(())
        })();
        if result.is_err() {
            let _ = unlinkat(&directory.0, &temporary, AtFlags::empty());
        }
        result?;
        Ok(registration)
    }
}
