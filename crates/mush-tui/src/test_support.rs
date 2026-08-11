//! shared test helpers
//!
//! only compiled for tests. lives here rather than in each module's
//! `mod tests` so widget snapshots and app fixtures are written the same
//! way everywhere.

use ratatui::buffer::Buffer;

/// flatten a rendered buffer into newline-separated rows, so a test can
/// assert on what a widget actually painted
pub(crate) fn buffer_to_string(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// draw through a `TestBackend` terminal and return the resulting buffer.
/// needed by overlays that render via `Frame` rather than `Widget`
pub(crate) fn draw_to_buffer(
    width: u16,
    height: u16,
    draw: impl FnOnce(&mut ratatui::Frame<'_>),
) -> Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal.draw(draw).expect("draw");
    terminal.backend().buffer().clone()
}
