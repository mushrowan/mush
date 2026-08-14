use std::cell::Cell;
use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::app::App;
use crate::ui::{input_height, layout, status_bar_height};
use crate::widgets::input_box::InputBox;
use crate::widgets::message_list::transcript_lines;
use crate::widgets::status_bar::StatusBar;
use crate::widgets::tool_panels::{ToolPanels, tool_panels_height};

#[derive(Debug)]
pub struct Viewport {
    scroll_top: Cell<usize>,
    following: Cell<bool>,
    total: Cell<usize>,
    height: Cell<usize>,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            scroll_top: Cell::new(0),
            following: Cell::new(true),
            total: Cell::new(0),
            height: Cell::new(0),
        }
    }
}

impl Viewport {
    pub fn window(&self, total: usize, height: usize) -> Range<usize> {
        self.total.set(total);
        self.height.set(height);
        let top = if self.following.get() {
            self.max_scroll()
        } else {
            self.scroll_top.get().min(self.max_scroll())
        };
        self.scroll_top.set(top);
        top..total.min(top + height)
    }

    pub fn scroll_by(&self, lines: isize) {
        let top = self.scroll_top.get().saturating_add_signed(lines);
        self.scroll_to(top);
    }

    pub fn to_top(&self) {
        self.scroll_to(0);
    }

    pub fn to_bottom(&self) {
        self.scroll_to(self.max_scroll());
    }

    #[must_use]
    pub fn page(&self) -> usize {
        self.height.get().max(1)
    }

    #[must_use]
    pub fn is_following(&self) -> bool {
        self.following.get()
    }

    fn scroll_to(&self, top: usize) {
        let top = top.min(self.max_scroll());
        self.scroll_top.set(top);
        self.following.set(top >= self.max_scroll());
    }

    fn max_scroll(&self) -> usize {
        self.total.get().saturating_sub(self.height.get())
    }
}

#[derive(Debug, Default)]
pub struct Screen {}

impl Screen {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn render(&mut self, app: &App, area: Rect, buf: &mut Buffer) {
        let tools_h = tool_panels_height(&app.active_tools, area.width);
        let input_h = input_height(&app.input, area.width);
        let status_h = status_bar_height(app, area.width);
        let regions = layout(area, input_h, tools_h, status_h);
        app.input.area.set(regions.input);

        let lines = transcript_lines(app, area.width);
        let window = app
            .viewport
            .window(lines.len(), usize::from(regions.messages.height));
        for (row, line) in regions.messages.rows().zip(&lines[window]) {
            line.render(row, buf);
        }

        if let Some(tools) = regions.tools {
            ToolPanels::new(&app.active_tools, &app.theme).render(tools, buf);
        }
        InputBox::new(app).render(regions.input, buf);
        StatusBar::new(app).render(regions.status, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_following_window_ends_at_the_newest_line() {
        assert_eq!(Viewport::default().window(200, 20), 180..200);
    }

    #[test]
    fn a_transcript_shorter_than_the_window_starts_at_the_top() {
        assert_eq!(Viewport::default().window(6, 20), 0..6);
    }

    #[test]
    fn scrolling_up_pauses_the_follow() {
        let viewport = Viewport::default();
        viewport.window(200, 20);

        viewport.scroll_by(-1);

        assert_eq!(viewport.window(200, 20), 179..199);
        assert!(!viewport.is_following());
    }

    #[test]
    fn scrolling_back_to_the_bottom_follows_again() {
        let viewport = Viewport::default();
        viewport.window(200, 20);
        viewport.scroll_by(-5);

        viewport.scroll_by(5);

        assert!(viewport.is_following());
    }

    #[test]
    fn a_paused_window_stays_put_as_the_transcript_grows() {
        let viewport = Viewport::default();
        viewport.window(200, 20);
        viewport.scroll_by(-10);

        assert_eq!(viewport.window(205, 20), 170..190);
    }
}
