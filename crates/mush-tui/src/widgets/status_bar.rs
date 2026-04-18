//! status bar widget - model info, cost, token usage

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, HorizontalAlignment, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use unicode_width::UnicodeWidthStr;

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
    if app.interaction.mode != crate::app::AppMode::ToolConfirm {
        return None;
    }
    Some(if let Some(ref prompt) = app.interaction.confirm_prompt {
        format!("run {prompt}? (y/n)")
    } else {
        "confirm tool? (y/n)".into()
    })
}

/// width of the usage bars in the status bar. scales with window size
fn usage_bar_width(total_width: u16) -> usize {
    if total_width >= 140 {
        20
    } else if total_width >= 100 {
        12
    } else if total_width >= 80 {
        8
    } else {
        5
    }
}

/// build the left-side info spans
fn left_spans(app: &App, width: u16) -> Vec<Span<'static>> {
    let dim = app.theme.dim;

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
            app.theme.status_model,
        ));
    }

    let sep = Span::styled(" │ ", dim);

    spans.extend([
        Span::styled(app.model_id.abbreviated(), app.theme.status_model),
        sep.clone(),
        Span::styled(
            format!("thinking: {thinking_label}"),
            if app.thinking_level == ThinkingLevel::Off {
                dim
            } else {
                app.theme.status_model
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
        let cache_remaining = app.cache.remaining_secs();
        let ctx_style = if pct > 75 {
            // context pressure always takes priority
            app.theme.context_danger
        } else if app.cache.ttl_secs == 0 {
            // caching disabled for this provider/retention
            dim
        } else {
            match cache_remaining {
                Some(r) if r > crate::app::CACHE_WARN_SECS => app.theme.context_ok,
                Some(r) if r > 0 => app.theme.context_warn,
                Some(0) => app.theme.context_cold,
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
                let elapsed = app.cache.elapsed_secs().unwrap_or(0);
                if elapsed < (app.cache.ttl_secs as u64) + crate::app::CACHE_COLD_DISPLAY_SECS {
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

    if app.interaction.show_cost && app.stats.total_cost > Dollars::ZERO {
        spans.push(Span::styled(format!(" {}", app.stats.total_cost), dim));
    }

    // oauth usage bars
    if let Some(ref usage) = app.oauth_usage {
        let bar_w = usage_bar_width(width);
        if let Some(ref w) = usage.five_hour {
            let pace = w.pace(mush_ai::oauth::usage::OAuthUsage::FIVE_HOUR);
            spans.push(sep.clone());
            spans.push(Span::styled("5h ", dim));
            spans.extend(super::usage_bar::render_usage_bar_width(
                w.utilization,
                pace,
                bar_w,
            ));
            spans.push(Span::styled(format!(" {}%", w.utilization as u32), dim));
        }
        if let Some(ref w) = usage.seven_day {
            let pace = w.pace(mush_ai::oauth::usage::OAuthUsage::SEVEN_DAY);
            spans.push(sep.clone());
            spans.push(Span::styled("7d ", dim));
            spans.extend(super::usage_bar::render_usage_bar_width(
                w.utilization,
                pace,
                bar_w,
            ));
            spans.push(Span::styled(format!(" {}%", w.utilization as u32), dim));
        }
    }

    if let Some(ref status) = app.status
        && status != "config reloaded"
    {
        spans.push(sep.clone());
        spans.push(Span::styled(status.clone(), dim));
    }

    // scroll mode indicator
    if app.interaction.mode == crate::app::AppMode::Scroll {
        let unit_label = match app.navigation.scroll_unit {
            crate::app::ScrollUnit::Block => "blocks",
            crate::app::ScrollUnit::Message => "messages",
        };
        spans.push(sep.clone());
        spans.push(Span::styled(
            format!("scroll: {unit_label} (b)"),
            app.theme.scroll_indicator,
        ));
    }

    // scroll position indicator (only when scrolled away from bottom)
    if app.scroll_offset > 0 {
        let total = app.render_state.total_content_lines.get();
        let visible = app.render_state.visible_area_height.get();
        let max_scroll = total.saturating_sub(visible);
        if max_scroll > 0 {
            // scroll_offset is lines from bottom, convert to percentage from top
            let from_top = max_scroll.saturating_sub(app.scroll_offset);
            let pct = (from_top as f64 / max_scroll as f64 * 100.0) as u16;
            spans.push(Span::styled(format!(" {pct}%"), app.theme.scroll_indicator));
        }
    }

    // background pane alerts
    if let Some(ref alert) = app.background_alert {
        spans.push(sep.clone());
        spans.push(Span::styled(alert.clone(), app.theme.alert));
    }

    spans
}

/// calculate how many lines the status bar needs (1 or 2, +1 for confirm prompt).
/// the render pass uses the same logic to pack overflow spans onto a second line
pub fn status_bar_height(app: &App, width: u16) -> u16 {
    let spans = left_spans(app, width);
    // use display width, not byte length: `│ ↑ ↓ ▀ ░` etc are multi-byte
    // in utf-8 but 1 column wide. ratatui's Paragraph::wrap uses display
    // width too, so mixing up byte counts here causes a spurious second
    // line with blank content when unicode chars overshoot the byte count
    let left_width: usize = spans.iter().map(|s| s.width()).sum();
    let right = truncate_path(&app.cwd, 30);
    let right_width = UnicodeWidthStr::width(right.as_str());
    let total = left_width + 2 + right_width; // 2 for padding between left and right
    let wraps = total > width as usize;
    if confirm_text(app).is_some() {
        if wraps { 3 } else { 2 }
    } else if wraps {
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
        let dim = self.app.theme.dim;

        // build all the info spans
        let spans = left_spans(self.app, area.width);
        let right_text = truncate_path(&self.app.cwd, 30);
        let confirm = confirm_text(self.app);

        // split area: optional confirm line at the bottom, main area above
        let (main, confirm_area) = if let Some(ref confirm_str) = confirm {
            let [main, conf] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
            (main, Some((conf, confirm_str.clone())))
        } else {
            (area, None)
        };

        // split main into left (wrapping content) and right (cwd, single line).
        // right column has fixed width = cwd display width + 1 space gap; shrinks if needed.
        // `…` (from truncate_path) and any unicode path components use display width
        let right_width =
            (UnicodeWidthStr::width(right_text.as_str()) as u16 + 1).min(main.width / 2);
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(right_width)]).areas(main);

        // left: wrap spans across lines automatically using Paragraph::wrap.
        // trim:false so our leading space (Span::raw(" ")) is preserved
        let left_paragraph = Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false });
        left_paragraph.render(left_area, buf);

        // right: right-align cwd on the first line of the right column
        if right_width > 0 {
            Paragraph::new(Line::from(vec![Span::styled(
                right_text,
                self.app.theme.status_dim,
            )]))
            .alignment(HorizontalAlignment::Right)
            .render(right_area, buf);
        }

        // confirm prompt on its own line at the bottom
        if let Some((conf_area, text)) = confirm_area {
            Paragraph::new(Line::from(vec![
                Span::styled(" ", dim),
                Span::styled(text, self.app.theme.confirm),
            ]))
            .render(conf_area, buf);
        }
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
        assert!(
            content.contains("c-s-4"),
            "expected abbreviated model: {content}"
        );
    }

    #[test]
    fn status_bar_shows_cost_and_context() {
        let mut app = App::new("test-model".into(), TokenCount::new(200_000));
        app.stats.total_cost = Dollars::new(0.0123);
        app.interaction.show_cost = true;
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
    fn status_bar_wraps_to_two_lines_when_content_exceeds_width() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.stats.input_tokens = TokenCount::new(45_000);
        app.stats.output_tokens = TokenCount::new(12_000);
        app.stats.cache_read_tokens = TokenCount::new(8_000);
        app.stats.cache_write_tokens = TokenCount::new(2_000);
        app.stats.context_tokens = TokenCount::new(45_000);
        app.interaction.show_cost = true;
        app.stats.total_cost = Dollars::new(0.1234);
        // narrow width forces wrapping
        let height = status_bar_height(&app, 40);
        assert_eq!(height, 2, "expected 2 lines for narrow width");
        let buf = render_status(&app, 40, height);
        let content = buffer_to_string(&buf);
        // content that exists should still be visible somewhere
        assert!(
            content.contains("45k") || content.contains("200k"),
            "expected wrapped content, got: {content}"
        );
    }

    #[test]
    fn status_bar_height_uses_display_width_not_byte_length() {
        // regression: content.len() sums bytes, so multi-byte unicode chars
        // like │ ↑ ↓ R W (3 bytes each, 1 column wide) overshoot the width
        // check and force a spurious 2-line status bar. ratatui's Paragraph::wrap
        // uses display width so only renders 1 line, leaving a blank second line
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.stats.input_tokens = TokenCount::new(45_000);
        app.stats.output_tokens = TokenCount::new(12_000);
        app.stats.cache_read_tokens = TokenCount::new(8_000);
        app.stats.cache_write_tokens = TokenCount::new(2_000);
        app.stats.context_tokens = TokenCount::new(45_000);
        app.cwd = "~".into();

        let spans = left_spans(&app, 200);
        let display_width: usize = spans.iter().map(|s| s.width()).sum();
        let byte_length: usize = spans.iter().map(|s| s.content.len()).sum();
        assert!(
            display_width < byte_length,
            "test scenario must have multi-byte chars: display={display_width}, bytes={byte_length}"
        );

        // pick a width that fits display + cwd but would trip the byte-length check.
        // right column = cwd (1 char) + 1 space. 2 for padding between left/right.
        let right_width = app.cwd.chars().count();
        let width = (display_width + 2 + right_width) as u16;
        let height = status_bar_height(&app, width);
        assert_eq!(
            height, 1,
            "expected 1 line when content fits by display width \
             (display={display_width}, bytes={byte_length}, width={width})"
        );
    }

    #[test]
    fn status_bar_shrinks_usage_bars_on_narrow_widths() {
        use chrono;
        use mush_ai::oauth::usage::{OAuthUsage, UsageWindow};
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.oauth_usage = Some(OAuthUsage {
            five_hour: Some(UsageWindow {
                utilization: 50.0,
                resets_at: chrono::Utc::now() + chrono::TimeDelta::minutes(120),
            }),
            seven_day: None,
        });
        // spans at wide width should include the default 20-cell bar
        let wide_spans = left_spans(&app, 200);
        let wide_bar_cells = wide_spans
            .iter()
            .filter(|s| s.content.as_ref() == "▀" || s.content.as_ref() == "░")
            .count();
        // spans at narrow width should have fewer bar cells
        let narrow_spans = left_spans(&app, 60);
        let narrow_bar_cells = narrow_spans
            .iter()
            .filter(|s| s.content.as_ref() == "▀" || s.content.as_ref() == "░")
            .count();
        assert!(
            narrow_bar_cells < wide_bar_cells,
            "expected bars to shrink on narrow widths: narrow={narrow_bar_cells} wide={wide_bar_cells}"
        );
    }

    #[test]
    fn truncate_path_short_unchanged() {
        assert_eq!(truncate_path("~/dev/mush", 30), "~/dev/mush");
    }

    #[test]
    fn status_bar_shows_cache_countdown() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.cache.ttl_secs = 300;
        app.stats.context_tokens = TokenCount::new(10_000);
        app.cache.refresh();
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

    #[test]
    fn status_bar_shows_scroll_block_mode() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.interaction.mode = crate::app::AppMode::Scroll;
        app.navigation.scroll_unit = crate::app::ScrollUnit::Block;
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("blocks"),
            "missing block mode indicator: {content}"
        );
    }

    #[test]
    fn status_bar_shows_scroll_message_mode() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.interaction.mode = crate::app::AppMode::Scroll;
        app.navigation.scroll_unit = crate::app::ScrollUnit::Message;
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("messages"),
            "missing message mode indicator: {content}"
        );
    }
}
