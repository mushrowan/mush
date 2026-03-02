//! status bar widget - model info, cost, token usage

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use mush_ai::types::ThinkingLevel;

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

        let thinking_label = match self.app.thinking_level {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
        };

        let mut spans = vec![
            Span::styled(" ", dim),
            Span::styled(&self.app.model_id, Style::default().fg(Color::Cyan)),
            Span::styled(" • ", dim),
            Span::styled(
                format!("thinking {thinking_label}"),
                if self.app.thinking_level == ThinkingLevel::Off {
                    dim
                } else {
                    Style::default().fg(Color::Cyan)
                },
            ),
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

        if self.app.has_unread {
            spans.push(Span::styled(
                " | ↓ new messages (esc)",
                Style::default().fg(Color::Blue),
            ));
        }

        // right-align hint
        let left_len: usize = spans.iter().map(|s| s.content.len()).sum();
        let hint = if self.app.is_streaming {
            "esc abort | ctrl+c quit"
        } else {
            "enter send | ctrl+t thinking | ctrl+o expand | ctrl+c quit"
        };
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
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("ctrl+c"));
    }

    #[test]
    fn status_bar_shows_thinking_level() {
        let mut app = App::new("test".into());
        app.thinking_level = ThinkingLevel::High;
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("thinking high"));
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
