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
use std::path::Path;

/// write `contents` to `path`, creating or truncating with mode 0o600
/// on unix. on windows, falls back to [`std::fs::write`] (no mode bits).
/// unlike `fs::write`, this does not set the permission on an existing
/// file that the caller is overwriting, because we want a new file to
/// be 0o600 at creation time (no race window where another user could
/// read it between create and chmod)
pub fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;

        // remove first so OpenOptions's mode takes effect on existing files
        // (umask aside, open(2) with mode only applies when creating)
        let _ = std::fs::remove_file(path);
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(contents.as_ref())?;
        f.flush()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
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
}
