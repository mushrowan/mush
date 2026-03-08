//! status bar widget - model info, cost, token usage

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use mush_ai::types::ThinkingLevel;

use crate::app::App;
use crate::path_utils::truncate_path;

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

/// get the confirm prompt text (only shown during tool confirmation)
fn confirm_text(app: &App) -> Option<String> {
    if app.mode != crate::app::AppMode::ToolConfirm {
        return None;
    }
    Some(if let Some(ref prompt) = app.confirm_prompt {
        format!("run {prompt}? (y/n)")
    } else {
        "confirm tool? (y/n)".into()
    })
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

    let mut spans = vec![Span::styled(" ", dim)];

    // pane indicator when multi-pane
    if let Some((pane_idx, pane_count)) = app.pane_info {
        spans.push(Span::styled(
            format!("[{pane_idx}/{pane_count}] "),
            Style::default().fg(Color::Cyan),
        ));
    }

    spans.extend([
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
    ]);

    if app.stats.input_tokens > 0 {
        spans.push(Span::styled(
            format!(" | ↑{}", format_tokens(app.stats.input_tokens)),
            dim,
        ));
    }
    if app.stats.output_tokens > 0 {
        spans.push(Span::styled(
            format!(" ↓{}", format_tokens(app.stats.output_tokens)),
            dim,
        ));
    }
    if app.stats.cache_read_tokens > 0 {
        spans.push(Span::styled(
            format!(" R{}", format_tokens(app.stats.cache_read_tokens)),
            dim,
        ));
    }
    if app.stats.cache_write_tokens > 0 {
        spans.push(Span::styled(
            format!(" W{}", format_tokens(app.stats.cache_write_tokens)),
            dim,
        ));
    }

    if app.stats.context_tokens > 0 {
        let ctx = format_tokens(app.stats.context_tokens);
        let window = format_tokens(app.stats.context_window);
        let pct = if app.stats.context_window > 0 {
            (app.stats.context_tokens as f64 / app.stats.context_window as f64 * 100.0) as u64
        } else {
            0
        };

        // colour by cache warmth, with context pressure overriding
        let cache_remaining = app.cache_remaining_secs();
        let ctx_style = if pct > 75 {
            // context pressure always takes priority
            Style::default().fg(Color::Red)
        } else if app.cache_ttl_secs == 0 {
            // caching disabled for this provider/retention
            dim
        } else {
            match cache_remaining {
                Some(r) if r > 60 => Style::default().fg(Color::Green),
                Some(r) if r > 0 => Style::default().fg(Color::Yellow),
                Some(0) => Style::default().fg(Color::DarkGray),
                _ => dim,
            }
        };

        // append cache countdown when active
        let cache_suffix = match cache_remaining {
            Some(r) if r > 0 => {
                let mins = r / 60;
                let secs = r % 60;
                format!(" {mins}:{secs:02}")
            }
            Some(0) => {
                // show "cold" briefly then fade out
                let elapsed = app
                    .cache_last_active
                    .map(|t| t.elapsed().as_secs())
                    .unwrap_or(0);
                if elapsed < (app.cache_ttl_secs as u64) + 30 {
                    " cold".into()
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        };

        spans.push(Span::styled(
            format!(" | {ctx}/{window}{cache_suffix}"),
            ctx_style,
        ));
    }

    if app.show_cost && app.stats.total_cost > 0.0 {
        spans.push(Span::styled(
            format!(" | ${:.4}", app.stats.total_cost),
            dim,
        ));
    }

    if let Some(ref status) = app.status
        && status != "config reloaded"
    {
        spans.push(Span::styled(format!(" | {status}"), dim));
    }

    // scroll position indicator (only when scrolled away from bottom)
    if app.scroll_offset > 0 {
        let total = app.total_content_lines.get();
        let visible = app.visible_area_height.get();
        let max_scroll = total.saturating_sub(visible);
        if max_scroll > 0 {
            // scroll_offset is lines from bottom, convert to percentage from top
            let from_top = max_scroll.saturating_sub(app.scroll_offset);
            let pct = (from_top as f64 / max_scroll as f64 * 100.0) as u16;
            spans.push(Span::styled(
                format!(" | {pct}%"),
                Style::default().fg(Color::Blue),
            ));
        }
    }

    // background pane alerts
    if let Some(ref alert) = app.background_alert {
        spans.push(Span::styled(
            format!(" | {alert}"),
            Style::default().fg(Color::Yellow),
        ));
    }

    spans
}

/// calculate how many lines the status bar needs (1 or 2)
pub fn status_bar_height(app: &App, width: u16) -> u16 {
    let left_len: usize = left_spans(app).iter().map(|s| s.content.len()).sum();
    let right = truncate_path(&app.cwd, 30);
    let total = left_len + 2 + right.len(); // 2 for padding between left and right
    if let Some(confirm) = confirm_text(app) {
        // confirm prompt gets its own line
        let _ = confirm;
        if total > width as usize { 3 } else { 2 }
    } else if total > width as usize {
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
        let right_text = truncate_path(&self.app.cwd, 30);
        let confirm = confirm_text(self.app);

        let left_len: usize = spans.iter().map(|s| s.content.len()).sum();

        if let Some(ref confirm_str) = confirm {
            // tool confirmation: show info + cwd on line 1, confirm on line 2
            let padding = (area.width as usize).saturating_sub(left_len + right_text.len() + 1);
            if padding > 0 {
                spans.push(Span::styled(" ".repeat(padding), dim));
                spans.push(Span::styled(
                    right_text,
                    Style::default().fg(Color::DarkGray),
                ));
            }
            let line1 = Line::from(spans);
            let line2 = Line::from(vec![
                Span::styled(" ", dim),
                Span::styled(confirm_str.clone(), Style::default().fg(Color::Yellow)),
            ]);
            Paragraph::new(vec![line1, line2]).render(area, buf);
            return;
        }

        // single line: right-align cwd
        let padding = (area.width as usize).saturating_sub(left_len + right_text.len() + 1);
        if padding > 0 {
            spans.push(Span::styled(" ".repeat(padding), dim));
            spans.push(Span::styled(
                right_text,
                Style::default().fg(Color::DarkGray),
            ));
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
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("claude-sonnet-4"));
    }

    #[test]
    fn status_bar_shows_cost_and_context() {
        let mut app = App::new("test-model".into(), 200_000);
        app.stats.total_cost = 0.0123;
        app.show_cost = true;
        app.stats.input_tokens = 45_000;
        app.stats.output_tokens = 12_000;
        app.stats.cache_read_tokens = 8_000;
        app.stats.cache_write_tokens = 2_000;
        app.stats.context_tokens = 45_000;
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
    fn status_bar_hides_config_reloaded_status() {
        let mut app = App::new("test".into(), 200_000);
        app.status = Some("config reloaded".into());
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(!content.contains("config reloaded"));
    }

    #[test]
    fn status_bar_shows_cwd_right() {
        let mut app = App::new("test".into(), 200_000);
        app.cwd = "/home/user/project".into();
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("/home/user/project"));
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
    fn status_bar_single_line_normally() {
        let app = App::new("test".into(), 200_000);
        // without hotkey hints, even narrow terminals should fit in 1 line
        assert_eq!(status_bar_height(&app, 80), 1);
        assert_eq!(status_bar_height(&app, 200), 1);
    }

    #[test]
    fn truncate_path_short_unchanged() {
        assert_eq!(truncate_path("~/dev/mush", 30), "~/dev/mush");
    }

    #[test]
    fn status_bar_shows_cache_countdown() {
        let mut app = App::new("test".into(), 200_000);
        app.cache_ttl_secs = 300;
        app.stats.context_tokens = 10_000;
        app.refresh_cache_timer();
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        // should show "10k/200k 4:59" or similar
        assert!(content.contains("10k/200k"));
        assert!(content.contains(":"));
    }

    #[test]
    fn truncate_path_long_keeps_tail() {
        let long = "~/dev/some/deep/nested/project";
        let result = truncate_path(long, 20);
        assert!(result.starts_with('…'));
        assert!(result.ends_with("project"));
        assert!(result.len() <= 20);
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
