//! input box widget - text entry with cursor

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

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
        // +1 for border, +2 for "> " prefix
        let x = area.x + 1 + 2 + self.app.cursor as u16;
        let y = area.y + 1;
        (x.min(area.x + area.width - 2), y)
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

        let text = Line::from(vec![
            Span::styled(prompt, Style::default().fg(Color::Cyan)),
            Span::styled(self.app.input.as_str(), style),
        ]);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if self.app.is_streaming {
                Color::DarkGray
            } else {
                Color::Cyan
            }));

        Paragraph::new(text).block(block).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn input_box_renders_empty() {
        let app = App::new("test".into());
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("> "));
    }

    #[test]
    fn input_box_renders_text() {
        let mut app = App::new("test".into());
        app.input = "hello world".into();
        app.cursor = 11;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("hello world"));
    }

    #[test]
    fn input_box_streaming_shows_dots() {
        let mut app = App::new("test".into());
        app.is_streaming = true;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("..."));
    }

    #[test]
    fn cursor_position_calculation() {
        let mut app = App::new("test".into());
        app.input = "hello".into();
        app.cursor = 3;
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 10, 40, 3);
        let (x, y) = input_box.cursor_position(area);
        // x: 0 (area.x) + 1 (border) + 2 ("> ") + 3 (cursor) = 6
        assert_eq!(x, 6);
        assert_eq!(y, 11);
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
