use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const STALL_WATCHDOG_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_STALL_THRESHOLD: Duration = Duration::from_secs(2);

pub(super) struct ProcessIdentity {
    pub timestamp: String,
    pub pid: u32,
    pub tty: Option<String>,
    pub term: Option<String>,
    pub term_program: Option<String>,
}

pub(super) struct TerminalStateDump {
    pub identity: ProcessIdentity,
    pub raw_mode_still_enabled: bool,
    pub actual_termios: Option<TermiosProbe>,
    pub notes: Vec<String>,
}

pub(super) struct TerminalStartupDump {
    pub identity: ProcessIdentity,
    pub phases: Vec<StartupPhase>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct StallSnapshot {
    pub phase: String,
    pub detail: String,
    pub loop_iteration: u64,
    pub draw_count: u64,
    pub elapsed: Duration,
    pub active_streams: usize,
    pub active_tools: Vec<String>,
}

struct WatchdogState {
    last_progress: Instant,
    phase: String,
    detail: String,
    loop_iteration: u64,
    draw_count: u64,
    active_streams: usize,
    active_tools: Vec<String>,
    last_dumped_iteration: u64,
    stop: bool,
}

pub(super) struct StallWatchdog {
    state: Option<Arc<Mutex<WatchdogState>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StartupPhase {
    pub name: String,
    pub size: Result<Option<(u16, u16)>, String>,
}

impl StartupPhase {
    pub fn new(name: impl Into<String>, size: io::Result<Option<(u16, u16)>>) -> Self {
        Self {
            name: name.into(),
            size: size.map_err(|error| error.to_string()),
        }
    }
}

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
            identity: ProcessIdentity::capture(),
            raw_mode_still_enabled,
            actual_termios: probe_termios(),
            notes,
        }
    }

    pub fn render(&self) -> String {
        let mut s = String::from("# mush terminal-state dump\n");
        self.identity.push_to(&mut s);
        let _ = writeln!(s, "raw_mode_still_enabled: {}", self.raw_mode_still_enabled);
        match self.actual_termios {
            Some(t) => {
                let _ = writeln!(
                    s,
                    "actual_termios: icanon={} echo={} isig={} looks_raw={}",
                    t.icanon,
                    t.echo,
                    t.isig,
                    t.looks_raw()
                );
            }
            None => s.push_str("actual_termios: unavailable\n"),
        }
        push_notes(&mut s, &self.notes);
        s.push_str(
            "\nrestore sequence attempted:\n\
             - PopKeyboardEnhancementFlags (\\e[<u)\n\
             - DisableFocusChange (\\e[?1004l)\n\
             - DisableBracketedPaste (\\e[?2004l)\n\
             - SetCursorStyle::DefaultUserShape (\\e[0 q)\n\
             - disable_raw_mode (tcsetattr)\n\
             - Show cursor (\\e[?25h)\n\
             - ResetColor (\\e[0m)\n",
        );
        s
    }
}

impl TerminalStartupDump {
    pub fn capture(phases: Vec<StartupPhase>, notes: Vec<String>) -> Self {
        Self {
            identity: ProcessIdentity::capture(),
            phases,
            notes,
        }
    }

    pub fn render(&self) -> String {
        let mut s = String::from("# mush terminal-startup dump\n");
        self.identity.push_to(&mut s);
        s.push_str("\nstartup phases:\n");
        for phase in &self.phases {
            let _ = match &phase.size {
                Ok(Some((width, height))) => {
                    writeln!(s, "- {}: size={}x{}", phase.name, width, height)
                }
                Ok(None) => writeln!(s, "- {}: size=not checked", phase.name),
                Err(error) => writeln!(s, "- {}: size_error={}", phase.name, error),
            };
        }
        push_notes(&mut s, &self.notes);
        s
    }
}

impl StallSnapshot {
    pub fn render(&self) -> String {
        let mut s = String::from("# mush tui stall dump\n");
        let _ = writeln!(s, "timestamp: {}", current_timestamp());
        let _ = writeln!(s, "pid: {}", std::process::id());
        let _ = writeln!(s, "phase: {}", self.phase);
        let _ = writeln!(s, "detail: {}", self.detail);
        let _ = writeln!(s, "loop_iteration: {}", self.loop_iteration);
        let _ = writeln!(s, "draw_count: {}", self.draw_count);
        let _ = writeln!(s, "elapsed_ms: {}", self.elapsed.as_millis());
        let _ = writeln!(s, "active_streams: {}", self.active_streams);
        s.push_str("active_tools:\n");
        if self.active_tools.is_empty() {
            s.push_str("- none\n");
        } else {
            for tool in &self.active_tools {
                let _ = writeln!(s, "- {tool}");
            }
        }
        s
    }
}

impl ProcessIdentity {
    fn capture() -> Self {
        Self {
            timestamp: current_timestamp(),
            pid: std::process::id(),
            tty: current_tty(),
            term: std::env::var("TERM").ok(),
            term_program: std::env::var("TERM_PROGRAM").ok(),
        }
    }

    fn file_name(&self, prefix: &str) -> String {
        format!("{prefix}-{}-{}.txt", self.timestamp, self.pid)
    }

    fn push_to(&self, s: &mut String) {
        let _ = writeln!(s, "timestamp: {}", self.timestamp);
        let _ = writeln!(s, "pid: {}", self.pid);
        let _ = writeln!(s, "tty: {}", self.tty.as_deref().unwrap_or("unknown"));
        let _ = writeln!(s, "term: {}", self.term.as_deref().unwrap_or("unset"));
        let _ = writeln!(
            s,
            "term_program: {}",
            self.term_program.as_deref().unwrap_or("unset")
        );
    }
}

fn push_notes(s: &mut String, notes: &[String]) {
    if notes.is_empty() {
        return;
    }
    s.push_str("\nnotes:\n");
    for note in notes {
        let _ = writeln!(s, "- {note}");
    }
}

impl StallWatchdog {
    pub fn disabled() -> Self {
        Self {
            state: None,
            handle: None,
        }
    }

    pub fn from_env() -> Self {
        if std::env::var_os("MUSH_STALL_WATCHDOG").is_none() {
            return Self::disabled();
        }
        let threshold = std::env::var("MUSH_STALL_THRESHOLD_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_STALL_THRESHOLD);
        let dir = std::env::var_os("MUSH_STALL_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("MUSH_TRACE_DIR").map(PathBuf::from))
            .unwrap_or_else(default_stall_dir);
        Self::start(dir, threshold)
    }

    pub fn start(dir: PathBuf, threshold: Duration) -> Self {
        let state = Arc::new(Mutex::new(WatchdogState {
            last_progress: Instant::now(),
            phase: "startup".into(),
            detail: String::new(),
            loop_iteration: 0,
            draw_count: 0,
            active_streams: 0,
            active_tools: Vec::new(),
            last_dumped_iteration: u64::MAX,
            stop: false,
        }));
        let thread_state = state.clone();
        let handle = std::thread::Builder::new()
            .name("mush-stall-watchdog".into())
            .spawn(move || run_stall_watchdog(thread_state, dir, threshold))
            .ok();
        Self {
            state: Some(state),
            handle,
        }
    }

    pub fn progress(&self, phase: &'static str, detail: impl Into<String>, snapshot: LoopSnapshot) {
        let Some(state) = &self.state else {
            return;
        };
        if let Ok(mut state) = state.lock() {
            state.last_progress = Instant::now();
            state.phase = phase.into();
            state.detail = detail.into();
            state.loop_iteration = snapshot.loop_iteration;
            state.draw_count = snapshot.draw_count;
            state.active_streams = snapshot.active_streams;
            state.active_tools = snapshot.active_tools;
        }
    }
}

impl Drop for StallWatchdog {
    fn drop(&mut self) {
        if let Some(state) = &self.state
            && let Ok(mut state) = state.lock()
        {
            state.stop = true;
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub(super) struct LoopSnapshot {
    pub loop_iteration: u64,
    pub draw_count: u64,
    pub active_streams: usize,
    pub active_tools: Vec<String>,
}

pub(super) fn write_terminal_state_dump(
    dir: &Path,
    dump: &TerminalStateDump,
) -> io::Result<PathBuf> {
    let name = dump.identity.file_name("mush-term");
    write_dump(dir, &name, &dump.render())
}

pub(super) fn write_terminal_startup_dump(
    dir: &Path,
    dump: &TerminalStartupDump,
) -> io::Result<PathBuf> {
    let name = dump.identity.file_name("mush-startup");
    write_dump(dir, &name, &dump.render())
}

pub(super) fn write_stall_dump(dir: &Path, dump: &StallSnapshot) -> io::Result<PathBuf> {
    let name = format!(
        "mush-stall-{}-{}-{}.txt",
        current_timestamp(),
        std::process::id(),
        dump.loop_iteration
    );
    write_dump(dir, &name, &dump.render())
}

fn write_dump(dir: &Path, name: &str, contents: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(name);
    write_owner_only(&path, contents.as_bytes())?;
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
    let fd: std::os::unix::io::RawFd = 0;
    let mut t = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr writes a termios into the pointer on success.
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

pub(super) fn default_dump_dir() -> PathBuf {
    mush_session::data_dir().join("terminal-dumps")
}

fn default_stall_dir() -> PathBuf {
    mush_session::data_dir().join("stall-dumps")
}

fn run_stall_watchdog(state: Arc<Mutex<WatchdogState>>, dir: PathBuf, threshold: Duration) {
    loop {
        std::thread::sleep(STALL_WATCHDOG_INTERVAL);
        let Some(snapshot) = stalled_snapshot(&state, threshold) else {
            if state.lock().map(|state| state.stop).unwrap_or(true) {
                break;
            }
            continue;
        };
        match write_stall_dump(&dir, &snapshot) {
            Ok(path) => {
                tracing::warn!(
                    dump = %path.display(),
                    elapsed_ms = snapshot.elapsed.as_millis(),
                    phase = %snapshot.phase,
                    detail = %snapshot.detail,
                    loop_iteration = snapshot.loop_iteration,
                    draw_count = snapshot.draw_count,
                    active_streams = snapshot.active_streams,
                    active_tools = ?snapshot.active_tools,
                    "tui loop stall detected"
                );
            }
            Err(error) => {
                tracing::warn!(?error, "failed to write tui stall dump");
            }
        }
    }
}

fn stalled_snapshot(
    state: &Arc<Mutex<WatchdogState>>,
    threshold: Duration,
) -> Option<StallSnapshot> {
    let now = Instant::now();
    let mut state = state.lock().ok()?;
    if state.stop {
        return None;
    }
    let elapsed = now.duration_since(state.last_progress);
    if elapsed < threshold || state.last_dumped_iteration == state.loop_iteration {
        return None;
    }
    if is_idle_event_wait(&state) {
        return None;
    }
    state.last_dumped_iteration = state.loop_iteration;
    Some(StallSnapshot {
        phase: state.phase.clone(),
        detail: state.detail.clone(),
        loop_iteration: state.loop_iteration,
        draw_count: state.draw_count,
        elapsed,
        active_streams: state.active_streams,
        active_tools: state.active_tools.clone(),
    })
}

fn is_idle_event_wait(state: &WatchdogState) -> bool {
    state.phase == "waiting_for_event" && state.active_streams == 0 && state.active_tools.is_empty()
}

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

pub(super) fn write_startup_diagnostics(phases: Vec<StartupPhase>, mut notes: Vec<String>) {
    let suspicious_size = phases
        .iter()
        .any(|phase| matches!(phase.size, Ok(Some((0, _))) | Ok(Some((_, 0))) | Err(_)));
    if !suspicious_size {
        return;
    }

    notes.push("terminal reported suspicious size during startup".into());
    let dump = TerminalStartupDump::capture(phases, notes);
    let dir = default_dump_dir();
    match write_terminal_startup_dump(&dir, &dump) {
        Ok(path) => {
            tracing::warn!(
                dump = %path.display(),
                "suspicious terminal state during startup; diagnostics written"
            );
        }
        Err(error) => {
            tracing::warn!(?error, "failed to write terminal startup diagnostics");
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
            identity: ProcessIdentity {
                timestamp: "20260420T153045Z".into(),
                pid: 42,
                tty: Some("/dev/pts/9".into()),
                term: Some("xterm-ghostty".into()),
                term_program: Some("ghostty".into()),
            },
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
    fn stall_dump_includes_loop_state() {
        let dump = StallSnapshot {
            phase: "draw".into(),
            detail: "draw_panes".into(),
            loop_iteration: 42,
            draw_count: 7,
            elapsed: Duration::from_millis(3500),
            active_streams: 1,
            active_tools: vec!["bash: cargo test".into()],
        };

        let rendered = dump.render();

        assert!(rendered.contains("phase: draw"));
        assert!(rendered.contains("detail: draw_panes"));
        assert!(rendered.contains("loop_iteration: 42"));
        assert!(rendered.contains("draw_count: 7"));
        assert!(rendered.contains("elapsed_ms: 3500"));
        assert!(rendered.contains("active_streams: 1"));
        assert!(rendered.contains("- bash: cargo test"));
    }

    #[test]
    fn stalled_snapshot_fires_once_per_loop_iteration() {
        let state = Arc::new(Mutex::new(WatchdogState {
            last_progress: Instant::now() - Duration::from_secs(3),
            phase: "select".into(),
            detail: "waiting".into(),
            loop_iteration: 9,
            draw_count: 4,
            active_streams: 1,
            active_tools: Vec::new(),
            last_dumped_iteration: u64::MAX,
            stop: false,
        }));

        let first = stalled_snapshot(&state, Duration::from_secs(2));
        let second = stalled_snapshot(&state, Duration::from_secs(2));

        assert!(first.is_some());
        assert!(second.is_none());

        state.lock().unwrap().loop_iteration = 10;
        assert!(stalled_snapshot(&state, Duration::from_secs(2)).is_some());
    }

    #[test]
    fn stalled_snapshot_ignores_idle_event_waits() {
        let state = Arc::new(Mutex::new(WatchdogState {
            last_progress: Instant::now() - Duration::from_secs(30),
            phase: "waiting_for_event".into(),
            detail: "parked before select/input handling".into(),
            loop_iteration: 9,
            draw_count: 4,
            active_streams: 0,
            active_tools: Vec::new(),
            last_dumped_iteration: u64::MAX,
            stop: false,
        }));

        assert!(
            stalled_snapshot(&state, Duration::from_secs(2)).is_none(),
            "normal event-loop parking should not produce a stall dump"
        );
    }

    #[test]
    fn stalled_snapshot_reports_waits_with_active_work() {
        let state = Arc::new(Mutex::new(WatchdogState {
            last_progress: Instant::now() - Duration::from_secs(30),
            phase: "waiting_for_event".into(),
            detail: "streaming select".into(),
            loop_iteration: 9,
            draw_count: 4,
            active_streams: 1,
            active_tools: Vec::new(),
            last_dumped_iteration: u64::MAX,
            stop: false,
        }));

        assert!(
            stalled_snapshot(&state, Duration::from_secs(2)).is_some(),
            "active streams should still be reported if the loop stops making progress"
        );
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
        assert_eq!(dump.identity.pid, std::process::id());
        assert!(!dump.raw_mode_still_enabled);
        assert!(!dump.identity.timestamp.is_empty());
    }

    #[test]
    fn startup_dump_records_terminal_init_phases() {
        let tmp = tempfile::tempdir().unwrap();
        let dump = TerminalStartupDump {
            identity: ProcessIdentity {
                timestamp: "20260514T213000Z".into(),
                pid: 42,
                tty: Some("/dev/pts/9".into()),
                term: Some("xterm-ghostty".into()),
                term_program: Some("ghostty".into()),
            },
            phases: vec![
                StartupPhase::new("before_restore", Ok(Some((120, 40)))),
                StartupPhase::new("after_enter", Ok(Some((0, 0)))),
            ],
            notes: vec!["terminal reported zero size during startup".into()],
        };

        let path = write_terminal_startup_dump(tmp.path(), &dump).unwrap();
        let contents = fs::read_to_string(&path).unwrap();

        assert!(contents.contains("# mush terminal-startup dump"));
        assert!(contents.contains("before_restore: size=120x40"));
        assert!(contents.contains("after_enter: size=0x0"));
        assert!(contents.contains("terminal reported zero size"));

        #[cfg(unix)]
        assert_eq!(path.metadata().unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn startup_diagnostics_ignore_normal_sizes() {
        let phases = [
            StartupPhase::new("before", Ok(Some((120, 40)))),
            StartupPhase::new("after", Ok(Some((120, 40)))),
        ];

        let suspicious = phases
            .iter()
            .any(|phase| matches!(phase.size, Ok(Some((0, _))) | Ok(Some((_, 0))) | Err(_)));

        assert!(!suspicious);
    }
}
