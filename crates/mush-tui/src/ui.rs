//! main layout and rendering

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::Widget;

use crate::app::App;
use crate::widgets::input_box::InputBox;
use crate::widgets::message_list::MessageList;
use crate::widgets::status_bar::StatusBar;

/// the full TUI layout, composing all widgets
pub struct Ui<'a> {
    app: &'a App,
}

impl<'a> Ui<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// get the cursor position for the terminal
    pub fn cursor_position(&self, area: Rect) -> (u16, u16) {
        let chunks = layout(area);
        InputBox::new(self.app).cursor_position(chunks.input)
    }
}

/// named layout regions
pub struct LayoutRegions {
    pub messages: Rect,
    pub input: Rect,
    pub status: Rect,
}

/// compute the main layout
pub fn layout(area: Rect) -> LayoutRegions {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // messages
            Constraint::Length(3), // input
            Constraint::Length(1), // status bar
        ])
        .split(area);

    LayoutRegions {
        messages: chunks[0],
        input: chunks[1],
        status: chunks[2],
    }
}

impl Widget for Ui<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let regions = layout(area);
        MessageList::new(self.app).render(regions.messages, buf);
        InputBox::new(self.app).render(regions.input, buf);
        StatusBar::new(self.app).render(regions.status, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn full_layout_renders() {
        let mut app = App::new("claude-sonnet-4".into());
        app.push_user_message("what is rust?".into());
        app.start_streaming();
        app.push_text_delta("rust is a systems programming language");
        app.finish_streaming(None, Some(0.003));

        let buf = render_full(&app, 60, 20);
        let content = buffer_to_string(&buf);

        assert!(content.contains("you"));
        assert!(content.contains("what is rust"));
        assert!(content.contains("mush"));
        assert!(content.contains("systems programming"));
        assert!(content.contains("claude-sonnet-4"));
        assert!(content.contains("> "));
    }

    #[test]
    fn layout_regions_are_correct() {
        let area = Rect::new(0, 0, 80, 24);
        let regions = layout(area);
        assert_eq!(regions.status.height, 1);
        assert_eq!(regions.input.height, 3);
        assert_eq!(regions.messages.height, 20); // 24 - 3 - 1
    }

    #[test]
    fn cursor_position_in_layout() {
        let mut app = App::new("test".into());
        app.input = "hello".into();
        app.cursor = 5;
        let ui = Ui::new(&app);
        let area = Rect::new(0, 0, 80, 24);
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
