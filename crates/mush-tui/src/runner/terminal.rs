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

use crate::terminal_policy::{
    ImageProbeMode, KeyboardEnhancementMode, MouseTrackingMode, TerminalPolicy,
};

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

pub(super) fn probe_image_picker(policy: TerminalPolicy) -> Option<ratatui_image::picker::Picker> {
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
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(event::EnableBracketedPaste)?;
    io::stdout().execute(event::EnableFocusChange)?;
    enable_mouse_tracking(policy.mouse_tracking)?;
    enable_keyboard_enhancement(policy.keyboard_enhancement);
    let _ = io::stdout().execute(SetCursorStyle::BlinkingBar);

    std::thread::sleep(std::time::Duration::from_millis(50));
    while event::poll(std::time::Duration::ZERO)? {
        let stale = event::read()?;
        tracing::debug!(?stale, "drained stale event from terminal probe");
    }

    Ok(())
}

fn enable_keyboard_enhancement(mode: KeyboardEnhancementMode) {
    match mode {
        KeyboardEnhancementMode::Auto => match supports_keyboard_enhancement() {
            Ok(true) => push_keyboard_enhancement(),
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
            push_keyboard_enhancement();
        }
        KeyboardEnhancementMode::Disabled => {
            tracing::debug!("keyboard enhancement disabled by terminal policy");
        }
    }
}

fn push_keyboard_enhancement() {
    if let Err(error) = io::stdout().execute(PushKeyboardEnhancementFlags(
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
    )) {
        tracing::debug!(?error, "failed to enable keyboard enhancement");
    }
}

pub(super) fn install_panic_cleanup_hook() {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal_state();
        prev_hook(info);
    }));
}

/// enable mouse tracking: clicks, scroll, and drag with sgr coordinates
///
/// `?1002h` reports button press/release and motion while a button is held.
/// `?1006h` enables SGR coordinate encoding for large terminals.
/// we avoid `?1003h` (any-event tracking) which floods the event stream
/// with movement events even when no button is held
fn enable_minimal_mouse_tracking() -> io::Result<()> {
    use std::io::Write;

    io::stdout().write_all(b"\x1b[?1002h\x1b[?1006h")?;
    io::stdout().flush()
}

fn enable_mouse_tracking(mode: MouseTrackingMode) -> io::Result<()> {
    match mode {
        MouseTrackingMode::Minimal => enable_minimal_mouse_tracking(),
        MouseTrackingMode::Disabled => {
            tracing::debug!("mouse tracking disabled by terminal policy");
            Ok(())
        }
    }
}

fn disable_mouse_tracking() {
    use std::io::Write;

    let _ = io::stdout().write_all(b"\x1b[?1002l\x1b[?1006l");
    let _ = io::stdout().flush();
}

pub(super) fn restore_terminal_state() {
    use std::io::Write;

    let _ = io::stdout().execute(PopKeyboardEnhancementFlags);
    let _ = io::stdout().execute(event::DisableFocusChange);
    let _ = io::stdout().execute(event::DisableBracketedPaste);
    let _ = io::stdout().execute(SetCursorStyle::DefaultUserShape);
    let _ = disable_raw_mode();
    disable_mouse_tracking();
    let _ = io::stdout().execute(LeaveAlternateScreen);
    let _ = io::stdout().execute(Show);
    let _ = io::stdout().execute(crossterm::style::ResetColor);
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
}

pub(super) fn cleanup(
    terminal: &mut Terminal<super::caching_backend::CachingBackend<CrosstermBackend<io::Stdout>>>,
) -> io::Result<()> {
    restore_terminal_state();
    terminal.show_cursor()?;
    Ok(())
}
