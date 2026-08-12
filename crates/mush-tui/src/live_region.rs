//! where the redrawn part of the inline ui sits on screen

use ratatui::layout::{Rect, Size};

/// the block at the bottom of the screen that mush redraws each frame.
///
/// settled history is printed above it and then belongs to the terminal,
/// so the region tracks only its own top row and height
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveRegion {
    screen: Size,
    top: u16,
    height: u16,
}

impl LiveRegion {
    #[must_use]
    pub fn new(screen: Size, top: u16) -> Self {
        Self {
            screen,
            top,
            height: 0,
        }
    }

    #[must_use]
    pub fn area(&self) -> Rect {
        Rect::new(0, self.top, self.screen.width, self.height)
    }

    #[must_use]
    pub fn screen(&self) -> Size {
        self.screen
    }

    /// the last row of the screen, where a newline scrolls
    #[must_use]
    pub fn bottom_row(&self) -> u16 {
        self.screen.height.saturating_sub(1)
    }

    /// put the block back where it was on a screen of a different size,
    /// unplaced, so the next frame draws it from scratch
    pub fn reset(&mut self, screen: Size) {
        self.screen = screen;
        self.top = self.top.min(self.bottom_row());
        self.height = 0;
    }

    /// put a block `height` tall below `lines` of history just printed at
    /// the top of the old one, and say how far the caller still has to
    /// scroll the screen to fit it
    pub fn advance(&mut self, lines: u16, height: u16) -> u16 {
        let cursor = self.top.saturating_add(lines).min(self.bottom_row());
        self.height = height.min(self.screen.height);
        let overflow = cursor
            .saturating_add(self.height)
            .saturating_sub(self.screen.height);
        self.top = cursor - overflow;
        overflow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_pushes_the_block_down_the_screen() {
        let mut region = LiveRegion::new(Size::new(80, 24), 0);
        assert_eq!(region.advance(0, 4), 0);
        assert_eq!(region.area(), Rect::new(0, 0, 80, 4));

        assert_eq!(region.advance(5, 4), 0);
        assert_eq!(region.area(), Rect::new(0, 5, 80, 4));
    }

    #[test]
    fn a_block_against_the_bottom_scrolls_the_screen_instead() {
        let mut region = LiveRegion::new(Size::new(80, 24), 0);
        region.advance(20, 4);
        assert_eq!(region.area(), Rect::new(0, 20, 80, 4));

        // printing five lines from row 20 scrolled the screen twice on
        // its own, and the block is still three rows short
        assert_eq!(region.advance(5, 4), 3);
        assert_eq!(region.area(), Rect::new(0, 20, 80, 4));
    }

    #[test]
    fn a_growing_block_scrolls_once_it_reaches_the_bottom() {
        let mut region = LiveRegion::new(Size::new(80, 24), 0);
        region.advance(10, 4);
        assert_eq!(region.area(), Rect::new(0, 10, 80, 4));

        // rows 10..18 are free, so it grows in place
        assert_eq!(region.advance(0, 8), 0);
        assert_eq!(region.area(), Rect::new(0, 10, 80, 8));

        // 20 rows from row 10 overshoots the screen by 6
        assert_eq!(region.advance(0, 20), 6);
        assert_eq!(region.area(), Rect::new(0, 4, 80, 20));
    }

    #[test]
    fn a_block_taller_than_the_screen_fills_it() {
        let mut region = LiveRegion::new(Size::new(80, 24), 0);
        region.advance(10, 4);

        assert_eq!(region.advance(0, 30), 10);
        assert_eq!(region.area(), Rect::new(0, 0, 80, 24));
    }
}

/// tripwires on the ratatui gaps that make mush own the history writes.
///
/// each one passes while the gap is open and fails when upstream closes
/// it, so a version bump forces us back here to delete code
#[cfg(test)]
mod upstream_gaps {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::text::Line;
    use ratatui::widgets::Widget;
    use ratatui::{Terminal, TerminalOptions, Viewport};

    /// when this fails, drop `Scrollback`'s own emission and call
    /// `insert_before`
    #[test]
    fn insert_before_ignores_a_fixed_viewport() {
        let mut terminal = Terminal::with_options(
            TestBackend::new(20, 10),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 5, 20, 5)),
            },
        )
        .unwrap();

        terminal
            .insert_before(1, |buf| Line::raw("settled").render(buf.area, buf))
            .unwrap();

        let screen = terminal.backend().buffer().content();
        let text: String = screen.iter().map(|c| c.symbol()).collect();
        assert!(
            !text.contains("settled"),
            "ratatui now writes history for a Fixed viewport, so mush should not"
        );
    }
}
