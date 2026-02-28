//! status bar widget - model info, cost, token usage

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::app::App;

/// renders the status bar
pub struct StatusBar<'a> {
    app: &'a App,
}

impl<'a> StatusBar<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);

        let mut spans = vec![
            Span::styled(" ", dim),
            Span::styled(&self.app.model_id, Style::default().fg(Color::Cyan)),
        ];

        if self.app.total_tokens > 0 {
            spans.push(Span::styled(
                format!(" | {}tok", self.app.total_tokens),
                dim,
            ));
        }

        if self.app.total_cost > 0.0 {
            spans.push(Span::styled(format!(" | ${:.4}", self.app.total_cost), dim));
        }

        if let Some(ref status) = self.app.status {
            spans.push(Span::styled(format!(" | {status}"), dim));
        }

        // right-align hint
        let left_len: usize = spans.iter().map(|s| s.content.len()).sum();
        let hint = "ctrl+c quit | esc abort";
        let padding = (area.width as usize).saturating_sub(left_len + hint.len() + 1);
        if padding > 0 {
            spans.push(Span::styled(" ".repeat(padding), dim));
            spans.push(Span::styled(hint, dim));
        }

        Paragraph::new(Line::from(spans)).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn status_bar_shows_model() {
        let app = App::new("claude-sonnet-4".into());
        let buf = render_status(&app, 80, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("claude-sonnet-4"));
    }

    #[test]
    fn status_bar_shows_cost_and_tokens() {
        let mut app = App::new("test-model".into());
        app.total_cost = 0.0123;
        app.total_tokens = 5000;
        let buf = render_status(&app, 80, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("5000tok"));
        assert!(content.contains("$0.0123"));
    }

    #[test]
    fn status_bar_shows_hint() {
        let app = App::new("test".into());
        let buf = render_status(&app, 80, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("ctrl+c"));
    }

    fn render_status(app: &App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(StatusBar::new(app), area);
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
