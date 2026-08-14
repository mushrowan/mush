const MAX_PREVIEW_LINES: usize = 12;
const MAX_SINGLE_TOOL_LINES: usize = 40;
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
        let line = lines[i];
        if line.starts_with("--- [") && line.ends_with("] ---") {
            let is_error = line.contains("[error]");
            i += 1;
            let mut content = String::new();
            while i < lines.len() {
                let next = lines[i];
                if (next.starts_with("--- [") && next.ends_with("] ---"))
                    || next.starts_with("batch: ")
                {
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
        } else {
            i += 1;
        }
    }

    sections
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
    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();
    let preview: Vec<String> = lines
        .into_iter()
        .take(max_lines)
        .map(|l| {
            if l.len() > MAX_PREVIEW_LINE_LEN {
                let end = l.floor_char_boundary(MAX_PREVIEW_LINE_LEN);
                format!("{}…", &l[..end])
            } else {
                l.to_string()
            }
        })
        .collect();
    let mut result = preview.join("\n");
    if total > max_lines {
        result.push_str(&format!("\n… ({} more lines)", total - max_lines));
    }
    result
}
