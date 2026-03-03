//! input box widget - text entry with cursor

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use crate::app::App;

/// renders the input box at the bottom of the screen
pub struct InputBox<'a> {
    app: &'a App,
}

impl<'a> InputBox<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// the cursor position within the input box (for terminal cursor placement)
    pub fn cursor_position(&self, area: Rect) -> (u16, u16) {
        // available content width: area - 2 (borders) - 2 ("> " prefix)
        let content_width = area.width.saturating_sub(4) as usize;
        if content_width == 0 {
            return (area.x + 1, area.y + 1);
        }
        // cursor offset includes the prompt prefix
        let cursor_offset = self.app.cursor + 2;
        let cursor_line = cursor_offset / content_width;
        let cursor_col = cursor_offset % content_width;
        let x = area.x + 1 + cursor_col as u16;
        let y = area.y + 1 + cursor_line as u16;
        (
            x.min(area.x + area.width - 2),
            y.min(area.y + area.height - 2),
        )
    }
}

impl Widget for InputBox<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let prompt = if self.app.is_streaming { "..." } else { "> " };
        let style = if self.app.is_streaming {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };

        let mut spans = vec![
            Span::styled(prompt, Style::default().fg(Color::Cyan)),
            Span::styled(self.app.input.as_str(), style),
        ];
        // ghost completion hint
        if let Some(ghost) = self.app.ghost_text() {
            spans.push(Span::styled(ghost, Style::default().fg(Color::DarkGray)));
        }
        let text = Line::from(spans);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if self.app.is_streaming {
                Color::DarkGray
            } else {
                Color::Cyan
            }));

        Paragraph::new(text)
            .block(block)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn input_box_renders_empty() {
        let app = App::new("test".into(), 200_000);
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("> "));
    }

    #[test]
    fn input_box_renders_text() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello world".into();
        app.cursor = 11;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("hello world"));
    }

    #[test]
    fn input_box_streaming_shows_dots() {
        let mut app = App::new("test".into(), 200_000);
        app.is_streaming = true;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("..."));
    }

    #[test]
    fn cursor_position_calculation() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello".into();
        app.cursor = 3;
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 10, 40, 3);
        let (x, y) = input_box.cursor_position(area);
        // content_width = 40 - 4 = 36
        // cursor_offset = 3 + 2 = 5
        // cursor_line = 5 / 36 = 0, cursor_col = 5 % 36 = 5
        // x: 0 + 1 + 5 = 6
        assert_eq!(x, 6);
        assert_eq!(y, 11);
    }

    #[test]
    fn cursor_position_wraps() {
        let mut app = App::new("test".into(), 200_000);
        // 44 chars in a 20-wide box: content_width = 16, wraps to 3 lines
        app.input = "this is a long prompt that wraps around!!!".into();
        app.cursor = 42;
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 0, 20, 6);
        let (x, y) = input_box.cursor_position(area);
        // cursor_offset = 42 + 2 = 44
        // content_width = 20 - 4 = 16
        // cursor_line = 44 / 16 = 2, cursor_col = 44 % 16 = 12
        assert_eq!(x, 1 + 12); // border + col
        assert_eq!(y, 1 + 2); // border + line
    }

    #[test]
    fn input_box_shows_ghost_completion() {
        let mut app = App::new("test".into(), 200_000);
        app.completions = vec!["/help".into(), "/history".into()];
        app.input = "/h".into();
        app.cursor = 2;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        // should show "/h" + ghost "elp"
        assert!(content.contains("/help"));
    }

    fn render_input(app: &App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(InputBox::new(app), area);
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
