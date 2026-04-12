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
        let input_h = input_height(&self.app.input, area.width);
        let tools_h = tool_panels_height(&self.app.active_tools, area.width);
        let status_h = if self.hide_status {
            0
        } else {
            status_bar_height(self.app, area.width)
        };
        let chunks = layout(area, input_h, tools_h, status_h);
        self.app.input.area.set(chunks.input);
        // sync visible_lines so scroll gets clamped correctly when the
        // input box changes height (e.g. after inserting a newline)
        self.app
            .input
            .visible_lines
            .set(chunks.input.height.saturating_sub(2));
        self.app.input.ensure_cursor_visible();
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
pub fn input_height(input: &crate::app::InputBuffer, area_width: u16) -> u16 {
    let content_width = area_width.saturating_sub(2) as usize;
    if content_width == 0 {
        return 3;
    }

    let layout = input.layout(content_width);
    (layout.total_lines + 2).min(12)
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
        let input_h = input_height(&self.app.input, area.width);
        let tools_h = tool_panels_height(&self.app.active_tools, area.width);
        let status_h = if self.hide_status {
            0
        } else {
            status_bar_height(self.app, area.width)
        };
        let regions = layout(area, input_h, tools_h, status_h);
        self.app.input.area.set(regions.input);
        MessageList::new(self.app).render(regions.messages, buf);
        if let Some(tools_area) = regions.tools {
            ToolPanels::new(
                &self.app.active_tools,
                &self.app.throbber_state,
                &self.app.theme,
            )
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
    use mush_ai::types::{Dollars, TokenCount};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn input_height_short_text() {
        // "hi" in an 80-wide box: 1 line + 2 borders = 3
        assert_eq!(input_height_for("hi", 80, vec![]), 3);
    }

    #[test]
    fn input_height_wrapping() {
        // 100 chars in a 20-wide box: content_width=18, effective=102, ceil(102/18)+1=6+2=8
        let text = "a".repeat(100);
        assert_eq!(input_height_for(&text, 20, vec![]), 8);
    }

    #[test]
    fn input_height_capped() {
        // 1000 chars in a 20-wide box would be 65 lines, but capped at 12
        let text = "a".repeat(1000);
        assert_eq!(input_height_for(&text, 20, vec![]), 12);
    }

    #[test]
    fn input_height_multiline() {
        // "hello\nworld" in a 40-wide box: 2 lines + 2 borders = 4
        assert_eq!(input_height_for("hello\nworld", 40, vec![]), 4);
        // three newlines = 4 lines + 2 borders = 6
        assert_eq!(input_height_for("a\nb\nc\nd", 40, vec![]), 6);
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
        assert_eq!(input_height_for(&input, 80, vec![img]), 3);
    }

    #[test]
    fn ui_sets_input_area_for_mouse_routing() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let ui = Ui::new(&app);
        let area = Rect::new(0, 0, 80, 24);
        let _ = ui.cursor_position(area);
        let input = app.input.area.get();
        assert!(input.height > 0);
        assert!(input.y > 0);
    }

    #[test]
    fn full_layout_renders() {
        let mut app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
        app.push_user_message("what is rust?");
        app.start_streaming();
        app.push_text_delta("rust is a systems programming language");
        app.finish_streaming(None, Some(Dollars::new(0.003)));

        let buf = render_full(&app, 60, 20);
        let content = buffer_to_string(&buf);

        assert!(content.contains("what is rust"));
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
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "hello".into();
        app.input.cursor = 5;
        let ui = Ui::new(&app);
        // use wide area so status bar fits on 1 line
        let area = Rect::new(0, 0, 200, 24);
        let (x, y) = ui.cursor_position(area);
        // input at y=20 (messages 0-19), cursor inside input: y=21 (border+1)
        assert_eq!(y, 21);
        // x: 0 + 1 (border) + 2 ("> ") + 5 (cursor) = 8
        assert_eq!(x, 8);
    }

    #[test]
    fn input_layout_cache_reuses_layout_across_cursor_and_render() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "one two three four five six seven eight nine ten".into();
        app.input.cursor = app.input.text.len();
        app.input.reset_layout_builds();

        let area = Rect::new(0, 0, 20, 8);
        let _ = Ui::new(&app).cursor_position(area);
        let _buf = render_full(&app, 20, 8);

        assert_eq!(app.input.layout_builds(), 1);
    }

    #[test]
    fn input_layout_cache_refreshes_after_text_change() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "hello".into();
        app.input.cursor = app.input.text.len();
        app.input.reset_layout_builds();

        assert_eq!(input_height(&app.input, 20), 3);
        assert_eq!(app.input.layout_builds(), 1);

        app.input.text.push('!');
        app.input.cursor = app.input.text.len();

        assert_eq!(input_height(&app.input, 20), 3);
        assert_eq!(app.input.layout_builds(), 2);
    }

    fn input_height_for(text: &str, width: u16, images: Vec<crate::app::PendingImage>) -> u16 {
        let mut input = crate::app::InputBuffer::new();
        input.text = text.to_string();
        input.cursor = input.text.len();
        input.images = images;
        input_height(&input, width)
    }

    #[test]
    fn cursor_position_correct_after_newline_grows_input() {
        // simulate the shift+enter scenario:
        // 1. render with "hello" (1-line input, area height 3)
        // 2. insert_char('\n') which calls ensure_cursor_visible with old area
        // 3. compute cursor_position with new layout (2-line input, area height 4)
        // the cursor should be on line 1, not line 0
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "hello".into();
        app.input.cursor = 5;

        // step 1: initial render sets area and visible_lines
        let area = Rect::new(0, 0, 40, 24);
        let _ = Ui::new(&app).cursor_position(area);
        let _ = render_full(&app, 40, 24);

        // step 2: insert newline (triggers ensure_cursor_visible with old area)
        app.input.insert_char('\n');

        // step 3: compute cursor position for the next frame
        let (_, cy) = Ui::new(&app).cursor_position(area);

        // cursor should be on line 1 of the input box (below "hello")
        // input area starts near the bottom, cursor should be at area.y + 1 + 1
        let input_area = app.input.area.get();
        let expected_y = input_area.y + 1 + 1; // border + line 1
        assert_eq!(
            cy, expected_y,
            "cursor should be on the new line, not the previous one"
        );
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
