use ratatui::buffer::{Buffer, CellDiffOption, CellWidth};
use ratatui::layout::{Constraint, HorizontalAlignment, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use mush_ai::types::{Dollars, ThinkingLevel, TokenCount};

use crate::app::App;
use crate::path_utils::truncate_path;

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
thread_local! {
    static LEFT_SPANS_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_left_spans_call_count() {
    LEFT_SPANS_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn left_spans_call_count() -> usize {
    LEFT_SPANS_CALLS.with(Cell::get)
}

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

struct StatusSpansBuilder {
    spans: Vec<Span<'static>>,
    sep: Span<'static>,
    has_content: bool,
}

impl StatusSpansBuilder {
    fn new(theme: &crate::theme::Theme) -> Self {
        Self {
            spans: vec![Span::styled(" ", theme.dim)],
            sep: Span::styled(" │ ", theme.input_border),
            has_content: false,
        }
    }

    fn section_break(&mut self) {
        if self.has_content {
            self.spans.push(self.sep.clone());
        }
    }

    fn push_inline(&mut self, span: Span<'static>) {
        self.spans.push(span);
        self.has_content = true;
    }

    fn extend_inline(&mut self, spans: impl IntoIterator<Item = Span<'static>>) {
        for span in spans {
            self.push_inline(span);
        }
    }

    fn into_spans(self) -> Vec<Span<'static>> {
        self.spans
    }
}

struct LeftStatus {
    spans: Vec<Span<'static>>,
    cache_timer_text: Option<String>,
}

fn left_status(app: &App, width: u16) -> LeftStatus {
    #[cfg(test)]
    LEFT_SPANS_CALLS.with(|c| c.set(c.get() + 1));

    let dim = app.theme.dim;

    let normalized = app.thinking_level.normalize_visible();
    let thinking_label = normalized.effort_name().unwrap_or("off");

    let mut b = StatusSpansBuilder::new(&app.theme);
    let mut cache_timer_text = None;

    if app.interaction.status_bar.show_pane_indicator
        && let Some((pane_idx, pane_count)) = app.pane_info
    {
        b.push_inline(Span::styled(
            format!("[{pane_idx}/{pane_count}] "),
            app.theme.status_model,
        ));
    }
    b.push_inline(Span::styled(
        app.model_id.abbreviated(),
        app.theme.status_model,
    ));

    if app.interaction.status_bar.show_thinking {
        b.section_break();
        b.push_inline(Span::styled(
            format!("thinking: {thinking_label}"),
            if app.thinking_level == ThinkingLevel::Off {
                dim
            } else {
                app.theme.status_model
            },
        ));
    }

    if app.interaction.show_token_counters {
        if app.stats.input_tokens > TokenCount::ZERO {
            b.push_inline(Span::styled(
                format!(" ↑{}", format_tokens(app.stats.input_tokens)),
                dim,
            ));
        }
        if app.stats.output_tokens > TokenCount::ZERO {
            b.push_inline(Span::styled(
                format!(" ↓{}", format_tokens(app.stats.output_tokens)),
                dim,
            ));
        }
        if app.stats.cache_read_tokens > TokenCount::ZERO {
            b.push_inline(Span::styled(
                format!(" R{}", format_tokens(app.stats.cache_read_tokens)),
                dim,
            ));
        }
        if app.stats.cache_write_tokens > TokenCount::ZERO {
            b.push_inline(Span::styled(
                format!(" W{}", format_tokens(app.stats.cache_write_tokens)),
                dim,
            ));
        }
    }

    if app.interaction.status_bar.show_context && app.stats.context_tokens > TokenCount::ZERO {
        let ctx = format_tokens(app.stats.context_tokens);
        let window = format_tokens(app.stats.context_window);
        let pct = app
            .stats
            .context_tokens
            .percent_of(app.stats.context_window) as u64;

        let cache_remaining = app.cache.remaining_secs();
        let cache_tiers = cache_warning_display_for_app(app);
        let ctx_style = if pct > 75 {
            app.theme.context_danger
        } else if app.cache.ttl_secs == 0 {
            dim
        } else {
            match cache_remaining {
                Some(r) if r > crate::app::CACHE_WARN_SECS => app.theme.context_ok,
                Some(r) if r > 0 => app.theme.context_warn,
                Some(0) => app.theme.context_cold,
                _ => dim,
            }
        };

        let CacheSuffix {
            display: cache_suffix,
            ticking,
        } = cache_suffix_for_app(app, cache_tiers, cache_remaining);
        cache_timer_text = ticking;

        b.section_break();
        b.push_inline(Span::styled(
            format!("{ctx}/{window}{cache_suffix}"),
            ctx_style,
        ));
    }

    if app.interaction.show_cost && app.stats.total_cost > Dollars::ZERO {
        b.push_inline(Span::styled(format!(" {}", app.stats.total_cost), dim));
    }

    if app.interaction.status_bar.show_oauth_usage
        && app.oauth_usage_provider.as_deref() == Some("anthropic")
        && app.model_id.as_str().contains("claude")
        && let Some(ref usage) = app.oauth_usage
    {
        let bar_w = usage_bar_width(width);
        if let Some(ref w) = usage.five_hour {
            let pace = w.pace(mush_ai::oauth::usage::OAuthUsage::FIVE_HOUR);
            b.section_break();
            b.push_inline(Span::styled("5h ", dim));
            b.extend_inline(super::usage_bar::render_usage_bar_width(
                w.utilization,
                pace,
                bar_w,
            ));
            b.push_inline(Span::styled(format!(" {}%", w.utilization as u32), dim));
        }
        if let Some(ref w) = usage.seven_day {
            let pace = w.pace(mush_ai::oauth::usage::OAuthUsage::SEVEN_DAY);
            b.section_break();
            b.push_inline(Span::styled("7d ", dim));
            b.extend_inline(super::usage_bar::render_usage_bar_width(
                w.utilization,
                pace,
                bar_w,
            ));
            b.push_inline(Span::styled(format!(" {}%", w.utilization as u32), dim));
        }
    }

    if app.interaction.status_bar.show_status_messages
        && let Some(label) = app.activity_label()
    {
        b.section_break();
        b.push_inline(Span::styled(
            format!(
                "{} ",
                crate::widgets::working_icon::working_icon(app.shimmer_phase())
            ),
            dim,
        ));
        b.extend_inline(crate::widgets::shimmer::shimmer_spans(
            &label,
            &app.theme.shimmer,
            app.shimmer_phase(),
            dim,
        ));
    }

    if app.interaction.status_bar.show_background_alerts
        && let Some(ref alert) = app.background_alert
    {
        b.section_break();
        b.push_inline(Span::styled(alert.clone(), app.theme.alert));
    }

    if !app.children.is_empty() {
        let running = app.running_children();
        let total = app.children.len();
        b.section_break();
        b.push_inline(Span::styled(
            format!("{running}/{total} agents"),
            if running > 0 {
                app.theme.status_model
            } else {
                dim
            },
        ));
    }

    LeftStatus {
        spans: b.into_spans(),
        cache_timer_text,
    }
}

#[cfg(test)]
fn left_spans(app: &App, width: u16) -> Vec<Span<'static>> {
    left_status(app, width).spans
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheSuffix {
    display: String,
    ticking: Option<String>,
}

fn cache_suffix_for_app(
    app: &crate::app::App,
    cache_tiers: Option<String>,
    cache_remaining: Option<u16>,
) -> CacheSuffix {
    match (cache_tiers, cache_remaining) {
        (Some(tiers), _) => CacheSuffix {
            display: format!(" {tiers}"),
            ticking: Some(tiers),
        },
        (_, Some(r)) if r > 0 => {
            let mins = r / 60;
            let secs = r % 60;
            let timer = format!("{mins}:{secs:02}");
            CacheSuffix {
                display: format!(" {timer}"),
                ticking: Some(timer),
            }
        }
        (_, Some(0)) => {
            let elapsed = app.cache.elapsed_secs().unwrap_or(0);
            let display =
                if elapsed < (app.cache.ttl_secs as u64) + crate::app::CACHE_COLD_DISPLAY_SECS {
                    " cold".into()
                } else {
                    String::new()
                };
            CacheSuffix {
                display,
                ticking: None,
            }
        }
        _ => CacheSuffix {
            display: String::new(),
            ticking: None,
        },
    }
}

fn cache_warning_display_for_app(app: &crate::app::App) -> Option<String> {
    let provider = if app.model_id.as_str().contains("codex") {
        mush_ai::types::Provider::Custom("openai-codex".into())
    } else if app.model_id.as_str().starts_with("gpt-") || app.model_id.as_str().contains("/gpt-") {
        mush_ai::types::Provider::Custom("openai".into())
    } else if app.model_id.as_str().contains("claude") {
        mush_ai::types::Provider::Anthropic
    } else {
        return None;
    };

    let retention =
        if matches!(provider, mush_ai::types::Provider::Anthropic) && app.cache.ttl_secs >= 3600 {
            Some(mush_ai::types::CacheRetention::Long)
        } else {
            None
        };

    crate::prompt_cache::cache_warning_display(
        &provider,
        retention.as_ref(),
        app.cache.elapsed_secs(),
    )
}

fn ensure_status_bar_cache(app: &App, width: u16) {
    {
        let slot = app.render_state.status_bar_cache.borrow();
        if let Some(c) = slot.as_ref()
            && c.width == width
        {
            return;
        }
    }

    let left_status = left_status(app, width);
    let spans = left_status.spans;
    let cache_timer_text = left_status.cache_timer_text;
    let right_text = if app.interaction.status_bar.show_cwd {
        format!("{} ", truncate_path(&app.cwd, 30))
    } else {
        String::new()
    };
    let left_width = spans_cell_width(&spans);
    let right_width = usize::from(right_text.as_str().cell_width());
    let total = left_width + 2 + right_width;
    let height = if total > width as usize { 2 } else { 1 };

    *app.render_state.status_bar_cache.borrow_mut() = Some(crate::app::CachedStatusBar {
        width,
        spans,
        right_text,
        cache_timer_text,
        height,
    });
}

fn spans_cell_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| usize::from(span.content.as_ref().cell_width()))
        .sum()
}

pub fn status_bar_height(app: &App, width: u16) -> u16 {
    ensure_status_bar_cache(app, width);
    app.render_state
        .status_bar_cache
        .borrow()
        .as_ref()
        .map(|c| c.height)
        .unwrap_or(1)
}

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
        ensure_status_bar_cache(self.app, area.width);
        let cache = self.app.render_state.status_bar_cache.borrow();
        let Some(data) = cache.as_ref() else {
            return;
        };

        let main = area;
        let right_width = (data.right_text.as_str().cell_width() + 1).min(main.width / 2);
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(right_width)]).areas(main);

        let left_paragraph =
            Paragraph::new(Line::from(data.spans.clone())).wrap(Wrap { trim: false });
        left_paragraph.render(left_area, buf);
        mark_cache_timer_cells(data.cache_timer_text.as_deref(), left_area, buf);

        if right_width > 0 {
            Paragraph::new(Line::from(vec![Span::styled(
                data.right_text.clone(),
                self.app.theme.status_dim,
            )]))
            .alignment(HorizontalAlignment::Right)
            .render(right_area, buf);
        }
    }
}

fn mark_cache_timer_cells(timer: Option<&str>, area: Rect, buf: &mut Buffer) {
    let Some(timer) = timer else {
        return;
    };
    mark_text_cells(area, buf, timer, CellDiffOption::AlwaysUpdate);
}

fn mark_text_cells(area: Rect, buf: &mut Buffer, needle: &str, diff_option: CellDiffOption) {
    if needle.is_empty() {
        return;
    }
    let symbols: Vec<String> = needle.chars().map(|ch| ch.to_string()).collect();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if x as usize + symbols.len() > area.right() as usize {
                break;
            }
            if symbols
                .iter()
                .enumerate()
                .all(|(offset, symbol)| buf[(x + offset as u16, y)].symbol() == symbol)
            {
                for offset in 0..symbols.len() {
                    if let Some(cell) = buf.cell_mut((x + offset as u16, y)) {
                        cell.set_diff_option(diff_option);
                    }
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::buffer_to_string;
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
        app.interaction.show_token_counters = true;
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
    fn status_bar_hides_token_counters_by_default() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.stats.input_tokens = TokenCount::new(45_000);
        app.stats.output_tokens = TokenCount::new(12_000);
        app.stats.cache_read_tokens = TokenCount::new(8_000);
        app.stats.cache_write_tokens = TokenCount::new(2_000);
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(
            !content.contains("↑45k"),
            "input counter should be hidden by default: {content}"
        );
        assert!(
            !content.contains("↓12k"),
            "output counter should be hidden by default: {content}"
        );
        assert!(
            !content.contains("R8k"),
            "cache-read counter should be hidden by default: {content}"
        );
        assert!(
            !content.contains("W2k"),
            "cache-write counter should be hidden by default: {content}"
        );
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
    fn status_bar_shows_activity_verb_when_streaming() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.start_streaming();
        app.push_text_delta("hi");
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("outputting…"),
            "expected 'outputting…' in status: {content}"
        );
    }

    #[test]
    fn status_bar_shows_using_tool_when_running() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.active_tools
            .push(crate::display_types::ActiveToolState {
                tool_call_id: "t1".into(),
                name: "bash".into(),
                summary: String::new(),
                live_output: None,
                status: crate::display_types::ToolCallStatus::Running,
                output: None,
            });
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("using bash…"),
            "expected 'using bash…' in status: {content}"
        );
    }

    #[test]
    fn status_bar_activity_verb_overrides_status_message() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.status = Some("press ctrl+c again to exit".into());
        app.start_streaming();
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("thinking…"));
        assert!(!content.contains("press ctrl+c again to exit"));
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
        assert_eq!(status_bar_height(&app, 80), 1);
        assert_eq!(status_bar_height(&app, 200), 1);
    }

    #[test]
    fn status_bar_wraps_to_two_lines_when_content_exceeds_width() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.interaction.show_token_counters = true;
        app.stats.input_tokens = TokenCount::new(45_000);
        app.stats.output_tokens = TokenCount::new(12_000);
        app.stats.cache_read_tokens = TokenCount::new(8_000);
        app.stats.cache_write_tokens = TokenCount::new(2_000);
        app.stats.context_tokens = TokenCount::new(45_000);
        app.interaction.show_cost = true;
        app.stats.total_cost = Dollars::new(0.1234);
        let height = status_bar_height(&app, 40);
        assert_eq!(height, 2, "expected 2 lines for narrow width");
        let buf = render_status(&app, 40, height);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("45k") || content.contains("200k"),
            "expected wrapped content, got: {content}"
        );
    }

    #[test]
    fn status_bar_height_uses_display_width_not_byte_length() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.interaction.show_token_counters = true;
        app.stats.input_tokens = TokenCount::new(45_000);
        app.stats.output_tokens = TokenCount::new(12_000);
        app.stats.cache_read_tokens = TokenCount::new(8_000);
        app.stats.cache_write_tokens = TokenCount::new(2_000);
        app.stats.context_tokens = TokenCount::new(45_000);
        app.cwd = "~".into();

        let spans = left_spans(&app, 200);
        let display_width = spans_cell_width(&spans);
        let byte_length: usize = spans.iter().map(|s| s.content.len()).sum();
        assert!(
            display_width < byte_length,
            "test scenario must have multi-byte chars: display={display_width}, bytes={byte_length}"
        );

        let right_width = app.cwd.chars().count() + 1;
        let width = (display_width + 2 + right_width) as u16;
        let height = status_bar_height(&app, width);
        assert_eq!(
            height, 1,
            "expected 1 line when content fits by display width \
             (display={display_width}, bytes={byte_length}, width={width})"
        );
    }

    #[test]
    fn status_bar_cwd_uses_terminal_cell_width_for_halfwidth_dakuten() {
        use ratatui::buffer::CellWidth;

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.cwd = "abcｶﾞ".repeat(4);

        let right_text = format!("{} ", truncate_path(&app.cwd, 30));
        let ratatui_width = usize::from(right_text.as_str().cell_width());
        let unicode_width = unicode_width::UnicodeWidthStr::width(right_text.as_str());
        assert!(
            ratatui_width > unicode_width,
            "test cwd must expose a width-model difference: ratatui={ratatui_width}, unicode={unicode_width}"
        );

        let left_width = spans_cell_width(&left_spans(&app, 200));
        let width = (left_width + 2 + unicode_width) as u16;
        assert_eq!(
            status_bar_height(&app, width),
            2,
            "status bar should wrap when cwd fits only under unicode-width counting"
        );

        let content = buffer_to_string(&render_status(&app, 60, 1));
        assert!(
            content.lines().any(|row| row.matches("ｶﾞ").count() == 4),
            "right cwd should keep every halfwidth dakuten pair visible, got:\n{content}"
        );
    }

    #[test]
    fn a_status_never_lands_in_the_bar() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.status = Some(crate::app_state::Status::error("clipboard failed: wl-copy"));

        let spans = left_spans(&app, 200);

        assert!(
            spans
                .iter()
                .all(|span| !span.content.contains("clipboard failed")),
            "{spans:?}"
        );
    }

    #[test]
    fn status_bar_left_side_uses_terminal_cell_width_for_halfwidth_dakuten() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.interaction.status_bar.show_cwd = false;
        app.model_id = "abcｶﾞ".repeat(4).into();

        let spans = left_spans(&app, 200);
        let ratatui_width = spans_cell_width(&spans);
        let unicode_width: usize = spans.iter().map(|s| s.width()).sum();
        assert!(
            ratatui_width > unicode_width,
            "test status must expose a width-model difference: ratatui={ratatui_width}, unicode={unicode_width}"
        );

        let width = (unicode_width + 2) as u16;
        assert_eq!(
            status_bar_height(&app, width),
            2,
            "status bar should wrap when the left status fits only under unicode-width counting"
        );
    }

    #[test]
    fn status_bar_shrinks_usage_bars_on_narrow_widths() {
        use chrono;
        use mush_ai::oauth::usage::{OAuthUsage, UsageWindow};
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.model_id = "claude-sonnet-4-5".into();
        app.oauth_usage_provider = Some("anthropic".into());
        app.oauth_usage = Some(OAuthUsage {
            five_hour: Some(UsageWindow {
                utilization: 50.0,
                resets_at: chrono::Utc::now() + chrono::TimeDelta::minutes(120),
            }),
            seven_day: None,
        });
        let wide_spans = left_spans(&app, 200);
        let wide_bar_cells = wide_spans
            .iter()
            .filter(|s| s.content.as_ref() == "▀" || s.content.as_ref() == "░")
            .count();
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
        assert!(content.contains("10k/200k"));
        assert!(content.contains(":"));
    }

    #[test]
    fn status_bar_marks_cache_countdown_cells_always_update() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.cache.ttl_secs = 300;
        app.stats.context_tokens = TokenCount::new(10_000);
        app.cache.refresh();
        let buf = render_status(&app, 120, 1);

        let context_x = find_symbol_sequence_x(&buf, &["1", "0", "k", "/", "2", "0", "0", "k"])
            .expect("expected context segment");
        for x in context_x..context_x + 8 {
            assert_eq!(
                buf[(x, 0)].diff_option,
                ratatui::buffer::CellDiffOption::None,
                "static context cell at x={x} should use normal diffing"
            );
        }

        let timer_x = find_symbol_sequence_x(&buf, &["1", "0", "k", "/", "2", "0", "0", "k", " "])
            .expect("expected cache timer suffix")
            + 9;
        for x in timer_x..timer_x + 4 {
            assert_eq!(
                buf[(x, 0)].diff_option,
                ratatui::buffer::CellDiffOption::AlwaysUpdate,
                "cache countdown cell at x={x} should force updates"
            );
        }
    }

    #[test]
    fn status_bar_shows_short_and_long_cache_tiers_for_hour_claude_cache() {
        let mut app = App::new("claude-sonnet-4".into(), TokenCount::new(200_000));
        app.cache.ttl_secs = 3600;
        app.stats.context_tokens = TokenCount::new(10_000);
        app.cache.refresh();

        let buf = render_status(&app, 140, 1);
        let content = buffer_to_string(&buf);

        assert!(
            content.contains("4:00/59:00") || content.contains("3:59/58:59"),
            "expected both short and long cache tier countdowns: {content}"
        );
    }

    #[test]
    fn status_bar_hides_cold_when_cache_timer_disabled() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.cache.ttl_secs = 0;
        app.stats.context_tokens = TokenCount::new(10_000);
        app.cache.refresh();
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(content.contains("10k/200k"));
        assert!(
            !content.contains("cold"),
            "expected no 'cold' suffix with idle timer off, got: {content}"
        );
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
        assert!(content.contains("│"), "missing │ separator: {content}");
        assert!(
            content.contains("thinking:"),
            "missing thinking: label: {content}"
        );
    }

    #[test]
    fn status_bar_dividers_use_input_border_style() {
        let mut app = App::new("test-model".into(), TokenCount::new(200_000));
        app.interaction.status_bar.show_thinking = true;
        let buf = render_status(&app, 120, 1);

        let mut found = None;
        for x in 0..buf.area.width {
            let cell = &buf[(x, 0)];
            if cell.symbol() == "│" {
                found = Some((x, cell.style()));
                break;
            }
        }

        let (x, style) = found.expect("expected a `│` divider somewhere on the status bar");
        let expected = app.theme.input_border;
        assert_eq!(
            style.fg, expected.fg,
            "divider at col {x} fg should match theme.input_border, got {:?} expected {:?}",
            style.fg, expected.fg
        );
        assert_eq!(
            style.add_modifier, expected.add_modifier,
            "divider at col {x} modifiers should match theme.input_border, got {:?} expected {:?}",
            style.add_modifier, expected.add_modifier
        );
    }

    #[test]
    fn status_bar_hides_thinking_when_toggle_off() {
        let mut app = App::new("test-model".into(), TokenCount::new(200_000));
        app.interaction.status_bar.show_thinking = false;
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(
            !content.contains("thinking:"),
            "thinking segment should be hidden: {content}"
        );
    }

    #[test]
    fn status_bar_hides_context_when_toggle_off() {
        let mut app = App::new("test-model".into(), TokenCount::new(200_000));
        app.stats.context_tokens = TokenCount::new(10_000);
        app.interaction.status_bar.show_context = false;
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(
            !content.contains("10k/200k"),
            "context segment should be hidden: {content}"
        );
    }

    #[test]
    fn status_bar_hides_cwd_when_toggle_off() {
        let mut app = App::new("test-model".into(), TokenCount::new(200_000));
        app.cwd = "/home/user/dev/mush".into();
        app.interaction.status_bar.show_cwd = false;
        let buf = render_status(&app, 120, 1);
        let content = buffer_to_string(&buf);
        assert!(
            !content.contains("mush"),
            "cwd segment should be hidden: {content}"
        );
    }

    #[test]
    fn status_bar_hides_oauth_usage_when_toggle_off() {
        use mush_ai::oauth::usage::{OAuthUsage, UsageWindow};
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.oauth_usage = Some(OAuthUsage {
            five_hour: Some(UsageWindow {
                utilization: 37.0,
                resets_at: chrono::Utc::now() + chrono::TimeDelta::minutes(150),
            }),
            seven_day: None,
        });
        app.interaction.status_bar.show_oauth_usage = false;
        let buf = render_status(&app, 140, 1);
        let content = buffer_to_string(&buf);
        assert!(
            !content.contains("5h"),
            "5h oauth bar should be hidden: {content}"
        );
    }

    #[test]
    fn status_bar_hides_anthropic_usage_for_codex_model() {
        use mush_ai::oauth::usage::{OAuthUsage, UsageWindow};
        let mut app = App::new("openai/gpt-5.1-codex".into(), TokenCount::new(200_000));
        app.oauth_usage_provider = Some("anthropic".into());
        app.oauth_usage = Some(OAuthUsage {
            five_hour: Some(UsageWindow {
                utilization: 37.0,
                resets_at: chrono::Utc::now() + chrono::TimeDelta::minutes(150),
            }),
            seven_day: None,
        });

        let buf = render_status(&app, 140, 1);
        let content = buffer_to_string(&buf);

        assert!(
            !content.contains("5h"),
            "stale claude usage leaked: {content}"
        );
    }

    #[test]
    fn status_bar_shows_usage_bars() {
        use mush_ai::oauth::usage::{OAuthUsage, UsageWindow};

        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.model_id = "claude-sonnet-4".into();
        app.oauth_usage_provider = Some("anthropic".into());
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

    fn render_full_ui(app: &App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut screen = crate::screen::Screen::new();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let _ = crate::widgets::status_bar::status_bar_height(app, area.width);
                let _ = crate::ui::Ui::new(app).cursor_position(area);
                screen.render(app, area, frame.buffer_mut());
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn left_spans_built_once_per_frame() {
        let mut app = App::new("test-model".into(), TokenCount::new(200_000));
        app.stats.input_tokens = TokenCount::new(45_000);
        app.stats.output_tokens = TokenCount::new(12_000);

        reset_left_spans_call_count();
        let _ = render_full_ui(&app, 120, 20);
        let calls = left_spans_call_count();
        assert_eq!(
            calls, 1,
            "left_spans should be built once per frame, was {calls}"
        );
    }

    fn find_symbol_sequence_x(buf: &Buffer, symbols: &[&str]) -> Option<u16> {
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let fits = x as usize + symbols.len() <= buf.area.width as usize;
                if fits
                    && symbols
                        .iter()
                        .enumerate()
                        .all(|(offset, symbol)| buf[(x + offset as u16, y)].symbol() == *symbol)
                {
                    return Some(x);
                }
            }
        }
        None
    }

    #[test]
    fn status_bar_right_side_has_symmetric_trailing_padding() {
        let mut app = App::new("test-model".into(), TokenCount::new(200_000));
        app.cwd = "~/dev/mush".into();
        let buf = render_status(&app, 60, 1);
        let content = buffer_to_string(&buf);
        let line = content.lines().next().unwrap_or("");
        assert!(
            line.contains("~/dev/mush"),
            "cwd should render on status bar: {line:?}"
        );
        assert!(
            line.ends_with(' '),
            "right edge of status bar should have a trailing pad space, got {line:?}"
        );
    }
}
