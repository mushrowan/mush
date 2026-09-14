use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use rustix::fs::{Mode, OFlags, openat};
use serde::de::DeserializeOwned;

use super::{MAX_FILES, REQUEST_BYTES, ReadError};

pub(super) struct Budget {
    bytes: u64,
    files: usize,
}

impl Budget {
    pub(super) fn new() -> Self {
        Self {
            bytes: REQUEST_BYTES,
            files: MAX_FILES,
        }
    }
}

pub(super) struct Directory(pub(super) File);

pub(super) fn valid_path(path: &Path) -> bool {
    path.is_absolute()
        && path.as_os_str().len() <= 4096
        && path.components().count() <= 64
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && !path.to_string_lossy().chars().any(char::is_control)
}

impl Directory {
    pub(super) fn workers(&self) -> Result<Vec<String>, ReadError> {
        let mut workers = Vec::new();
        for (index, entry) in rustix::fs::Dir::read_from(&self.0)
            .map_err(open_error)?
            .enumerate()
        {
            if index >= 64 {
                return Err(ReadError::Oversized);
            }
            let entry = entry.map_err(open_error)?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if super::worker_name(name)
                && matches!(
                    entry.file_type(),
                    rustix::fs::FileType::Directory
                        | rustix::fs::FileType::Symlink
                        | rustix::fs::FileType::Unknown
                )
            {
                workers.push(name.to_string());
            }
        }
        workers.sort();
        Ok(workers)
    }

    pub(super) fn open(path: &Path) -> Result<Self, ReadError> {
        if !valid_path(path) {
            return Err(ReadError::Unsafe);
        }
        let mut file = File::open("/").map_err(ReadError::from)?;
        for component in path.components() {
            if let Component::Normal(name) = component {
                file = File::from(
                    openat(
                        &file,
                        name,
                        OFlags::RDONLY
                            | OFlags::DIRECTORY
                            | OFlags::NOFOLLOW
                            | OFlags::NONBLOCK
                            | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(open_error)?,
                );
                let meta = file.metadata()?;
                let uid = rustix::process::geteuid().as_raw();
                if (meta.uid() != uid && meta.uid() != 0)
                    || (meta.mode() & 0o022 != 0 && !(meta.uid() == 0 && meta.mode() & 0o1000 != 0))
                {
                    return Err(ReadError::Unsafe);
                }
            }
        }
        Self::owned(file)
    }

    fn owned(file: File) -> Result<Self, ReadError> {
        let meta = file.metadata()?;
        if !meta.is_dir()
            || meta.uid() != rustix::process::geteuid().as_raw()
            || meta.mode() & 0o022 != 0
        {
            return Err(ReadError::Unsafe);
        }
        Ok(Self(file))
    }

    pub(super) fn child(&self, name: &str) -> Result<Self, ReadError> {
        if !super::worker_name(name) {
            return Err(ReadError::Unsafe);
        }
        let fd = openat(
            &self.0,
            name,
            OFlags::RDONLY
                | OFlags::DIRECTORY
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(open_error)?;
        Self::owned(File::from(fd))
    }

    pub(super) fn json<T: DeserializeOwned>(
        &self,
        name: &str,
        limit: u64,
        budget: &mut Budget,
    ) -> Result<T, ReadError> {
        if budget.files == 0 || budget.bytes == 0 {
            return Err(ReadError::Oversized);
        }
        budget.files -= 1;
        let fd = openat(
            &self.0,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(open_error)?;
        let file = File::from(fd);
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != rustix::process::geteuid().as_raw()
            || meta.mode() & 0o022 != 0
            || meta.nlink() != 1
        {
            return Err(ReadError::Unsafe);
        }
        let limit = limit.min(budget.bytes.saturating_sub(1));
        if meta.len() > limit {
            return Err(ReadError::Oversized);
        }
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        budget.bytes = budget.bytes.saturating_sub(bytes.len() as u64);
        if bytes.len() as u64 > limit {
            return Err(ReadError::Oversized);
        }
        serde_json::from_slice(&bytes).map_err(|_| ReadError::Malformed)
    }
}

pub(super) fn open_error(error: rustix::io::Errno) -> ReadError {
    if error == rustix::io::Errno::LOOP || error == rustix::io::Errno::NOTDIR {
        ReadError::Unsafe
    } else {
        std::io::Error::from(error).into()
    }
}

pub(super) fn read_json<T: DeserializeOwned>(
    path: &Path,
    limit: u64,
    budget: &mut Budget,
) -> Result<T, ReadError> {
    if !valid_path(path) {
        return Err(ReadError::Unsafe);
    }
    let parent = path.parent().ok_or(ReadError::Unsafe)?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or(ReadError::Unsafe)?;
    Directory::open(parent)?.json(name, limit, budget)
}
