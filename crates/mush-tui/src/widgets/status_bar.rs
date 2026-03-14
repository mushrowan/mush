//! status bar widget - model info, cost, token usage

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use mush_ai::types::{Dollars, ThinkingLevel, TokenCount};

use crate::app::App;
use crate::path_utils::truncate_path;

/// format token count as human-readable (e.g. 45k, 200k, 1.2m)
fn format_tokens(tokens: mush_ai::types::TokenCount) -> String {
    let n = tokens.get();
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        format!("{n}")
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

    let thinking_label = match app.thinking_level.normalize_visible() {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Minimal => unreachable!(),
    };

    let mut spans = vec![Span::styled(" ", dim)];

    // pane indicator when multi-pane
    if let Some((pane_idx, pane_count)) = app.pane_info {
        spans.push(Span::styled(
            format!("[{pane_idx}/{pane_count}] "),
            Style::default().fg(Color::Cyan),
        ));
    }

    let sep = Span::styled(" │ ", dim);

    spans.extend([
        Span::styled(app.model_id.to_string(), Style::default().fg(Color::Cyan)),
        sep.clone(),
        Span::styled(
            format!("thinking: {thinking_label}"),
            if app.thinking_level == ThinkingLevel::Off {
                dim
            } else {
                Style::default().fg(Color::Cyan)
            },
        ),
    ]);

    if app.stats.input_tokens > TokenCount::ZERO {
        spans.push(Span::styled(
            format!(" ↑{}", format_tokens(app.stats.input_tokens)),
            dim,
        ));
    }
    if app.stats.output_tokens > TokenCount::ZERO {
        spans.push(Span::styled(
            format!(" ↓{}", format_tokens(app.stats.output_tokens)),
            dim,
        ));
    }
    if app.stats.cache_read_tokens > TokenCount::ZERO {
        spans.push(Span::styled(
            format!(" R{}", format_tokens(app.stats.cache_read_tokens)),
            dim,
        ));
    }
    if app.stats.cache_write_tokens > TokenCount::ZERO {
        spans.push(Span::styled(
            format!(" W{}", format_tokens(app.stats.cache_write_tokens)),
            dim,
        ));
    }

    if app.stats.context_tokens > TokenCount::ZERO {
        let ctx = format_tokens(app.stats.context_tokens);
        let window = format_tokens(app.stats.context_window);
        let pct = app
            .stats
            .context_tokens
            .percent_of(app.stats.context_window) as u64;

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

        spans.push(sep.clone());
        spans.push(Span::styled(
            format!("{ctx}/{window}{cache_suffix}"),
            ctx_style,
        ));
    }

    if app.show_cost && app.stats.total_cost > Dollars::ZERO {
        spans.push(Span::styled(format!(" {}", app.stats.total_cost), dim));
    }

    // oauth usage bars
    if let Some(ref usage) = app.oauth_usage {
        if let Some(ref w) = usage.five_hour {
            let pace = w.pace(mush_ai::oauth::usage::OAuthUsage::FIVE_HOUR);
            spans.push(sep.clone());
            spans.push(Span::styled("5h ", dim));
            spans.extend(super::usage_bar::render_usage_bar(w.utilization, pace));
            spans.push(Span::styled(format!(" {}%", w.utilization as u32), dim));
        }
        if let Some(ref w) = usage.seven_day {
            let pace = w.pace(mush_ai::oauth::usage::OAuthUsage::SEVEN_DAY);
            spans.push(sep.clone());
            spans.push(Span::styled("7d ", dim));
            spans.extend(super::usage_bar::render_usage_bar(w.utilization, pace));
            spans.push(Span::styled(format!(" {}%", w.utilization as u32), dim));
        }
    }

    if let Some(ref status) = app.status
        && status != "config reloaded"
    {
        spans.push(sep.clone());
        spans.push(Span::styled(status.clone(), dim));
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
                format!(" {pct}%"),
                Style::default().fg(Color::Blue),
            ));
        }
    }

    // background pane alerts
    if let Some(ref alert) = app.background_alert {
        spans.push(sep.clone());
        spans.push(Span::styled(
            alert.clone(),
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
        let app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("claude-sonnet-4"));
    }

    #[test]
    fn status_bar_shows_cost_and_context() {
        let mut app = App::new("test-model".into(), TokenCount::new(200_000));
        app.stats.total_cost = Dollars::new(0.0123);
        app.show_cost = true;
        app.stats.input_tokens = TokenCount::new(45_000);
        app.stats.output_tokens = TokenCount::new(12_000);
        app.stats.cache_read_tokens = TokenCount::new(8_000);
        app.stats.cache_write_tokens = TokenCount::new(2_000);
        app.stats.context_tokens = TokenCount::new(45_000);
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
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.status = Some("config reloaded".into());
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(!content.contains("config reloaded"));
    }

    #[test]
    fn status_bar_shows_cwd_right() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.cwd = "/home/user/project".into();
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("/home/user/project"));
    }

    #[test]
    fn status_bar_shows_thinking_level() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.thinking_level = ThinkingLevel::High;
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("thinking: high"));
    }

    #[test]
    fn status_bar_single_line_normally() {
        let app = App::new("test".into(), TokenCount::new(200_000));
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
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.cache_ttl_secs = 300;
        app.stats.context_tokens = TokenCount::new(10_000);
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

    #[test]
    fn status_bar_uses_pipe_separators() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.stats.context_tokens = TokenCount::new(10_000);
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        // model │ thinking │ context
        assert!(content.contains("│"), "missing │ separator: {content}");
        assert!(
            content.contains("thinking:"),
            "missing thinking: label: {content}"
        );
    }

    #[test]
    fn status_bar_shows_usage_bars() {
        use mush_ai::oauth::usage::{OAuthUsage, UsageWindow};

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.oauth_usage = Some(OAuthUsage {
            five_hour: Some(UsageWindow {
                utilization: 37.0,
                resets_at: chrono::Utc::now() + chrono::TimeDelta::minutes(150),
            }),
            seven_day: Some(UsageWindow {
                utilization: 26.0,
                resets_at: chrono::Utc::now() + chrono::TimeDelta::days(4),
            }),
        });
        let buf = render_status(&app, 160, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("5h"), "missing 5h label: {content}");
        assert!(content.contains("37%"), "missing 5h percentage: {content}");
        assert!(content.contains("7d"), "missing 7d label: {content}");
        assert!(content.contains("26%"), "missing 7d percentage: {content}");
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
