//! terminal-state leak diagnostics
//!
//! after `run_tui` exits we sometimes see the host terminal left in a
//! broken mode (raw input stuck on, mouse tracking still reporting,
//! kitty kbd flags still pushed, etc). this module captures what we
//! can observe at exit time and persists it to a per-session dump file
//! so users can attach it when reporting the bug.
//!
//! dumps land under `<data_dir>/terminal-dumps/mush-term-<ts>-<pid>.txt`
//! with mode 0o600. nothing secret is logged, but the dir also holds
//! future diagnostics and secrets-adjacent context (`$TERM_PROGRAM`
//! can leak the user's terminal emulator), so we default to owner-only

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(super) struct TerminalStateDump {
    pub timestamp: String,
    pub pid: u32,
    pub tty: Option<String>,
    pub term: Option<String>,
    pub term_program: Option<String>,
    pub raw_mode_still_enabled: bool,
    pub actual_termios: Option<TermiosProbe>,
    pub notes: Vec<String>,
}

/// snapshot of the real tty line discipline via `tcgetattr`. when
/// `icanon` or `echo` are false we know the underlying tty is in a
/// raw-ish state regardless of what crossterm's own flag says
#[derive(Debug, Clone, Copy)]
pub(super) struct TermiosProbe {
    pub icanon: bool,
    pub echo: bool,
    pub isig: bool,
}

impl TermiosProbe {
    pub fn looks_raw(&self) -> bool {
        !self.icanon || !self.echo
    }
}

impl TerminalStateDump {
    pub fn capture(raw_mode_still_enabled: bool, notes: Vec<String>) -> Self {
        Self {
            timestamp: current_timestamp(),
            pid: std::process::id(),
            tty: current_tty(),
            term: std::env::var("TERM").ok(),
            term_program: std::env::var("TERM_PROGRAM").ok(),
            raw_mode_still_enabled,
            actual_termios: probe_termios(),
            notes,
        }
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str("# mush terminal-state dump\n");
        s.push_str(&format!("timestamp: {}\n", self.timestamp));
        s.push_str(&format!("pid: {}\n", self.pid));
        s.push_str(&format!(
            "tty: {}\n",
            self.tty.as_deref().unwrap_or("unknown")
        ));
        s.push_str(&format!(
            "term: {}\n",
            self.term.as_deref().unwrap_or("unset")
        ));
        s.push_str(&format!(
            "term_program: {}\n",
            self.term_program.as_deref().unwrap_or("unset")
        ));
        s.push_str(&format!(
            "raw_mode_still_enabled: {}\n",
            self.raw_mode_still_enabled
        ));
        match self.actual_termios {
            Some(t) => {
                s.push_str(&format!(
                    "actual_termios: icanon={} echo={} isig={} looks_raw={}\n",
                    t.icanon,
                    t.echo,
                    t.isig,
                    t.looks_raw()
                ));
            }
            None => {
                s.push_str("actual_termios: unavailable\n");
            }
        }
        if !self.notes.is_empty() {
            s.push_str("\nnotes:\n");
            for note in &self.notes {
                s.push_str(&format!("- {note}\n"));
            }
        }
        s.push_str(
            "\nrestore sequence attempted:\n\
             - PopKeyboardEnhancementFlags (\\e[<u)\n\
             - DisableFocusChange (\\e[?1004l)\n\
             - DisableBracketedPaste (\\e[?2004l)\n\
             - SetCursorStyle::DefaultUserShape (\\e[0 q)\n\
             - disable_raw_mode (tcsetattr)\n\
             - disable_mouse_tracking (\\e[?1002l\\e[?1006l)\n\
             - LeaveAlternateScreen (\\e[?1049l)\n\
             - Show cursor (\\e[?25h)\n\
             - ResetColor (\\e[0m)\n",
        );
        s
    }
}

pub(super) fn write_terminal_state_dump(
    dir: &Path,
    dump: &TerminalStateDump,
) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("mush-term-{}-{}.txt", dump.timestamp, dump.pid));
    write_owner_only(&path, dump.render().as_bytes())?;
    Ok(path)
}

#[cfg(unix)]
fn write_owner_only(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    use std::io::Write;
    f.write_all(contents)
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, contents: &[u8]) -> io::Result<()> {
    fs::write(path, contents)
}

fn current_timestamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn current_tty() -> Option<String> {
    // best-effort: read /proc/self/fd/0 on linux; on other unices skip
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link("/proc/self/fd/0")
            .ok()
            .map(|p| p.display().to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(unix)]
fn probe_termios() -> Option<TermiosProbe> {
    use std::mem::MaybeUninit;
    // probe stdin; if stdin isn't a tty (piped) this returns ENOTTY
    // and we just report "unavailable". we don't open /dev/tty here
    // because that would open a new handle and we want the truth about
    // the fd we were actually reading from
    let fd: std::os::unix::io::RawFd = 0;
    let mut t = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr writes a termios into the pointer on success.
    // we only read the returned struct if the call succeeded
    let rc = unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let t = unsafe { t.assume_init() };
    Some(TermiosProbe {
        icanon: (t.c_lflag & libc::ICANON) != 0,
        echo: (t.c_lflag & libc::ECHO) != 0,
        isig: (t.c_lflag & libc::ISIG) != 0,
    })
}

#[cfg(not(unix))]
fn probe_termios() -> Option<TermiosProbe> {
    None
}

/// location for terminal-state dumps. lives alongside other mush data
pub(super) fn default_dump_dir() -> PathBuf {
    mush_session::data_dir().join("terminal-dumps")
}

/// run after restore_terminal_state. if raw mode is still reporting
/// enabled, the terminal is likely broken from the user's POV: dump
/// what we know to a file, log a warning, and echo the dump path to
/// stderr so the user sees it in their shell right after mush exits
pub(super) fn verify_terminal_restored(extra_notes: Vec<String>) {
    let raw_still = match crossterm::terminal::is_raw_mode_enabled() {
        Ok(v) => v,
        Err(error) => {
            tracing::debug!(?error, "is_raw_mode_enabled check failed; skipping dump");
            return;
        }
    };
    let termios = probe_termios();
    let termios_raw = termios.map(|t| t.looks_raw()).unwrap_or(false);
    if !raw_still && !termios_raw {
        return;
    }

    let mut notes = extra_notes;
    if raw_still {
        notes.push("cleanup completed but crossterm raw-mode flag still set".into());
    }
    if termios_raw {
        notes.push(
            "tcgetattr on stdin reports ICANON or ECHO off after restore (real tty raw state)"
                .into(),
        );
    }
    let dump = TerminalStateDump::capture(raw_still, notes);
    let dir = default_dump_dir();
    match write_terminal_state_dump(&dir, &dump) {
        Ok(path) => {
            tracing::warn!(
                dump = %path.display(),
                "terminal left in raw-ish state after restore; diagnostics written"
            );
            eprintln!(
                "\nmush: terminal may be left in a broken state.\n\
                 diagnostics written to: {}\n\
                 run `reset` or `stty sane` to recover.",
                path.display()
            );
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                "failed to write terminal-state dump; terminal may be broken"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn dump_file_is_created_with_owner_only_perms() {
        let tmp = tempfile::tempdir().unwrap();
        let dump = TerminalStateDump {
            timestamp: "20260420T153045Z".into(),
            pid: 42,
            tty: Some("/dev/pts/9".into()),
            term: Some("xterm-ghostty".into()),
            term_program: Some("ghostty".into()),
            raw_mode_still_enabled: true,
            actual_termios: Some(TermiosProbe {
                icanon: false,
                echo: false,
                isig: true,
            }),
            notes: vec!["restore_terminal_state completed without errors".into()],
        };
        let path = write_terminal_state_dump(tmp.path(), &dump).unwrap();

        assert!(path.exists(), "dump file should exist at {path:?}");
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("mush-term-"),
            "dump filename should be prefixed with mush-term-"
        );

        let meta = fs::metadata(&path).unwrap();
        let mode = meta.mode() & 0o777;
        assert_eq!(mode, 0o600, "dump file should be 0o600, was {mode:o}");

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("raw_mode_still_enabled: true"));
        assert!(contents.contains("pid: 42"));
        assert!(contents.contains("tty: /dev/pts/9"));
        assert!(contents.contains("term: xterm-ghostty"));
        assert!(
            contents.contains("actual_termios: icanon=false echo=false isig=true looks_raw=true")
        );
        assert!(contents.contains("restore_terminal_state completed without errors"));
    }

    #[test]
    fn termios_probe_looks_raw_when_icanon_or_echo_off() {
        let raw = TermiosProbe {
            icanon: false,
            echo: true,
            isig: true,
        };
        assert!(raw.looks_raw());
        let also_raw = TermiosProbe {
            icanon: true,
            echo: false,
            isig: true,
        };
        assert!(also_raw.looks_raw());
        let cooked = TermiosProbe {
            icanon: true,
            echo: true,
            isig: true,
        };
        assert!(!cooked.looks_raw());
    }

    #[test]
    fn capture_uses_current_process_pid() {
        let dump = TerminalStateDump::capture(false, vec![]);
        assert_eq!(dump.pid, std::process::id());
        assert!(!dump.raw_mode_still_enabled);
        assert!(!dump.timestamp.is_empty());
    }
}
