//! status bar widget - model info, cost, token usage

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use mush_ai::types::ThinkingLevel;

use crate::app::App;

/// format token count as human-readable (e.g. 45k, 200k, 1.2m)
fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        format!("{tokens}")
    }
}

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
            Span::styled(self.app.model_id.as_ref(), Style::default().fg(Color::Cyan)),
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

        if self.app.total_input_tokens > 0 {
            spans.push(Span::styled(
                format!(" | ↑{}", format_tokens(self.app.total_input_tokens)),
                dim,
            ));
        }
        if self.app.total_output_tokens > 0 {
            spans.push(Span::styled(
                format!(" ↓{}", format_tokens(self.app.total_output_tokens)),
                dim,
            ));
        }
        if self.app.total_cache_read_tokens > 0 {
            spans.push(Span::styled(
                format!(" R{}", format_tokens(self.app.total_cache_read_tokens)),
                dim,
            ));
        }
        if self.app.total_cache_write_tokens > 0 {
            spans.push(Span::styled(
                format!(" W{}", format_tokens(self.app.total_cache_write_tokens)),
                dim,
            ));
        }

        if self.app.context_tokens > 0 {
            let ctx = format_tokens(self.app.context_tokens);
            let window = format_tokens(self.app.context_window);
            let pct = if self.app.context_window > 0 {
                (self.app.context_tokens as f64 / self.app.context_window as f64 * 100.0) as u64
            } else {
                0
            };
            let ctx_style = if pct > 75 {
                Style::default().fg(Color::Red)
            } else if pct > 50 {
                Style::default().fg(Color::Yellow)
            } else {
                dim
            };
            spans.push(Span::styled(format!(" | {ctx}/{window}"), ctx_style));
        }

        if self.app.show_cost && self.app.total_cost > 0.0 {
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
        let confirm_hint;
        let hint = if self.app.mode == crate::app::AppMode::Scroll {
            "j/k scroll | g/G top/bottom | y copy | esc exit"
        } else if self.app.mode == crate::app::AppMode::ToolConfirm {
            confirm_hint = if let Some(ref prompt) = self.app.confirm_prompt {
                format!("run {prompt}? (y/n)")
            } else {
                "confirm tool? (y/n)".to_string()
            };
            confirm_hint.as_str()
        } else if self.app.is_streaming {
            "esc abort | ctrl+c quit"
        } else {
            "enter send | ctrl+s scroll | ctrl+t thinking | ctrl+i injection | ctrl+c quit"
        };
        let hint_style = if self.app.mode == crate::app::AppMode::ToolConfirm {
            Style::default().fg(Color::Yellow)
        } else {
            dim
        };
        let padding = (area.width as usize).saturating_sub(left_len + hint.len() + 1);
        if padding > 0 {
            spans.push(Span::styled(" ".repeat(padding), dim));
            spans.push(Span::styled(hint, hint_style));
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
        let app = App::new("claude-sonnet-4".into(), 200_000);
        let buf = render_status(&app, 80, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("claude-sonnet-4"));
    }

    #[test]
    fn status_bar_shows_cost_and_context() {
        let mut app = App::new("test-model".into(), 200_000);
        app.total_cost = 0.0123;
        app.show_cost = true;
        app.total_input_tokens = 45_000;
        app.total_output_tokens = 12_000;
        app.total_cache_read_tokens = 8_000;
        app.total_cache_write_tokens = 2_000;
        app.context_tokens = 45_000;
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("↑45k"));
        assert!(content.contains("↓12k"));
        assert!(content.contains("R8k"));
        assert!(content.contains("W2k"));
        assert!(content.contains("45k/200k"));
        assert!(content.contains("$0.0123"));
    }

    #[test]
    fn status_bar_shows_hint() {
        let app = App::new("test".into(), 200_000);
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("ctrl+i injection"));
        assert!(content.contains("ctrl+c"));
    }

    #[test]
    fn status_bar_shows_thinking_level() {
        let mut app = App::new("test".into(), 200_000);
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
