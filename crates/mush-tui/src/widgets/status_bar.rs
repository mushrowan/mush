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

/// get the hint text for the current app mode
fn hint_text(app: &App) -> String {
    if app.mode == crate::app::AppMode::Scroll {
        "j/k scroll | g/G top/bottom | y copy | esc exit".into()
    } else if app.mode == crate::app::AppMode::ToolConfirm {
        if let Some(ref prompt) = app.confirm_prompt {
            format!("run {prompt}? (y/n)")
        } else {
            "confirm tool? (y/n)".into()
        }
    } else if app.is_streaming {
        "esc abort | ctrl+c quit".into()
    } else {
        "enter send | ctrl+v image | ctrl+s scroll | ctrl+t thinking | ctrl+c quit".into()
    }
}

/// build the left-side info spans
fn left_spans(app: &App) -> Vec<Span<'static>> {
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);

    let thinking_label = match app.thinking_level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
    };

    let mut spans = vec![
        Span::styled(" ", dim),
        Span::styled(app.model_id.to_string(), Style::default().fg(Color::Cyan)),
        Span::styled(" • ", dim),
        Span::styled(
            format!("thinking {thinking_label}"),
            if app.thinking_level == ThinkingLevel::Off {
                dim
            } else {
                Style::default().fg(Color::Cyan)
            },
        ),
    ];

    if app.total_input_tokens > 0 {
        spans.push(Span::styled(
            format!(" | ↑{}", format_tokens(app.total_input_tokens)),
            dim,
        ));
    }
    if app.total_output_tokens > 0 {
        spans.push(Span::styled(
            format!(" ↓{}", format_tokens(app.total_output_tokens)),
            dim,
        ));
    }
    if app.total_cache_read_tokens > 0 {
        spans.push(Span::styled(
            format!(" R{}", format_tokens(app.total_cache_read_tokens)),
            dim,
        ));
    }
    if app.total_cache_write_tokens > 0 {
        spans.push(Span::styled(
            format!(" W{}", format_tokens(app.total_cache_write_tokens)),
            dim,
        ));
    }

    if app.context_tokens > 0 {
        let ctx = format_tokens(app.context_tokens);
        let window = format_tokens(app.context_window);
        let pct = if app.context_window > 0 {
            (app.context_tokens as f64 / app.context_window as f64 * 100.0) as u64
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

    if app.show_cost && app.total_cost > 0.0 {
        spans.push(Span::styled(format!(" | ${:.4}", app.total_cost), dim));
    }

    if let Some(ref status) = app.status {
        spans.push(Span::styled(format!(" | {status}"), dim));
    }

    if app.has_unread {
        spans.push(Span::styled(
            " | ↓ new messages (esc)",
            Style::default().fg(Color::Blue),
        ));
    }

    spans
}

/// calculate how many lines the status bar needs (1 or 2)
pub fn status_bar_height(app: &App, width: u16) -> u16 {
    let left_len: usize = left_spans(app).iter().map(|s| s.content.len()).sum();
    let hint = hint_text(app);
    let total = left_len + 1 + hint.len();
    if total > width as usize {
        2
    } else {
        1
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

        let mut spans = left_spans(self.app);
        let hint = hint_text(self.app);
        let hint_style = if self.app.mode == crate::app::AppMode::ToolConfirm {
            Style::default().fg(Color::Yellow)
        } else {
            dim
        };

        let left_len: usize = spans.iter().map(|s| s.content.len()).sum();

        if area.height >= 2 {
            // 2-line mode: info on line 1, hints on line 2
            let total = left_len + 1 + hint.len();
            if total > area.width as usize {
                let line1 = Line::from(std::mem::take(&mut spans));
                let mut hint_spans = vec![Span::styled(" ", dim)];
                hint_spans.push(Span::styled(hint, hint_style));
                let line2 = Line::from(hint_spans);
                Paragraph::new(vec![line1, line2]).render(area, buf);
                return;
            }
        }

        // single line: right-align hint if it fits
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
        assert!(content.contains("ctrl+v image"));
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

    #[test]
    fn status_bar_wraps_to_two_lines_when_narrow() {
        let app = App::new("test".into(), 200_000);
        // at 80 cols, left (22) + hint (73) = 96 which exceeds 80
        // so it wraps: line 1 = info, line 2 = hints (73 fits within 80)
        let buf = render_status(&app, 80, 2);
        let content = buffer_to_string(&buf);
        assert!(content.contains("test"));
        assert!(content.contains("ctrl+c"));
    }

    #[test]
    fn status_bar_height_narrow() {
        let app = App::new("claude-sonnet-4".into(), 200_000);
        // narrow terminal should need 2 lines
        assert_eq!(status_bar_height(&app, 40), 2);
        // wide terminal should fit in 1
        assert_eq!(status_bar_height(&app, 200), 1);
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
