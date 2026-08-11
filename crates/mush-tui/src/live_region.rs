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
    pub fn new(screen: Size, height: u16) -> Self {
        Self {
            screen,
            top: 0,
            height: height.min(screen.height),
        }
    }

    #[must_use]
    pub fn area(&self) -> Rect {
        Rect::new(0, self.top, self.screen.width, self.height)
    }

    /// move down past `lines` of settled history, and say how far the
    /// screen had to scroll to fit them
    pub fn emit(&mut self, lines: u16) -> u16 {
        let wanted = self.top.saturating_add(lines);
        self.settle(wanted)
    }

    /// resize the region, and say how far the screen had to scroll to fit it
    pub fn set_height(&mut self, height: u16) -> u16 {
        self.height = height.min(self.screen.height);
        self.settle(self.top)
    }

    /// pull `wanted` back up to the highest row the region still fits at
    fn settle(&mut self, wanted: u16) -> u16 {
        let floor = self.screen.height.saturating_sub(self.height);
        self.top = wanted.min(floor);
        wanted - self.top
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settled_lines_push_the_live_region_down_the_screen() {
        let mut region = LiveRegion::new(Size::new(80, 24), 4);
        assert_eq!(region.area(), Rect::new(0, 0, 80, 4));

        assert_eq!(region.emit(5), 0);
        assert_eq!(region.area(), Rect::new(0, 5, 80, 4));
    }

    #[test]
    fn a_live_region_against_the_bottom_scrolls_the_screen_instead() {
        let mut region = LiveRegion::new(Size::new(80, 24), 4);
        region.emit(20);
        assert_eq!(region.area(), Rect::new(0, 20, 80, 4));

        // five more lines want rows 20..25, and only 20..24 are left
        assert_eq!(region.emit(5), 5);
        assert_eq!(region.area(), Rect::new(0, 20, 80, 4));
    }

    #[test]
    fn a_growing_live_region_scrolls_once_it_reaches_the_bottom() {
        let mut region = LiveRegion::new(Size::new(80, 24), 4);
        region.emit(10);
        assert_eq!(region.area(), Rect::new(0, 10, 80, 4));

        // rows 10..18 are free, so it grows in place
        assert_eq!(region.set_height(8), 0);
        assert_eq!(region.area(), Rect::new(0, 10, 80, 8));

        // 20 rows from row 10 overshoots the screen by 6
        assert_eq!(region.set_height(20), 6);
        assert_eq!(region.area(), Rect::new(0, 4, 80, 20));
    }

    #[test]
    fn a_live_region_taller_than_the_screen_fills_it() {
        let mut region = LiveRegion::new(Size::new(80, 24), 4);
        region.emit(10);

        assert_eq!(region.set_height(30), 10);
        assert_eq!(region.area(), Rect::new(0, 0, 80, 24));
    }
}

/// tripwires on the ratatui gaps that make mush own the history writes.
///
/// each one passes while the gap is open and fails when upstream closes
/// it, so a version bump forces us back here to delete code
#[cfg(test)]
mod upstream_gaps {
    use ratatui::backend::{Backend, TestBackend};
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
