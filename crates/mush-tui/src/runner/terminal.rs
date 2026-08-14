use std::io::{self, Write};
#[cfg(unix)]
use std::sync::{Mutex, OnceLock};

use crossterm::QueueableCommand;
use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{
    self, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Size;

use crate::app::App;
use crate::inline::Inline;

use crate::terminal_policy::{ImageProbeMode, KeyboardEnhancementMode, TerminalPolicy};

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

    std::thread::sleep(std::time::Duration::from_millis(50));
    while event::poll(std::time::Duration::ZERO)? {
        let stale = event::read()?;
        tracing::debug!(?stale, "drained stale event from terminal probe");
    }

    Ok(())
}

pub(super) fn reapply_tui_modes_after_tool(policy: TerminalPolicy) {
    if let Err(error) = enable_raw_mode() {
        tracing::debug!(?error, "failed to re-enable raw mode after tool");
    }
    if let Err(error) = apply_tty_modes(&mut io::stdout(), policy) {
        tracing::debug!(?error, "failed to reapply tty modes after tool");
    }
}

fn apply_tty_modes(out: &mut impl Write, policy: TerminalPolicy) -> io::Result<()> {
    out.queue(event::EnableBracketedPaste)?;
    out.queue(event::EnableFocusChange)?;
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

fn restore_tty_modes(out: &mut impl Write) {
    let _ = out.queue(PopKeyboardEnhancementFlags);
    let _ = out.queue(event::DisableFocusChange);
    let _ = out.queue(event::DisableBracketedPaste);
    let _ = out.queue(SetCursorStyle::DefaultUserShape);
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
        assert!(!out.contains("?1049"), "alt screen: {out:?}");
        assert!(
            !out.contains("?1002"),
            "the wheel is the terminal's: {out:?}"
        );
    }

    #[test]
    fn leaving_the_ui_puts_the_modes_back_and_nothing_else() {
        let out = written(restore_tty_modes);

        assert!(out.contains("\x1b[?2004l"), "bracketed paste: {out:?}");
        assert!(!out.contains("?1049"), "alt screen: {out:?}");
    }

    #[test]
    fn a_terminal_that_wont_say_where_the_cursor_is_opens_at_the_bottom() {
        let screen = Size::new(80, 24);

        assert_eq!(opening_row(Ok((0, 7)), screen), 7);
        assert_eq!(opening_row(Err(io::Error::other("no reply")), screen), 23);
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

pub(super) fn opening_row(position: io::Result<(u16, u16)>, screen: Size) -> u16 {
    match position {
        Ok((_, row)) => row,
        Err(error) => {
            tracing::debug!(?error, "no cursor position, opening at the bottom");
            screen.height.saturating_sub(1)
        }
    }
}

pub(super) fn cleanup(
    terminal: &mut Terminal<
        super::size_cached_backend::SizeCachedBackend<CrosstermBackend<io::Stdout>>,
    >,
    inline: &mut Inline,
    app: &App,
) -> io::Result<()> {
    inline.finish(app, &mut io::stdout())?;
    restore_terminal_state();
    terminal.show_cursor()?;
    Ok(())
}
