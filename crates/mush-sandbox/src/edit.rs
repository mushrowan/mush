use crate::{Error, Result, Sandbox};
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

const LIMIT: u64 = 16 * 1024 * 1024;
const NO_SYMLINKS: u64 = 0x04;
const BENEATH: u64 = 0x08;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn open_at(directory: i32, path: &Path, flags: i32, resolve: u64) -> Result<File> {
    let name =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::EditDenied(path.into()))?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve,
    };
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory,
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if descriptor < 0 {
        return Err(Error::EditOpen {
            path: path.into(),
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(unsafe { File::from_raw_fd(descriptor as i32) })
}

pub(super) fn edit_file(sandbox: &Sandbox, path: &Path, old: &str, new: &str) -> Result<()> {
    if old.is_empty() {
        return Err(Error::EditEmpty);
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(Error::EditTraversal(path.into()));
    }
    if !path.is_absolute() {
        return Err(Error::EditDenied(path.into()));
    }
    let root = sandbox
        .write
        .iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
        .ok_or_else(|| Error::EditDenied(path.into()))?;
    let mut file = if path == root {
        open_at(
            libc::AT_FDCWD,
            path,
            libc::O_RDWR | libc::O_NONBLOCK,
            NO_SYMLINKS,
        )?
    } else {
        let directory = open_at(
            libc::AT_FDCWD,
            root,
            libc::O_PATH | libc::O_DIRECTORY,
            NO_SYMLINKS,
        )?;
        let relative = path
            .strip_prefix(root)
            .map_err(|_| Error::EditDenied(path.into()))?;
        open_at(
            directory.as_raw_fd(),
            relative,
            libc::O_RDWR | libc::O_NONBLOCK,
            NO_SYMLINKS | BENEATH,
        )?
    };
    let metadata = file.metadata().map_err(Error::EditIo)?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(Error::EditFileType);
    }
    if metadata.len() > LIMIT {
        return Err(Error::EditTooLarge);
    }
    let mut content = String::new();
    (&mut file)
        .take(LIMIT + 1)
        .read_to_string(&mut content)
        .map_err(Error::EditIo)?;
    if content.len() as u64 > LIMIT {
        return Err(Error::EditTooLarge);
    }
    let count = content.matches(old).count();
    if count != 1 {
        return Err(Error::EditMatches(count));
    }
    let replacement = content.replacen(old, new, 1);
    if replacement.len() as u64 > LIMIT {
        return Err(Error::EditTooLarge);
    }
    file.seek(SeekFrom::Start(0)).map_err(Error::EditIo)?;
    file.write_all(replacement.as_bytes())
        .map_err(Error::EditIo)?;
    file.set_len(replacement.len() as u64)
        .map_err(Error::EditIo)?;
    Ok(())
}
