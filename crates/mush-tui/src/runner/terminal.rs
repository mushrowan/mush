use std::io::{self, Write};
#[cfg(unix)]
use std::sync::{Mutex, OnceLock};

use crossterm::QueueableCommand;
use crossterm::cursor::{MoveTo, SetCursorStyle, Show};
use crossterm::event::{
    self, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use crate::terminal_policy::{
    ImageProbeMode, KeyboardEnhancementMode, MouseTrackingMode, TerminalPolicy,
};

#[cfg(unix)]
static ORIGINAL_TERMIOS: OnceLock<Mutex<Option<libc::termios>>> = OnceLock::new();

pub(super) struct TerminalStateGuard {
    active: bool,
}

impl TerminalStateGuard {
    pub(super) fn new() -> Self {
        Self { active: true }
    }

    pub(super) fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for TerminalStateGuard {
    fn drop(&mut self) {
        if self.active {
            restore_terminal_state();
            super::diag::verify_terminal_restored(vec![
                "exit path: TerminalStateGuard::drop (early return or panic unwind)".into(),
            ]);
        }
    }
}

pub(super) fn probe_image_picker(policy: TerminalPolicy) -> Option<ratatui_image::picker::Picker> {
    // never probe on non-tty stdio: `from_query_stdio` writes escape
    // queries and blocks reading the response. on a pipe or broken pty
    // those bytes leak into the output stream and the read can hang
    let env = crate::terminal_policy::TtyEnvironment::detect();
    if !env.is_tty() {
        tracing::debug!(
            reason = ?env.broken_reason(),
            "skipping image protocol probe: not a full tty environment"
        );
        return None;
    }

    match policy.image_probe {
        ImageProbeMode::Auto => match ratatui_image::picker::Picker::from_query_stdio() {
            Ok(picker) => Some(picker),
            Err(error) => {
                tracing::debug!(?error, "image protocol probe failed, leaving it disabled");
                None
            }
        },
        ImageProbeMode::Disabled => {
            tracing::debug!("image protocol probe disabled by terminal policy");
            None
        }
    }
}

pub(super) fn enter_tui_terminal(policy: TerminalPolicy) -> io::Result<()> {
    snapshot_original_terminal_state();
    enable_raw_mode()?;
    apply_tty_modes(&mut io::stdout(), policy)?;

    // give the terminal a moment to react to the enable bursts, then
    // drain any response bytes (cursor probes, keyboard enhancement
    // acknowledgements) that would otherwise surface as spurious events
    std::thread::sleep(std::time::Duration::from_millis(50));
    while event::poll(std::time::Duration::ZERO)? {
        let stale = event::read()?;
        tracing::debug!(?stale, "drained stale event from terminal probe");
    }

    Ok(())
}

/// re-apply the mode enables.
///
/// called after tool executions that may have touched `/dev/tty` or
/// otherwise stripped our modes (e.g. bash running `stty sane`,
/// `reset`, or a pager that pops kitty kbd flags and bracketed paste
/// on exit)
pub(super) fn reapply_tui_modes_after_tool(policy: TerminalPolicy) {
    if let Err(error) = enable_raw_mode() {
        tracing::debug!(?error, "failed to re-enable raw mode after tool");
    }
    if let Err(error) = apply_tty_modes(&mut io::stdout(), policy) {
        tracing::debug!(?error, "failed to reapply tty modes after tool");
    }
}

/// the mode enables that don't depend on raw mode: bracketed paste,
/// focus change, mouse tracking, keyboard enhancement, cursor shape
fn apply_tty_modes(out: &mut impl Write, policy: TerminalPolicy) -> io::Result<()> {
    out.queue(event::EnableBracketedPaste)?;
    out.queue(event::EnableFocusChange)?;
    enable_mouse_tracking(out, policy.mouse_tracking)?;
    enable_keyboard_enhancement(out, policy.keyboard_enhancement);
    out.queue(SetCursorStyle::BlinkingBar)?;
    out.flush()
}

fn enable_keyboard_enhancement(out: &mut impl Write, mode: KeyboardEnhancementMode) {
    match mode {
        KeyboardEnhancementMode::Auto => match supports_keyboard_enhancement() {
            Ok(true) => push_keyboard_enhancement(out),
            Ok(false) => {
                tracing::debug!("keyboard enhancement unsupported, leaving it disabled");
            }
            Err(error) => {
                tracing::debug!(
                    ?error,
                    "keyboard enhancement probe failed, leaving it disabled"
                );
            }
        },
        KeyboardEnhancementMode::Enabled => {
            tracing::debug!("keyboard enhancement forced on by terminal policy");
            push_keyboard_enhancement(out);
        }
        KeyboardEnhancementMode::Disabled => {
            tracing::debug!("keyboard enhancement disabled by terminal policy");
        }
    }
}

fn push_keyboard_enhancement(out: &mut impl Write) {
    if let Err(error) = out.queue(PushKeyboardEnhancementFlags(
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
    )) {
        tracing::debug!(?error, "failed to enable keyboard enhancement");
    }
}

pub(super) fn install_panic_cleanup_hook() {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal_state();
        super::diag::verify_terminal_restored(vec![format!(
            "exit path: panic hook, payload: {}",
            panic_payload_str(info)
        )]);
        prev_hook(info);
    }));
}

fn panic_payload_str(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".into()
    }
}

/// enable mouse tracking: clicks, scroll, and drag with sgr coordinates
///
/// `?1002h` reports button press/release and motion while a button is held.
/// `?1006h` enables SGR coordinate encoding for large terminals.
/// we avoid `?1003h` (any-event tracking) which floods the event stream
/// with movement events even when no button is held
fn enable_mouse_tracking(out: &mut impl Write, mode: MouseTrackingMode) -> io::Result<()> {
    match mode {
        MouseTrackingMode::Minimal => out.write_all(b"\x1b[?1002h\x1b[?1006h"),
        MouseTrackingMode::Disabled => {
            tracing::debug!("mouse tracking disabled by terminal policy");
            Ok(())
        }
    }
}

/// put back everything [`apply_tty_modes`] set
fn restore_tty_modes(out: &mut impl Write) {
    let _ = out.queue(PopKeyboardEnhancementFlags);
    let _ = out.queue(event::DisableFocusChange);
    let _ = out.queue(event::DisableBracketedPaste);
    let _ = out.queue(SetCursorStyle::DefaultUserShape);
    let _ = out.write_all(b"\x1b[?1002l\x1b[?1006l");
    let _ = out.queue(Show);
    let _ = out.queue(crossterm::style::ResetColor);
    let _ = out.flush();
}

pub(super) fn restore_terminal_state() {
    restore_tty_modes(&mut io::stdout());
    let _ = disable_raw_mode();
    restore_original_terminal_state();
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
}

#[cfg(unix)]
fn snapshot_original_terminal_state() {
    let state = ORIGINAL_TERMIOS.get_or_init(|| Mutex::new(None));
    let Ok(mut original) = state.lock() else {
        return;
    };
    if original.is_some() {
        return;
    }
    if let Some(termios) = read_stdin_termios() {
        *original = Some(termios);
    }
}

#[cfg(not(unix))]
fn snapshot_original_terminal_state() {}

#[cfg(unix)]
fn restore_original_terminal_state() {
    let Some(state) = ORIGINAL_TERMIOS.get() else {
        return;
    };
    let Ok(original) = state.lock() else {
        return;
    };
    let Some(termios) = original.as_ref() else {
        return;
    };
    if let Err(error) = set_stdin_termios(termios) {
        tracing::debug!(?error, "failed to restore original stdin termios");
    }
}

#[cfg(not(unix))]
fn restore_original_terminal_state() {}

#[cfg(unix)]
fn read_stdin_termios() -> Option<libc::termios> {
    read_termios(libc::STDIN_FILENO)
}

#[cfg(unix)]
fn read_termios(fd: libc::c_int) -> Option<libc::termios> {
    use std::mem::MaybeUninit;
    let mut termios = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr writes a termios value on success. we only read
    // the value when the call returns zero. fd is provided by libc or test
    // code and tcgetattr validates it before writing.
    let rc = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
    if rc == 0 {
        // SAFETY: the successful tcgetattr call initialised termios.
        Some(unsafe { termios.assume_init() })
    } else {
        None
    }
}

#[cfg(unix)]
fn set_stdin_termios(termios: &libc::termios) -> io::Result<()> {
    set_termios(libc::STDIN_FILENO, termios)
}

#[cfg(unix)]
fn set_termios(fd: libc::c_int, termios: &libc::termios) -> io::Result<()> {
    // SAFETY: termios points to a valid termios snapshot captured from
    // tcgetattr, and tcsetattr validates fd before applying it.
    let rc = unsafe { libc::tcsetattr(fd, libc::TCSANOW, termios) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn quiet_policy() -> TerminalPolicy {
        TerminalPolicy {
            keyboard_enhancement: KeyboardEnhancementMode::Disabled,
            mouse_tracking: MouseTrackingMode::Minimal,
            image_probe: ImageProbeMode::Disabled,
        }
    }

    fn written(write: impl FnOnce(&mut Vec<u8>)) -> String {
        let mut out = Vec::new();
        write(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn entering_the_ui_stays_on_the_main_screen() {
        let out = written(|out| apply_tty_modes(out, quiet_policy()).unwrap());

        assert!(out.contains("\x1b[?2004h"), "bracketed paste: {out:?}");
        assert!(out.contains("\x1b[?1002h"), "mouse tracking: {out:?}");
        assert!(!out.contains("?1049"), "alt screen: {out:?}");
    }

    #[test]
    fn leaving_the_ui_puts_the_modes_back_and_nothing_else() {
        let out = written(restore_tty_modes);

        assert!(out.contains("\x1b[?2004l"), "bracketed paste: {out:?}");
        assert!(out.contains("\x1b[?1002l"), "mouse tracking: {out:?}");
        assert!(!out.contains("?1049"), "alt screen: {out:?}");
    }

    #[test]
    fn the_prompt_lands_below_the_block() {
        let out = written(|out| park_cursor_below(out, Rect::new(0, 10, 40, 6)).unwrap());

        assert_eq!(out, "\x1b[16;1H\r\n");
    }

    #[test]
    fn raw_termios_snapshot_restores_echo_and_canonical_mode() {
        let Some((master_fd, slave_fd)) = open_pty_pair() else {
            return;
        };
        let _master = FdGuard(master_fd);
        let _slave = FdGuard(slave_fd);
        let Some(original) = read_termios(slave_fd) else {
            return;
        };
        let mut rawish = original;
        rawish.c_lflag &= !(libc::ICANON | libc::ECHO);
        set_termios(slave_fd, &rawish).unwrap();

        set_termios(slave_fd, &original).unwrap();

        let restored = read_termios(slave_fd).unwrap();
        assert_ne!(restored.c_lflag & libc::ICANON, 0);
        assert_ne!(restored.c_lflag & libc::ECHO, 0);
    }

    struct FdGuard(libc::c_int);

    impl Drop for FdGuard {
        fn drop(&mut self) {
            // SAFETY: closing a raw fd obtained from open/openpty.
            let _ = unsafe { libc::close(self.0) };
        }
    }

    fn open_pty_pair() -> Option<(libc::c_int, libc::c_int)> {
        let mut master = 0;
        let mut slave = 0;
        // SAFETY: openpty initialises master/slave fds on success. the
        // optional termios, winsize, and name pointers may be null.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if rc == 0 { Some((master, slave)) } else { None }
    }
}

/// leave the block on screen and put the cursor after it, so the shell
/// prompt doesn't land on top of the last thing mush drew
fn park_cursor_below(out: &mut impl Write, block: Rect) -> io::Result<()> {
    out.queue(MoveTo(0, block.bottom().saturating_sub(1)))?;
    out.write_all(b"\r\n")?;
    out.flush()
}

pub(super) fn cleanup(
    terminal: &mut Terminal<
        super::size_cached_backend::SizeCachedBackend<CrosstermBackend<io::Stdout>>,
    >,
    block: Rect,
) -> io::Result<()> {
    park_cursor_below(&mut io::stdout(), block)?;
    restore_terminal_state();
    terminal.show_cursor()?;
    Ok(())
}
