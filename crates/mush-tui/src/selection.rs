use std::ops::RangeInclusive;

use crate::app::App;
use crate::screen::SelectionKind;

pub(crate) fn text_for(
    lines: &[String],
    range: &RangeInclusive<(usize, usize)>,
    kind: SelectionKind,
) -> String {
    let (start_line, start_column) = *range.start();
    let (end_line, end_column) = *range.end();
    if kind == SelectionKind::Line {
        return lines
            .iter()
            .take(end_line + 1)
            .skip(start_line)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
    }
    let Some(line) = lines.get(start_line) else {
        return String::new();
    };
    if start_line == end_line {
        return line
            .chars()
            .skip(start_column)
            .take(end_column + 1 - start_column)
            .collect();
    }
    let mut out: Vec<String> = vec![line.chars().skip(start_column).collect()];
    for line in lines.iter().take(end_line).skip(start_line + 1) {
        out.push(line.clone());
    }
    if let Some(line) = lines.get(end_line) {
        out.push(line.chars().take(end_column + 1).collect());
    }
    out.join("\n")
}

pub(crate) fn strip_common_indent(text: &str) -> String {
    let indent = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    text.lines()
        .map(|line| line.get(indent..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn selected_text(app: &App, strip_indent: bool) -> Option<String> {
    let range = app.viewport.selection()?;
    let lines = crate::widgets::message_list::transcript_guards(app, app.viewport.area().width)
        .flatten()
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    let text = text_for(&lines, &range, app.viewport.selection_kind());
    Some(if strip_indent {
        strip_common_indent(&text)
    } else {
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linewise_range_takes_whole_rows() {
        let lines = vec![
            "keep me out".to_string(),
            "   hello".to_string(),
            "     hello".to_string(),
        ];

        assert_eq!(
            text_for(&lines, &((1, 4)..=(2, 4)), SelectionKind::Line),
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
            text_for(&lines, &((1, 0)..=(2, 9)), SelectionKind::Char),
            "   hello\n     hello"
        );
    }

    #[test]
    fn the_common_indent_comes_off_a_multi_line_copy() {
        assert_eq!(
            strip_common_indent("   hello\n     hello"),
            "hello\n  hello"
        );
    }

    #[test]
    fn a_range_inside_one_line_takes_those_columns() {
        let lines = vec![" world".to_string()];

        assert_eq!(
            text_for(&lines, &((0, 1)..=(0, 5)), SelectionKind::Char),
            "world"
        );
    }
}
