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
        // inner width of the block (left + right border = 2 columns)
        // the "> " prompt is part of text content, not the wrap boundary
        let content_width = area.width.saturating_sub(2) as usize;
        if content_width == 0 {
            return (area.x + 1, area.y + 1);
        }

        // walk through the input up to the cursor, tracking line/col
        // the first line has a "> " prefix (2 chars)
        let text_before_cursor = &self.app.input[..self.app.cursor];
        let mut line: usize = 0;
        let mut col: usize = 2; // start after "> " prompt

        for ch in text_before_cursor.chars() {
            if ch == '\n' {
                line += 1;
                col = 0; // no prompt prefix on continuation lines
            } else {
                col += ch.len_utf8();
                if col >= content_width {
                    line += 1;
                    col -= content_width;
                }
            }
        }

        let x = area.x + 1 + col as u16;
        let y = area.y + 1 + line as u16;
        (
            x.min(area.x + area.width - 2),
            y.min(area.y + area.height - 2),
        )
    }
}

impl Widget for InputBox<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let streaming_idle = self.app.is_streaming && self.app.input.is_empty();
        let prompt = if streaming_idle { "..." } else { "> " };
        let style = if streaming_idle {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };

        let input_lines: Vec<&str> = self.app.input.split('\n').collect();
        let mut lines: Vec<Line<'_>> = Vec::new();
        for (i, line_text) in input_lines.iter().enumerate() {
            let mut spans = Vec::new();
            if i == 0 {
                spans.push(Span::styled(prompt, Style::default().fg(Color::Cyan)));
            }
            spans.push(Span::styled(*line_text, style));
            // ghost completion on the last line only
            if i == input_lines.len() - 1
                && let Some(ghost) = self.app.ghost_text()
            {
                spans.push(Span::styled(ghost, Style::default().fg(Color::DarkGray)));
            }
            lines.push(Line::from(spans));
        }
        let text = ratatui::text::Text::from(lines);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if streaming_idle {
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
    fn input_box_streaming_shows_dots_when_empty() {
        let mut app = App::new("test".into(), 200_000);
        app.is_streaming = true;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("..."));
    }

    #[test]
    fn input_box_streaming_shows_prompt_when_typing() {
        let mut app = App::new("test".into(), 200_000);
        app.is_streaming = true;
        app.input = "hold on".into();
        app.cursor = 7;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("> hold on"));
        assert!(!content.contains("..."));
    }

    #[test]
    fn cursor_position_calculation() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello".into();
        app.cursor = 3;
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 10, 40, 3);
        let (x, y) = input_box.cursor_position(area);
        // content_width = 40 - 2 = 38 (borders only)
        // col = 2 (prompt) + 3 = 5, no wrapping
        // x: 0 + 1 + 5 = 6
        assert_eq!(x, 6);
        assert_eq!(y, 11);
    }

    #[test]
    fn cursor_position_wraps() {
        let mut app = App::new("test".into(), 200_000);
        // 42 chars + 2 prompt in a 20-wide box: content_width = 18
        app.input = "this is a long prompt that wraps around!!!".into();
        app.cursor = 42;
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 0, 20, 6);
        let (x, y) = input_box.cursor_position(area);
        // content_width = 20 - 2 = 18
        // col=2, +16 chars -> col=18 -> wrap: line=1, col=0
        // +18 chars -> col=18 -> wrap: line=2, col=0
        // +8 remaining chars -> col=8
        assert_eq!(x, 1 + 8); // border + col
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

    #[test]
    fn cursor_position_multiline() {
        let mut app = App::new("test".into(), 200_000);
        // "hello\nworld" with cursor at end (byte 11)
        app.input = "hello\nworld".into();
        app.cursor = 11; // after 'd'
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 0, 40, 5);
        let (x, y) = input_box.cursor_position(area);
        // line 0: "> hello" (7 chars), line 1: "world" (5 chars)
        // cursor is at col 5 on line 1
        assert_eq!(x, 1 + 5); // border + col
        assert_eq!(y, 1 + 1); // border + line 1
    }

    #[test]
    fn cursor_position_after_newline() {
        let mut app = App::new("test".into(), 200_000);
        // cursor right after the newline
        app.input = "hello\n".into();
        app.cursor = 6;
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 0, 40, 5);
        let (x, y) = input_box.cursor_position(area);
        // line 1, col 0
        assert_eq!(x, 1); // border + col 0
        assert_eq!(y, 2); // border + line 1
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
