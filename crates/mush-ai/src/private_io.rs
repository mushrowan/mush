//! file writers with private permissions
//!
//! `~/.local/share/mush/` contains secrets: oauth tokens, session
//! conversations (with potential api keys pasted by the user), stream
//! error dumps (which may echo request bodies), tool output captures.
//! all writes that create or overwrite those files should land with
//! mode 0o600 so another local user on the machine can't read them.
//!
//! use [`write_private`] wherever mush writes a long-lived file under
//! the data dir. ephemeral temp files (test fixtures, unit tests) are
//! unaffected

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// write `contents` to `path` via an atomic, private replacement.
///
/// the writer creates a private temp file in the target directory,
/// fsyncs it, then renames it over the destination. a crash mid-write
/// leaves either the old complete file or the new complete file, never
/// an empty/truncated destination from unlink-then-create. on unix the
/// temp file is created with mode 0o600. on other platforms we still use
/// the atomic temp-and-rename path, but do not manage platform ACLs.
pub fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    atomic_write(path, contents.as_ref(), true)
}

/// write non-secret state via the same atomic replacement path as
/// [`write_private`]. use this for long-lived config/cache files where
/// durability matters but mode 0o600 is not required.
pub fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    atomic_write(path, contents.as_ref(), false)
}

fn atomic_write(path: &Path, contents: &[u8], private: bool) -> io::Result<()> {
    atomic_write_with_temp_path(path, &temp_path_for(path), contents, private)
}

fn atomic_write_with_temp_path(
    path: &Path,
    temp_path: &Path,
    contents: &[u8],
    private: bool,
) -> io::Result<()> {
    let parent = parent_dir(path);
    std::fs::create_dir_all(parent)?;

    let result = write_temp_file(temp_path, contents, private)
        .and_then(|()| std::fs::rename(temp_path, path))
        .and_then(|()| sync_parent_dir(parent));

    if result.is_err() {
        let _ = std::fs::remove_file(temp_path);
    }

    result
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

#[cfg(test)]
pub fn atomic_write_for_test(
    path: &Path,
    temp_path: &Path,
    contents: &[u8],
    private: bool,
) -> io::Result<()> {
    atomic_write_with_temp_path(path, temp_path, contents, private)
}

fn temp_path_for(path: &Path) -> PathBuf {
    let file_name = path.file_name().map_or_else(
        || std::ffi::OsString::from("tmp"),
        std::ffi::OsStr::to_os_string,
    );
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let suffix = format!(".tmp-{}-{counter}-{nanos}", std::process::id());
    let mut temp_name = file_name;
    temp_name.push(suffix);
    path.with_file_name(temp_name)
}

fn write_temp_file(path: &Path, contents: &[u8], private: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        if private {
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        Ok(())
    }
}

fn sync_parent_dir(parent: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(parent)?.sync_all()
    }

    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn write_private_creates_with_mode_600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        write_private(&path, b"{\"token\":\"shh\"}").unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600, got {mode:o}");
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"token\":\"shh\"}");
    }

    #[test]
    fn write_private_overwrites_existing_file_with_mode_600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rotated.json");
        // pre-existing file with loose permissions
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private(&path, b"new").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "rewriting a world-readable file must tighten the mode to 0o600, got {mode:o}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn write_private_leaves_existing_file_when_replacement_cannot_be_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let temp_path = dir.path().join("credentials.json.tmp-test");
        write_private(&path, b"old-token").unwrap();
        std::fs::write(&temp_path, b"blocked temp path").unwrap();

        let err = atomic_write_with_temp_path(&path, &temp_path, b"new-token", true).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"old-token");
    }

    #[test]
    fn write_atomic_overwrites_without_forcing_private_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");

        write_atomic(&path, b"scope = 'global'\n").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"scope = 'global'\n");
    }

    #[test]
    fn write_atomic_handles_relative_file_without_parent_component() {
        let dir = tempfile::tempdir().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let result = write_atomic(Path::new("conversation.md"), b"# transcript\n");

        std::env::set_current_dir(old_cwd).unwrap();
        result.unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("conversation.md")).unwrap(),
            b"# transcript\n"
        );
    }
}
