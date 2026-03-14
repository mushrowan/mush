//! dual-layer usage bar using half-block characters
//!
//! renders two values (usage + pace) in a single row height.
//! top half (▀) = usage, bottom half (▄) = pace.
//! where both are filled: █ (fg = usage colour, bg = pace colour).
//! where only usage is filled: ▀ (fg = usage colour).
//! where only pace is filled: ▄ (fg = pace colour).
//! where neither: ░ (dim).

use ratatui::style::{Color, Style};
use ratatui::text::Span;

/// default bar width in characters
const BAR_WIDTH: usize = 20;

/// colour for the pace (bottom) layer
const PACE_COLOUR: Color = Color::DarkGray;

/// dim colour for empty cells
const EMPTY_COLOUR: Color = Color::DarkGray;

/// generate spans for a dual-layer usage bar
///
/// `usage_pct` and `pace_pct` are 0.0 - 100.0.
/// returns a vec of spans that can be inserted into a Line.
pub fn render_usage_bar(usage_pct: f32, pace_pct: f32) -> Vec<Span<'static>> {
    let usage_filled = pct_to_cells(usage_pct);
    let pace_filled = pct_to_cells(pace_pct);
    let usage_colour = colour_for_pct(usage_pct);

    let mut spans = Vec::with_capacity(BAR_WIDTH);

    for i in 0..BAR_WIDTH {
        let has_usage = i < usage_filled;
        let has_pace = i < pace_filled;

        let (ch, style) = match (has_usage, has_pace) {
            (true, true) => ("▀", Style::default().fg(usage_colour).bg(PACE_COLOUR)),
            (true, false) => ("▀", Style::default().fg(usage_colour)),
            (false, true) => ("▄", Style::default().fg(PACE_COLOUR)),
            (false, false) => ("░", Style::default().fg(EMPTY_COLOUR)),
        };

        spans.push(Span::styled(ch, style));
    }

    spans
}

fn pct_to_cells(pct: f32) -> usize {
    ((pct / 100.0 * BAR_WIDTH as f32).round() as usize).min(BAR_WIDTH)
}

/// usage colour: green < 50%, yellow 50-80%, red > 80%
fn colour_for_pct(pct: f32) -> Color {
    if pct >= 80.0 {
        Color::Red
    } else if pct >= 50.0 {
        Color::Yellow
    } else {
        Color::Cyan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_length_is_constant() {
        let spans = render_usage_bar(0.0, 0.0);
        assert_eq!(spans.len(), BAR_WIDTH);
        let spans = render_usage_bar(100.0, 100.0);
        assert_eq!(spans.len(), BAR_WIDTH);
        let spans = render_usage_bar(37.0, 50.0);
        assert_eq!(spans.len(), BAR_WIDTH);
    }

    #[test]
    fn empty_bar_all_dim() {
        let spans = render_usage_bar(0.0, 0.0);
        for span in &spans {
            assert_eq!(span.content.as_ref(), "░");
        }
    }

    #[test]
    fn full_bar_all_overlap() {
        let spans = render_usage_bar(100.0, 100.0);
        for span in &spans {
            assert_eq!(span.content.as_ref(), "▀");
            assert_eq!(span.style.bg, Some(PACE_COLOUR));
        }
    }

    #[test]
    fn usage_ahead_of_pace() {
        // 80% usage, 30% pace
        let spans = render_usage_bar(80.0, 30.0);
        // first 6 cells: both filled (▀ with bg)
        for span in &spans[..6] {
            assert_eq!(span.content.as_ref(), "▀");
            assert_eq!(span.style.bg, Some(PACE_COLOUR));
        }
        // cells 6-15: only usage (▀ without bg)
        for span in &spans[6..16] {
            assert_eq!(span.content.as_ref(), "▀");
            assert_eq!(span.style.bg, None);
        }
        // cells 16-19: empty
        for span in &spans[16..] {
            assert_eq!(span.content.as_ref(), "░");
        }
    }

    #[test]
    fn pace_ahead_of_usage() {
        // 26% usage, 43% pace
        let spans = render_usage_bar(26.0, 43.0);
        // first 5 cells: both (▀ with bg)
        for span in &spans[..5] {
            assert_eq!(span.content.as_ref(), "▀");
            assert_eq!(span.style.bg, Some(PACE_COLOUR));
        }
        // cells 5-8: only pace (▄)
        for span in &spans[5..9] {
            assert_eq!(span.content.as_ref(), "▄");
        }
        // rest: empty
        for span in &spans[9..] {
            assert_eq!(span.content.as_ref(), "░");
        }
    }

    #[test]
    fn colour_thresholds() {
        assert_eq!(colour_for_pct(0.0), Color::Cyan);
        assert_eq!(colour_for_pct(49.9), Color::Cyan);
        assert_eq!(colour_for_pct(50.0), Color::Yellow);
        assert_eq!(colour_for_pct(79.9), Color::Yellow);
        assert_eq!(colour_for_pct(80.0), Color::Red);
        assert_eq!(colour_for_pct(100.0), Color::Red);
    }

    #[test]
    fn pct_to_cells_rounds() {
        assert_eq!(pct_to_cells(0.0), 0);
        assert_eq!(pct_to_cells(5.0), 1); // 5% of 20 = 1
        assert_eq!(pct_to_cells(50.0), 10);
        assert_eq!(pct_to_cells(100.0), 20);
        assert_eq!(pct_to_cells(2.4), 0); // rounds to 0
        assert_eq!(pct_to_cells(2.5), 1); // rounds to 1
    }
}
