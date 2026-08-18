use std::ops::RangeInclusive;

use crate::app::App;

pub(crate) fn text_for(lines: &[String], range: &RangeInclusive<(usize, usize)>) -> String {
    let (start_line, start_column) = *range.start();
    let (end_line, end_column) = *range.end();
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
    String::new()
}

pub(crate) fn selected_text(app: &App) -> Option<String> {
    let range = app.viewport.selection()?;
    let lines = crate::widgets::message_list::transcript_guards(app, app.viewport.area().width)
        .flatten()
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    Some(text_for(&lines, &range))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_inside_one_line_takes_those_columns() {
        let lines = vec![" world".to_string()];

        assert_eq!(text_for(&lines, &((0, 1)..=(0, 5))), "world");
    }
}
