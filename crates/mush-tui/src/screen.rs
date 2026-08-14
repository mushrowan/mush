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

#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    scroll_top: usize,
    following: bool,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            scroll_top: 0,
            following: true,
        }
    }
}

impl Viewport {
    pub fn window(&mut self, total: usize, height: usize) -> Range<usize> {
        let max_scroll = total.saturating_sub(height);
        if self.following {
            self.scroll_top = max_scroll;
        } else {
            self.scroll_top = self.scroll_top.min(max_scroll);
        }
        self.scroll_top..total.min(self.scroll_top + height)
    }
}

#[derive(Debug, Default)]
pub struct Screen {
    viewport: Viewport,
}

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
        let window = self
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
}
