//! diff parsing and rendering for tool output.
//!
//! the edit tool emits diffs with line prefixes: `+ ` for additions,
//! `- ` for removals, `  ` for context, `  ...` for omitted gaps. this
//! module parses that format into structured events and renders them as
//! either inline or side-by-side rows depending on available width.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

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

/// fit a sequence of pre-styled spans to exactly `width` display columns,
/// truncating with `…` when the combined content exceeds the budget and
/// padding with spaces when short. preserves per-span styling on retained
/// content so intra-line highlights carry through
fn fit_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let total: usize = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if total <= width {
        let mut out = spans;
        let pad = width - total;
        if pad > 0 {
            out.push(Span::raw(" ".repeat(pad)));
        }
        return out;
    }
    // reserve 1 column for the ellipsis
    let budget = width.saturating_sub(1);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let span_width = UnicodeWidthStr::width(span.content.as_ref());
        if used + span_width <= budget {
            used += span_width;
            out.push(span);
            continue;
        }
        // partial: take chars up to remaining budget
        let remaining = budget.saturating_sub(used);
        let style = span.style;
        let mut taken = String::new();
        let mut taken_width = 0usize;
        for ch in span.content.chars() {
            let cw = ch.width().unwrap_or(0);
            if taken_width + cw > remaining {
                break;
            }
            taken_width += cw;
            taken.push(ch);
        }
        if !taken.is_empty() {
            out.push(Span::styled(taken, style));
        }
        break;
    }
    out.push(Span::raw(ELLIPSIS.to_string()));
    out
}

/// compute word-level intra-line highlights for a paired removed/added line.
///
/// returns `(removed_spans, added_spans)` where tokens that differ between
/// the two strings are styled with `theme.diff_removed_intra` /
/// `theme.diff_added_intra` and matching tokens use the base
/// `theme.diff_removed` / `theme.diff_added` styles. whitespace is kept
/// with the adjacent token
#[must_use]
pub fn paired_change_spans(
    removed: &str,
    added: &str,
    theme: &Theme,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let diff = TextDiff::from_words(removed, added);
    let mut removed_spans: Vec<Span<'static>> = Vec::new();
    let mut added_spans: Vec<Span<'static>> = Vec::new();
    for change in diff.iter_all_changes() {
        let text = change.value().to_string();
        if text.is_empty() {
            continue;
        }
        match change.tag() {
            ChangeTag::Equal => {
                removed_spans.push(Span::styled(text.clone(), theme.diff_removed));
                added_spans.push(Span::styled(text, theme.diff_added));
            }
            ChangeTag::Delete => {
                removed_spans.push(Span::styled(text, theme.diff_removed_intra));
            }
            ChangeTag::Insert => {
                added_spans.push(Span::styled(text, theme.diff_added_intra));
            }
        }
    }
    (removed_spans, added_spans)
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

/// collect the removed / added strings inside a contiguous change run.
/// caller has already identified that `events[start..end]` contains only
/// Removed / Added variants
fn split_run(run: &[DiffEvent]) -> (Vec<&str>, Vec<&str>) {
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for event in run {
        match event {
            DiffEvent::Removed(s) => removed.push(s.as_str()),
            DiffEvent::Added(s) => added.push(s.as_str()),
            _ => {}
        }
    }
    (removed, added)
}

/// build spans for one side of a paired diff row: prefix + styled content.
/// `removed` / `added` are the paired strings (always in that argument
/// order) and `side` selects which one to emit. when `side` is missing
/// its string (unpaired end of a run) an empty span list is returned
fn build_side(
    prefix: &str,
    removed: Option<&str>,
    added: Option<&str>,
    theme: &Theme,
    side: Side,
) -> Vec<Span<'static>> {
    let (own, base) = match side {
        Side::Removed => (removed, theme.diff_removed),
        Side::Added => (added, theme.diff_added),
    };
    let Some(own) = own else {
        return Vec::new();
    };
    let mut spans: Vec<Span<'static>> = vec![Span::styled(prefix.to_string(), base)];
    match (removed, added) {
        (Some(r), Some(a)) => {
            let (removed_spans, added_spans) = paired_change_spans(r, a, theme);
            spans.extend(match side {
                Side::Removed => removed_spans,
                Side::Added => added_spans,
            });
        }
        _ => {
            // unpaired end of run: no intra highlight, plain base style
            spans.push(Span::styled(own.to_string(), base));
        }
    }
    spans
}

#[derive(Copy, Clone)]
enum Side {
    Removed,
    Added,
}

/// inline (single-column) rendering: one event per row, prefix preserved.
/// consecutive removed/added pairs receive intra-line word highlights
fn render_inline(events: &[DiffEvent], inner_width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            DiffEvent::Context(s) => {
                rows.push(Line::from(cell(format!("  {s}"), inner_width, theme.dim)));
                i += 1;
            }
            DiffEvent::Gap => {
                rows.push(Line::from(cell("  ...".into(), inner_width, theme.dim)));
                i += 1;
            }
            DiffEvent::Removed(_) | DiffEvent::Added(_) => {
                let run_start = i;
                while i < events.len()
                    && matches!(events[i], DiffEvent::Removed(_) | DiffEvent::Added(_))
                {
                    i += 1;
                }
                let (removed, added) = split_run(&events[run_start..i]);
                // emit removed rows first, then added rows - matches the
                // source order where all - lines come before + lines in a run
                for (idx, removed_text) in removed.iter().enumerate() {
                    let spans = build_side(
                        "- ",
                        Some(removed_text),
                        added.get(idx).copied(),
                        theme,
                        Side::Removed,
                    );
                    rows.push(Line::from(fit_spans(spans, inner_width)));
                }
                for (idx, added_text) in added.iter().enumerate() {
                    let spans = build_side(
                        "+ ",
                        removed.get(idx).copied(),
                        Some(added_text),
                        theme,
                        Side::Added,
                    );
                    rows.push(Line::from(fit_spans(spans, inner_width)));
                }
            }
        }
    }
    rows
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
                let (removed, added) = split_run(&events[run_start..i]);
                let pair_count = removed.len().max(added.len());
                for row in 0..pair_count {
                    let left_spans = build_side(
                        "- ",
                        removed.get(row).copied(),
                        added.get(row).copied(),
                        theme,
                        Side::Removed,
                    );
                    let right_spans = build_side(
                        "+ ",
                        removed.get(row).copied(),
                        added.get(row).copied(),
                        theme,
                        Side::Added,
                    );
                    let mut spans = fit_spans(left_spans, left_width);
                    spans.push(Span::styled(" │ ", sep_style));
                    spans.extend(fit_spans(right_spans, right_width));
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

    // -- intra-line word-level highlighting --

    #[test]
    fn paired_change_spans_marks_changed_tokens() {
        let theme = Theme::dark();
        let (removed, added) = paired_change_spans("foo bar baz", "foo qux baz", &theme);

        // removed side should contain both unchanged "foo" / " baz" and the
        // changed "bar" at intra-highlight style
        let has_intra_bar = removed
            .iter()
            .any(|s| s.content.contains("bar") && s.style == theme.diff_removed_intra);
        assert!(
            has_intra_bar,
            "expected 'bar' at intra style, got {removed:?}"
        );

        let has_intra_qux = added
            .iter()
            .any(|s| s.content.contains("qux") && s.style == theme.diff_added_intra);
        assert!(
            has_intra_qux,
            "expected 'qux' at intra style, got {added:?}"
        );

        // common tokens on the removed side carry the base removed style
        let has_base_foo = removed
            .iter()
            .any(|s| s.content.contains("foo") && s.style == theme.diff_removed);
        assert!(
            has_base_foo,
            "expected 'foo' at base removed style, got {removed:?}"
        );
    }

    #[test]
    fn paired_change_spans_identical_inputs_have_no_intra_highlight() {
        let theme = Theme::dark();
        let (removed, added) = paired_change_spans("same text", "same text", &theme);
        assert!(
            removed
                .iter()
                .all(|s| s.style == theme.diff_removed || s.content.is_empty())
        );
        assert!(
            added
                .iter()
                .all(|s| s.style == theme.diff_added || s.content.is_empty())
        );
    }

    #[test]
    fn render_inline_applies_intra_highlight_to_paired_lines() {
        let theme = Theme::dark();
        let rows = render_diff("- foo bar baz\n+ foo qux baz\n", 40, &theme);
        assert_eq!(rows.len(), 2);

        // find the changed token on each side and confirm it carries intra style
        let removed_row = &rows[0];
        let has_bar_intra = removed_row
            .spans
            .iter()
            .any(|s| s.content.contains("bar") && s.style == theme.diff_removed_intra);
        assert!(
            has_bar_intra,
            "removed row missing intra-highlighted token, spans: {:?}",
            removed_row.spans
        );

        let added_row = &rows[1];
        let has_qux_intra = added_row
            .spans
            .iter()
            .any(|s| s.content.contains("qux") && s.style == theme.diff_added_intra);
        assert!(
            has_qux_intra,
            "added row missing intra-highlighted token, spans: {:?}",
            added_row.spans
        );
    }

    #[test]
    fn render_inline_leaves_unpaired_removed_without_intra_highlight() {
        // two removed, no added: nothing to diff against, should fall back to
        // plain removed style on all tokens
        let theme = Theme::dark();
        let rows = render_diff("- first line\n- second line\n", 40, &theme);
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert!(
                row.spans
                    .iter()
                    .all(|s| s.style != theme.diff_removed_intra),
                "unpaired removed should not have intra style, got {:?}",
                row.spans
            );
        }
    }
}
