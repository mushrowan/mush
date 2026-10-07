use std::fmt::Write;

const MAX_PREVIEW_LINES: usize = 5;
const MAX_SINGLE_TOOL_LINES: usize = 5;
const MAX_PREVIEW_LINE_LEN: usize = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchSection {
    pub is_error: bool,
    pub content: String,
}

#[must_use]
pub fn parse_batch_output(text: &str) -> Vec<BatchSection> {
    let mut sections = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let Some(is_error) = section_header(lines[i], sections.len()) else {
            i += 1;
            continue;
        };
        i += 1;
        let mut content = String::new();
        while i < lines.len() {
            let next = lines[i];
            if section_header(next, sections.len() + 1).is_some() || is_summary_line(next) {
                break;
            }
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(next);
            i += 1;
        }
        sections.push(BatchSection {
            is_error,
            content: content.trim().to_string(),
        });
    }

    sections
}

fn section_header(line: &str, expected_index: usize) -> Option<bool> {
    let rest = line.strip_prefix("--- [")?;
    let (index, rest) = rest.split_once("] ")?;
    if index.parse::<usize>().ok()? != expected_index {
        return None;
    }
    if rest.ends_with(" [ok] ---") {
        Some(false)
    } else if rest.ends_with(" [error] ---") {
        Some(true)
    } else {
        None
    }
}

fn is_summary_line(line: &str) -> bool {
    line.strip_prefix("batch: ")
        .is_some_and(|rest| rest.contains(" succeeded, ") && rest.ends_with(" failed"))
}

#[must_use]
pub fn truncate_output(output: &str) -> String {
    truncate_output_with_cap(output, MAX_PREVIEW_LINES)
}

#[must_use]
pub fn truncate_output_large(output: &str) -> String {
    truncate_output_with_cap(output, MAX_SINGLE_TOOL_LINES)
}

fn truncate_output_with_cap(output: &str, max_lines: usize) -> String {
    let skipped = output.lines().count().saturating_sub(max_lines);
    let mut preview = String::new();
    if skipped > 0 {
        let _ = writeln!(preview, "… {skipped} earlier lines");
    }
    for (index, line) in output.lines().skip(skipped).enumerate() {
        if index > 0 {
            preview.push('\n');
        }
        if line.len() > MAX_PREVIEW_LINE_LEN {
            preview.push_str(&line[..line.floor_char_boundary(MAX_PREVIEW_LINE_LEN)]);
            preview.push('…');
        } else {
            preview.push_str(line);
        }
    }
    preview
}
