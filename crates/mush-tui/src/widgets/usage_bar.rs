use ratatui::style::Style;
use ratatui::text::Span;

use crate::theme::Theme;

pub const METER_WIDTH: usize = 8;

#[must_use]
pub fn render_usage_meter(
    usage_pct: f32,
    pace_pct: f32,
    width: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let width = width.max(4);
    let used = pct_to_cells(usage_pct, width);
    let pace = pct_to_cells(pace_pct, width);
    let mut spans = Vec::with_capacity(3);
    push_run(&mut spans, "━", used, level_style(usage_pct, theme));
    push_run(
        &mut spans,
        "━",
        pace.saturating_sub(used),
        theme.input_border,
    );
    push_run(&mut spans, "─", width - used.max(pace), theme.dim);
    spans
}

#[must_use]
pub fn level_style(pct: f32, theme: &Theme) -> Style {
    if pct >= 80.0 {
        theme.context_danger
    } else if pct >= 50.0 {
        theme.context_warn
    } else {
        theme.status_model
    }
}

fn push_run(spans: &mut Vec<Span<'static>>, glyph: &str, cells: usize, style: Style) {
    if cells > 0 {
        spans.push(Span::styled(glyph.repeat(cells), style));
    }
}

fn pct_to_cells(pct: f32, width: usize) -> usize {
    ((pct / 100.0 * width as f32).round().max(0.0) as usize).min(width)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn the_meter_keeps_its_width() {
        let theme = Theme::dark();
        for (usage, pace) in [(0.0, 0.0), (100.0, 100.0), (37.0, 50.0), (80.0, 30.0)] {
            let spans = render_usage_meter(usage, pace, METER_WIDTH, &theme);
            assert_eq!(text(&spans).chars().count(), METER_WIDTH, "{usage} {pace}");
        }
    }

    #[test]
    fn an_empty_meter_is_a_faint_line() {
        let theme = Theme::dark();
        let spans = render_usage_meter(0.0, 0.0, METER_WIDTH, &theme);
        assert_eq!(text(&spans), "─".repeat(METER_WIDTH));
        assert_eq!(spans[0].style, theme.dim);
    }

    #[test]
    fn usage_under_pace_shows_the_headroom() {
        let theme = Theme::dark();
        let spans = render_usage_meter(25.0, 50.0, METER_WIDTH, &theme);
        assert_eq!(text(&spans), "━━━━────");
        assert_eq!(spans[0].content.as_ref(), "━━");
        assert_eq!(spans[0].style, theme.status_model);
        assert_eq!(spans[1].content.as_ref(), "━━");
        assert_eq!(spans[1].style, theme.input_border);
        assert_eq!(spans[2].style, theme.dim);
    }

    #[test]
    fn usage_over_pace_fills_in_its_level_colour() {
        let theme = Theme::dark();
        let spans = render_usage_meter(88.0, 30.0, METER_WIDTH, &theme);
        assert_eq!(text(&spans), "━━━━━━━─");
        assert_eq!(spans[0].style, theme.context_danger);
    }

    #[test]
    fn the_level_colour_follows_the_thresholds() {
        let theme = Theme::dark();
        assert_eq!(level_style(49.9, &theme), theme.status_model);
        assert_eq!(level_style(50.0, &theme), theme.context_warn);
        assert_eq!(level_style(79.9, &theme), theme.context_warn);
        assert_eq!(level_style(80.0, &theme), theme.context_danger);
    }

    #[test]
    fn cells_round_to_the_nearest() {
        assert_eq!(pct_to_cells(0.0, 20), 0);
        assert_eq!(pct_to_cells(2.4, 20), 0);
        assert_eq!(pct_to_cells(2.5, 20), 1);
        assert_eq!(pct_to_cells(50.0, 20), 10);
        assert_eq!(pct_to_cells(120.0, 20), 20);
        assert_eq!(pct_to_cells(-5.0, 20), 0);
    }
}
