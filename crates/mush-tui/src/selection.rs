use std::ops::RangeInclusive;

use crate::app::App;
use crate::screen::SelectionKind;
use crate::soft_wrap::Break;

pub(crate) fn text_for(
    lines: &[String],
    breaks: &[Option<Break>],
    range: &RangeInclusive<(usize, usize)>,
    kind: SelectionKind,
) -> String {
    let (start_line, start_column) = *range.start();
    let (end_line, end_column) = *range.end();
    if kind == SelectionKind::Block {
        let cells = |(line, column): (usize, usize)| {
            let offsets = crate::text::cell_offsets(lines.get(line).map_or("", String::as_str));
            let at = |column: usize| {
                offsets
                    .get(column)
                    .or(offsets.last())
                    .copied()
                    .unwrap_or(column)
            };
            at(column)..at(column + 1).max(at(column) + 1)
        };
        let (a, b) = (cells(*range.start()), cells(*range.end()));
        let (left, right) = (a.start.min(b.start), a.end.max(b.end));
        return lines
            .iter()
            .take(end_line + 1)
            .skip(start_line)
            .map(|line| within_cells(line, left..right))
            .collect::<Vec<_>>()
            .join("\n");
    }
    let pieces: Vec<(usize, String)> = if kind == SelectionKind::Line {
        (start_line..=end_line)
            .filter_map(|row| Some((row, lines.get(row)?.clone())))
            .collect()
    } else {
        let Some(line) = lines.get(start_line) else {
            return String::new();
        };
        if start_line == end_line {
            return graphemes_between(line, start_column, end_column + 1).to_string();
        }
        let mut pieces = vec![(
            start_line,
            graphemes_between(line, start_column, usize::MAX).to_string(),
        )];
        for row in start_line + 1..end_line {
            if let Some(line) = lines.get(row) {
                pieces.push((row, line.clone()));
            }
        }
        if let Some(line) = lines.get(end_line) {
            pieces.push((
                end_line,
                graphemes_between(line, 0, end_column + 1).to_string(),
            ));
        }
        pieces
    };
    join_rows(&pieces, breaks)
}

fn graphemes_between(line: &str, from: usize, to: usize) -> &str {
    let start = crate::text::byte_of_grapheme(line, from);
    let end = crate::text::byte_of_grapheme(line, to).max(start);
    &line[start..end]
}

fn within_cells(line: &str, cells: std::ops::Range<usize>) -> String {
    let mut at = 0;
    let mut out = String::new();
    for (_, grapheme) in crate::text::graphemes(line) {
        let width = crate::text::grapheme_width(grapheme);
        if at < cells.end && at + width.max(1) > cells.start {
            out.push_str(grapheme);
        }
        at += width;
    }
    out
}

fn join_rows(pieces: &[(usize, String)], breaks: &[Option<Break>]) -> String {
    let mut out = String::new();
    for (index, (row, piece)) in pieces.iter().enumerate() {
        if index == 0 {
            out.push_str(piece);
            continue;
        }
        match breaks.get(*row).copied().flatten() {
            Some(after) => {
                out.truncate(out.trim_end_matches(' ').len());
                out.push_str(after.joiner());
                out.push_str(piece.trim_start_matches(' '));
            }
            None => {
                out.push('\n');
                out.push_str(piece);
            }
        }
    }
    out
}

pub(crate) fn strip_padding(text: &str) -> String {
    let indent = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    text.lines()
        .map(|line| line.get(indent..).unwrap_or("").trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn resolve_log_search(
    app: &mut App,
    command: crate::normal::keys::Command,
    count: Option<usize>,
) -> Result<crate::normal::keys::Command, String> {
    use crate::motion::Motion;
    use crate::normal::keys::{Command, Search, Target};
    let motion = match command {
        Command::Move(motion) | Command::Operate(_, Target::Motion(motion)) => motion,
        command => return Ok(command),
    };
    let (pattern, forward) = match motion {
        Motion::Search { reverse } => {
            let search = app
                .interaction
                .normal
                .keys
                .last_search()
                .ok_or("no earlier search")?;
            (search.pattern.clone(), search.forward != reverse)
        }
        Motion::SearchWord { forward } => {
            let pattern = {
                use crate::normal::NormalTarget;
                let target = crate::normal::LogTarget::new(app);
                crate::motion::word_pattern(target.rows(), target.cursor())
            }
            .ok_or("no word under the cursor")?;
            app.interaction.normal.keys.remember_search(Search {
                pattern: pattern.clone(),
                forward,
            });
            (pattern, forward)
        }
        _ => return Ok(command),
    };
    let not_found = || format!("pattern not found: {pattern}");
    let regex = crate::motion::search_regex(&pattern).ok_or_else(not_found)?;
    let to =
        crate::widgets::message_list::transcript::search(app, &regex, forward, count.unwrap_or(1))
            .ok_or_else(not_found)?;
    let jump = Motion::Jump {
        to,
        linewise: false,
    };
    Ok(match command {
        Command::Operate(operator, _) => Command::Operate(operator, Target::Motion(jump)),
        _ => Command::Move(jump),
    })
}

pub(crate) fn selected_text(app: &App, strip_indent: bool) -> Option<String> {
    app.viewport.selection()?;
    let rows = crate::widgets::message_list::transcript::rows(app, |viewport| {
        viewport
            .selection()
            .map_or(0..0, |range| range.start().0..range.end().0 + 1)
    });
    let range = app.viewport.selection()?;
    let local = |(line, column): (usize, usize)| (line.saturating_sub(rows.base), column);
    let range = local(*range.start())..=local(*range.end());
    let text = text_for(
        &rows.rows,
        &rows.breaks,
        &range,
        app.viewport.selection_kind(),
    );
    Some(if strip_indent {
        strip_padding(&text)
    } else {
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_takes_whole_graphemes_by_column() {
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        let lines = vec![format!(" a{family}e\u{301}中b")];

        assert_eq!(
            text_for(&lines, &[], &((0, 2)..=(0, 4)), SelectionKind::Char),
            format!("{family}e\u{301}中")
        );
    }

    #[test]
    fn a_block_copy_takes_the_same_cells_from_wide_and_narrow_rows() {
        let lines = vec!["abcdef".to_string(), "中文中".to_string()];

        assert_eq!(
            text_for(&lines, &[], &((0, 2)..=(1, 1)), SelectionKind::Block),
            "cd\n文"
        );
    }

    #[test]
    fn a_blockwise_range_takes_the_same_columns_from_each_row() {
        let lines = vec![" abcdefg".to_string(), " uvwxyz".to_string()];

        assert_eq!(
            text_for(&lines, &[], &((0, 3)..=(1, 6)), SelectionKind::Block),
            "cdef\nwxyz"
        );
    }

    #[test]
    fn a_linewise_range_takes_whole_rows() {
        let lines = vec![
            "keep me out".to_string(),
            "   hello".to_string(),
            "     hello".to_string(),
        ];

        assert_eq!(
            text_for(&lines, &[], &((1, 4)..=(2, 4)), SelectionKind::Line),
            "   hello\n     hello"
        );
    }

    #[test]
    fn a_range_over_several_lines_runs_to_the_end_of_each() {
        let lines = vec![
            "keep me out".to_string(),
            "   hello".to_string(),
            "     hello".to_string(),
        ];

        assert_eq!(
            text_for(&lines, &[], &((1, 0)..=(2, 9)), SelectionKind::Char),
            "   hello\n     hello"
        );
    }

    #[test]
    fn soft_wrapped_rows_copy_as_one_line() {
        let lines = vec![
            " the quick brown".to_string(),
            " fox jumps".to_string(),
            " supercalifrag".to_string(),
            " ilistic".to_string(),
            " next paragraph".to_string(),
        ];
        let breaks = [
            None,
            Some(Break::AtSpace),
            Some(Break::AtSpace),
            Some(Break::MidWord),
            None,
        ];

        assert_eq!(
            text_for(&lines, &breaks, &((0, 5)..=(4, 4)), SelectionKind::Char),
            "quick brown fox jumps supercalifragilistic\n next"
        );
        assert_eq!(
            text_for(&lines, &breaks, &((0, 0)..=(1, 0)), SelectionKind::Line),
            " the quick brown fox jumps"
        );
    }

    #[test]
    fn trailing_padding_comes_off_every_row() {
        assert_eq!(strip_padding("  seq      \n  line 1   "), "seq\nline 1");
    }

    #[test]
    fn the_common_indent_comes_off_a_multi_line_copy() {
        assert_eq!(strip_padding("   hello\n     hello"), "hello\n  hello");
    }

    #[test]
    fn a_range_inside_one_line_takes_those_columns() {
        let lines = vec![" world".to_string()];

        assert_eq!(
            text_for(&lines, &[], &((0, 1)..=(0, 5)), SelectionKind::Char),
            "world"
        );
    }
}
