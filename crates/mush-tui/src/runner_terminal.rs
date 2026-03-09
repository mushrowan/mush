use std::io;

use crossterm::ExecutableCommand;
use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{
    self, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

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
        }
    }
}

pub(super) fn enter_tui_terminal() -> io::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    enable_mouse_scroll()?;
    match supports_keyboard_enhancement() {
        Ok(true) => {
            let _ = io::stdout().execute(PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
            ));
        }
        Ok(false) => {
            tracing::debug!("keyboard enhancement unsupported, leaving it disabled");
        }
        Err(error) => {
            tracing::debug!(
                ?error,
                "keyboard enhancement probe failed, leaving it disabled"
            );
        }
    }
    let _ = io::stdout().execute(SetCursorStyle::BlinkingBar);

    std::thread::sleep(std::time::Duration::from_millis(50));
    while event::poll(std::time::Duration::ZERO)? {
        let stale = event::read()?;
        tracing::debug!(?stale, "drained stale event from terminal probe");
    }

    Ok(())
}

pub(super) fn install_panic_cleanup_hook() {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal_state();
        prev_hook(info);
    }));
}

/// enable minimal mouse tracking: clicks + scroll with SGR coordinates
///
/// crossterm's `EnableMouseCapture` also enables `?1003h` (any-event tracking)
/// which floods the event stream with movement events. when events accumulate
/// faster than the TUI polls them, SGR escape sequence fragments can leak
/// through crossterm's parser as spurious key events, causing garbled text
pub(super) fn enable_mouse_scroll() -> io::Result<()> {
    use std::io::Write;

    io::stdout().write_all(b"\x1b[?1000h\x1b[?1006h")?;
    io::stdout().flush()
}

fn disable_mouse_scroll() {
    use std::io::Write;

    let _ = io::stdout().write_all(b"\x1b[?1000l\x1b[?1006l");
    let _ = io::stdout().flush();
}

pub(super) fn restore_terminal_state() {
    use std::io::Write;

    let _ = io::stdout().execute(PopKeyboardEnhancementFlags);
    let _ = io::stdout().execute(SetCursorStyle::DefaultUserShape);
    let _ = disable_raw_mode();
    disable_mouse_scroll();
    let _ = io::stdout().execute(LeaveAlternateScreen);
    let _ = io::stdout().execute(Show);
    let _ = io::stdout().execute(crossterm::style::ResetColor);
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
}

pub(super) fn cleanup(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    restore_terminal_state();
    terminal.show_cursor()?;
    Ok(())
}
