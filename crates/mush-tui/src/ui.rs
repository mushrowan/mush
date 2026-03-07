//! main layout and rendering

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::Widget;

use crate::app::{App, AppMode};
use crate::widgets::input_box::InputBox;
use crate::widgets::message_list::MessageList;
use crate::widgets::search_popup::SearchPopup;
use crate::widgets::status_bar::StatusBar;
pub use crate::widgets::status_bar::status_bar_height;
use crate::widgets::tool_panels::{ToolPanels, tool_panels_height};

/// the full TUI layout, composing all widgets
pub struct Ui<'a> {
    app: &'a App,
    /// when true, skip rendering the status bar (used for shared status bar in multi-pane)
    hide_status: bool,
}

impl<'a> Ui<'a> {
    pub fn new(app: &'a App) -> Self {
        Self {
            app,
            hide_status: false,
        }
    }

    /// suppress the per-pane status bar (for shared status bar in multi-pane)
    pub fn hide_status(mut self, hide: bool) -> Self {
        self.hide_status = hide;
        self
    }

    /// get the cursor position for the terminal
    pub fn cursor_position(&self, area: Rect) -> (u16, u16) {
        let input_h = input_height(&self.app.input, area.width, &self.app.pending_images);
        let tools_h = tool_panels_height(&self.app.active_tools, area.width);
        let status_h = if self.hide_status {
            0
        } else {
            status_bar_height(self.app, area.width)
        };
        let chunks = layout(area, input_h, tools_h, status_h);
        self.app.input_area.set(chunks.input);
        InputBox::new(self.app).cursor_position(chunks.input)
    }
}

/// named layout regions
pub struct LayoutRegions {
    pub messages: Rect,
    pub tools: Option<Rect>,
    pub input: Rect,
    pub status: Rect,
}

/// compute wrapped line count for input text, accounting for newlines and images
pub fn input_height(input: &str, area_width: u16, images: &[crate::app::PendingImage]) -> u16 {
    // inner width of the bordered block (left + right border = 2 columns)
    let content_width = area_width.saturating_sub(2) as usize;
    if content_width == 0 {
        return 3;
    }
    // expand image placeholders so wrapping accounts for token width
    let expanded = crate::widgets::input_box::expand_input(input, 0, images);
    // use the same word-wrap algorithm as the input box renderer
    let mut total_lines: usize = 0;
    for (i, line) in expanded.text.split('\n').enumerate() {
        let indent = if i == 0 { 2 } else { 0 }; // "> " prompt
        let segments = crate::widgets::input_box::word_wrap_segments(line, content_width, indent);
        total_lines += segments.len();
    }
    // +2 for borders, cap at 12 lines so it doesn't eat the whole screen
    (total_lines as u16 + 2).min(12)
}

/// compute the main layout
pub fn layout(area: Rect, input_h: u16, tools_h: u16, status_h: u16) -> LayoutRegions {
    if tools_h > 0 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),           // messages
                Constraint::Length(tools_h),  // tool panels
                Constraint::Length(input_h),  // input
                Constraint::Length(status_h), // status bar
            ])
            .split(area);

        LayoutRegions {
            messages: chunks[0],
            tools: Some(chunks[1]),
            input: chunks[2],
            status: chunks[3],
        }
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),           // messages
                Constraint::Length(input_h),  // input
                Constraint::Length(status_h), // status bar
            ])
            .split(area);

        LayoutRegions {
            messages: chunks[0],
            tools: None,
            input: chunks[1],
            status: chunks[2],
        }
    }
}

impl Widget for Ui<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let input_h = input_height(&self.app.input, area.width, &self.app.pending_images);
        let tools_h = tool_panels_height(&self.app.active_tools, area.width);
        let status_h = if self.hide_status {
            0
        } else {
            status_bar_height(self.app, area.width)
        };
        let regions = layout(area, input_h, tools_h, status_h);
        self.app.input_area.set(regions.input);
        MessageList::new(self.app).render(regions.messages, buf);
        if let Some(tools_area) = regions.tools {
            ToolPanels::new(&self.app.active_tools, &self.app.throbber_state)
                .render(tools_area, buf);
        }
        InputBox::new(self.app).render(regions.input, buf);
        if !self.hide_status {
            StatusBar::new(self.app).render(regions.status, buf);
        }

        // floating popups
        if self.app.mode == AppMode::Search {
            SearchPopup::new(self.app).render(area, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn input_height_short_text() {
        // "hi" in an 80-wide box: 1 line + 2 borders = 3
        assert_eq!(input_height("hi", 80, &[]), 3);
    }

    #[test]
    fn input_height_wrapping() {
        // 100 chars in a 20-wide box: content_width=18, effective=102, ceil(102/18)+1=6+2=8
        let text = "a".repeat(100);
        assert_eq!(input_height(&text, 20, &[]), 8);
    }

    #[test]
    fn input_height_capped() {
        // 1000 chars in a 20-wide box would be 65 lines, but capped at 12
        let text = "a".repeat(1000);
        assert_eq!(input_height(&text, 20, &[]), 12);
    }

    #[test]
    fn input_height_multiline() {
        // "hello\nworld" in a 40-wide box: 2 lines + 2 borders = 4
        assert_eq!(input_height("hello\nworld", 40, &[]), 4);
        // three newlines = 4 lines + 2 borders = 6
        assert_eq!(input_height("a\nb\nc\nd", 40, &[]), 6);
    }

    #[test]
    fn input_height_with_images() {
        use crate::app::{IMAGE_PLACEHOLDER, PendingImage};
        use mush_ai::types::ImageMimeType;
        let img = PendingImage {
            data: vec![],
            mime_type: ImageMimeType::Png,
            dimensions: Some((100, 200)),
        };
        // image token is inline, doesn't add a whole line
        let input = format!("hi{IMAGE_PLACEHOLDER}");
        // "hi[📷 100x200]" = ~16 chars, fits in 80-wide box = 1 line + 2 borders = 3
        assert_eq!(input_height(&input, 80, &[img]), 3);
    }

    #[test]
    fn ui_sets_input_area_for_mouse_routing() {
        let app = App::new("test".into(), 200_000);
        let ui = Ui::new(&app);
        let area = Rect::new(0, 0, 80, 24);
        let _ = ui.cursor_position(area);
        let input = app.input_area.get();
        assert!(input.height > 0);
        assert!(input.y > 0);
    }

    #[test]
    fn full_layout_renders() {
        let mut app = App::new("claude-sonnet-4".into(), 200_000);
        app.push_user_message("what is rust?");
        app.start_streaming();
        app.push_text_delta("rust is a systems programming language");
        app.finish_streaming(None, Some(0.003));

        let buf = render_full(&app, 60, 20);
        let content = buffer_to_string(&buf);

        assert!(content.contains("you"));
        assert!(content.contains("what is rust"));
        // model id shown as label instead of "mush"
        assert!(content.contains("claude-sonnet-4"));
        assert!(content.contains("systems programming"));
        assert!(content.contains("> "));
    }

    #[test]
    fn layout_regions_are_correct() {
        let area = Rect::new(0, 0, 80, 24);
        let regions = layout(area, 3, 0, 1);
        assert_eq!(regions.status.height, 1);
        assert_eq!(regions.input.height, 3);
        assert_eq!(regions.messages.height, 20); // 24 - 3 - 1
        assert!(regions.tools.is_none());
    }

    #[test]
    fn cursor_position_in_layout() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello".into();
        app.cursor = 5;
        let ui = Ui::new(&app);
        // use wide area so status bar fits on 1 line
        let area = Rect::new(0, 0, 200, 24);
        let (x, y) = ui.cursor_position(area);
        // input at y=20 (messages 0-19), cursor inside input: y=21 (border+1)
        assert_eq!(y, 21);
        // x: 0 + 1 (border) + 2 ("> ") + 5 (cursor) = 8
        assert_eq!(x, 8);
    }

    fn render_full(app: &App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(Ui::new(app), area);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_to_string(buf: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }
}
