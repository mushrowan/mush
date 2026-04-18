//! diff parsing and rendering for tool output.
//!
//! the edit tool emits diffs with line prefixes: `+ ` for additions,
//! `- ` for removals, `  ` for context, `  ...` for omitted gaps. this
//! module parses that format into structured events and renders them as
//! either inline or side-by-side rows depending on available width.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// minimum inner panel width at which side-by-side rendering kicks in.
/// below this we fall back to inline (single-column) rendering
pub const SIDE_BY_SIDE_MIN_WIDTH: usize = 80;

/// ellipsis used to truncate long lines in side-by-side rendering.
/// single cell (1 column) per unicode-width
const ELLIPSIS: char = '…';

/// a single semantic event extracted from diff text
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffEvent {
    /// unchanged context line (shown with a leading `  ` in source)
    Context(String),
    /// removed line (was `- ` prefixed in source)
    Removed(String),
    /// added line (was `+ ` prefixed in source)
    Added(String),
    /// elided gap between change groups (was `  ...` in source)
    Gap,
}

/// parse diff text into a sequence of events.
/// each input line becomes one event; the leading `+ `/`- `/`  ` prefix
/// is consumed and the remaining content is stored verbatim
#[must_use]
pub fn parse_diff_lines(text: &str) -> Vec<DiffEvent> {
    text.lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix("+ ") {
                DiffEvent::Added(rest.to_string())
            } else if let Some(rest) = line.strip_prefix("- ") {
                DiffEvent::Removed(rest.to_string())
            } else if line.trim_start() == "..." {
                DiffEvent::Gap
            } else if let Some(rest) = line.strip_prefix("  ") {
                DiffEvent::Context(rest.to_string())
            } else {
                // unprefixed line: treat as context with original content
                DiffEvent::Context(line.to_string())
            }
        })
        .collect()
}

/// truncate `text` so its unicode display width is at most `max`,
/// appending `…` if truncation happened. returns the original string when
/// it already fits
fn truncate_display(text: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if UnicodeWidthStr::width(text) <= max {
        return text.to_string();
    }
    // need to truncate: reserve 1 col for the ellipsis
    let budget = max.saturating_sub(1);
    let mut width = 0usize;
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if width + cw > budget {
            break;
        }
        width += cw;
        out.push(ch);
    }
    out.push(ELLIPSIS);
    out
}

/// render a styled cell of fixed display width, padding with spaces
fn cell<'a>(text: String, width: usize, style: Style) -> Vec<Span<'a>> {
    use unicode_width::UnicodeWidthStr;
    let truncated = truncate_display(&text, width);
    let used = UnicodeWidthStr::width(truncated.as_str());
    let pad = width.saturating_sub(used);
    vec![Span::styled(truncated, style), Span::raw(" ".repeat(pad))]
}

/// pick the rendering mode based on inner width, then render to rows.
/// each returned line holds only the diff content for one row; the caller
/// wraps it in any outer box chrome (borders, indents)
#[must_use]
pub fn render_diff(text: &str, inner_width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let events = parse_diff_lines(text);
    if inner_width >= SIDE_BY_SIDE_MIN_WIDTH {
        render_side_by_side(&events, inner_width, theme)
    } else {
        render_inline(&events, inner_width, theme)
    }
}

/// inline (single-column) rendering: one event per row, prefix preserved
fn render_inline(events: &[DiffEvent], inner_width: usize, theme: &Theme) -> Vec<Line<'static>> {
    events
        .iter()
        .map(|event| {
            let (prefix, content, style) = match event {
                DiffEvent::Added(s) => ("+ ", s.as_str(), theme.diff_added),
                DiffEvent::Removed(s) => ("- ", s.as_str(), theme.diff_removed),
                DiffEvent::Context(s) => ("  ", s.as_str(), theme.dim),
                DiffEvent::Gap => ("  ", "...", theme.dim),
            };
            let full = format!("{prefix}{content}");
            Line::from(cell(full, inner_width, style))
        })
        .collect()
}

/// side-by-side rendering: two columns separated by `│`.
/// removed lines go on the left, added lines on the right, paired by
/// position within each change run. context lines duplicate across both
/// columns so the reader can orient themselves. gaps render as a single
/// `...` row spanning both sides
fn render_side_by_side(
    events: &[DiffEvent],
    inner_width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // " │ " separator takes 3 cols, split remainder evenly
    let content_total = inner_width.saturating_sub(3);
    let left_width = content_total / 2;
    let right_width = content_total - left_width;
    let sep_style = theme.dim;

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            DiffEvent::Context(s) => {
                let mut spans = cell(format!("  {s}"), left_width, theme.dim);
                spans.push(Span::styled(" │ ", sep_style));
                spans.extend(cell(format!("  {s}"), right_width, theme.dim));
                rows.push(Line::from(spans));
                i += 1;
            }
            DiffEvent::Gap => {
                let mut spans = cell("  ...".into(), left_width, theme.dim);
                spans.push(Span::styled(" │ ", sep_style));
                spans.extend(cell("  ...".into(), right_width, theme.dim));
                rows.push(Line::from(spans));
                i += 1;
            }
            DiffEvent::Removed(_) | DiffEvent::Added(_) => {
                // collect consecutive -/+ run and zip into paired rows
                let run_start = i;
                while i < events.len()
                    && matches!(events[i], DiffEvent::Removed(_) | DiffEvent::Added(_))
                {
                    i += 1;
                }
                let run = &events[run_start..i];
                let removed: Vec<&str> = run
                    .iter()
                    .filter_map(|e| match e {
                        DiffEvent::Removed(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .collect();
                let added: Vec<&str> = run
                    .iter()
                    .filter_map(|e| match e {
                        DiffEvent::Added(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .collect();
                let pair_count = removed.len().max(added.len());
                for row in 0..pair_count {
                    let left = removed
                        .get(row)
                        .map(|s| (format!("- {s}"), theme.diff_removed))
                        .unwrap_or_else(|| (String::new(), Style::default()));
                    let right = added
                        .get(row)
                        .map(|s| (format!("+ {s}"), theme.diff_added))
                        .unwrap_or_else(|| (String::new(), Style::default()));
                    let mut spans = cell(left.0, left_width, left.1);
                    spans.push(Span::styled(" │ ", sep_style));
                    spans.extend(cell(right.0, right_width, right.1));
                    rows.push(Line::from(spans));
                }
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_identifies_addition_and_removal() {
        let text = "- old line\n+ new line\n";
        let events = parse_diff_lines(text);
        assert_eq!(
            events,
            vec![
                DiffEvent::Removed("old line".into()),
                DiffEvent::Added("new line".into()),
            ]
        );
    }

    #[test]
    fn parse_identifies_context_and_gap() {
        let text = "  kept line\n  ...\n";
        let events = parse_diff_lines(text);
        assert_eq!(
            events,
            vec![DiffEvent::Context("kept line".into()), DiffEvent::Gap]
        );
    }

    #[test]
    fn parse_unprefixed_lines_treated_as_context() {
        let text = "bare line\n";
        let events = parse_diff_lines(text);
        assert_eq!(events, vec![DiffEvent::Context("bare line".into())]);
    }

    #[test]
    fn truncate_display_fits_when_short() {
        assert_eq!(truncate_display("hello", 10), "hello");
    }

    #[test]
    fn truncate_display_returns_text_when_width_exactly_matches() {
        // 3 display cols, max 3: should return as-is with no ellipsis
        assert_eq!(truncate_display("abc", 3), "abc");
    }

    #[test]
    fn truncate_display_uses_ellipsis_when_too_long() {
        let result = truncate_display("hello world", 6);
        // max 6 means 5 content + 1 ellipsis cell
        assert!(result.ends_with(ELLIPSIS));
        assert!(result.chars().count() <= 6);
    }

    #[test]
    fn truncate_display_counts_unicode_width() {
        // `│` is 1 col wide
        let result = truncate_display("a│b│c│d", 4);
        assert!(result.ends_with(ELLIPSIS));
    }

    #[test]
    fn render_inline_below_threshold() {
        let theme = Theme::dark();
        let rows = render_diff("- old\n+ new\n", 40, &theme);
        assert_eq!(rows.len(), 2, "inline: one row per event");
    }

    #[test]
    fn render_side_by_side_above_threshold() {
        let theme = Theme::dark();
        // one removed + one added → one paired row
        let rows = render_diff("- old\n+ new\n", 100, &theme);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn side_by_side_pads_uneven_runs() {
        let theme = Theme::dark();
        // 2 removed, 1 added → 2 rows (second row has empty right side)
        let rows = render_diff("- old 1\n- old 2\n+ new\n", 100, &theme);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn side_by_side_context_appears_on_both_columns() {
        let theme = Theme::dark();
        let rows = render_diff("  context line\n", 100, &theme);
        assert_eq!(rows.len(), 1);
        // row should contain the context text twice (once each column)
        let rendered: String = rows[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("");
        let occurrences = rendered.matches("context line").count();
        assert_eq!(occurrences, 2, "context should appear on both sides");
    }

    #[test]
    fn side_by_side_separator_uses_vertical_bar() {
        let theme = Theme::dark();
        let rows = render_diff("- old\n+ new\n", 100, &theme);
        let rendered: String = rows[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("");
        assert!(rendered.contains(" │ "), "expected vertical bar separator");
    }

    #[test]
    fn side_by_side_truncates_long_lines() {
        let theme = Theme::dark();
        let long = "a".repeat(200);
        let input = format!("- {long}\n+ {long}\n");
        let rows = render_diff(&input, 100, &theme);
        // at inner width 100, each column gets ~48 cols. rendered row should
        // not exceed the budget: left + " │ " + right == 100
        let rendered: String = rows[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("");
        assert!(
            rendered.contains(ELLIPSIS),
            "expected ellipsis on truncated line"
        );
    }

    #[test]
    fn side_by_side_realistic_hunk_mixes_context_and_changes() {
        // a realistic edit: context line, then 2 removals + 3 additions, then context
        let theme = Theme::dark();
        let input = "  fn main() {\n- println!(\"hello\");\n- println!(\"extra\");\n+ println!(\"world\");\n+ println!(\"a\");\n+ println!(\"b\");\n  }\n";
        let rows = render_diff(input, 120, &theme);
        // expected rows: fn main() context, 3 paired rows (max 2,3), closing brace context
        assert_eq!(rows.len(), 5);
        // first row is context (both sides show fn main)
        let first: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(first.matches("fn main").count(), 2);
        // last row is closing brace context
        let last: String = rows[4].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(last.contains('}'));
    }
}
