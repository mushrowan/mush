//! helpers for parsing and previewing batch tool output

/// max lines for batch sub-call previews where multiple tools compete
/// for vertical space. batches render several panels in a row so each
/// gets a tight budget
const MAX_PREVIEW_LINES: usize = 12;
/// max lines for single-tool previews. single tools own the full message
/// width and have plenty of vertical room, so the old 12-line cap
/// truncated edit diffs even when there was no reason to. 40 lines shows
/// a useful diff hunk while still bounding very large tool outputs
const MAX_SINGLE_TOOL_LINES: usize = 40;
/// max chars per preview line
const MAX_PREVIEW_LINE_LEN: usize = 120;

/// parsed section from batch tool output
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchSection {
    pub is_error: bool,
    pub content: String,
}

/// parse batch output into per-sub-call sections
///
/// format: `--- [N] ToolName [ok|error] ---\ncontent\n\n`
#[must_use]
pub fn parse_batch_output(text: &str) -> Vec<BatchSection> {
    let mut sections = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        // match header: "--- [N] ToolName [ok|error] ---"
        if line.starts_with("--- [") && line.ends_with("] ---") {
            let is_error = line.contains("[error]");
            i += 1;
            // collect content until next header or summary line
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

/// preview builder for single-tool (non-batch) outputs. uses the larger
/// `MAX_SINGLE_TOOL_LINES` cap so edit diffs and read previews keep more
/// content visible when they own the full message width
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
