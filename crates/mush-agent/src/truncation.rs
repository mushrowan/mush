//! tool output truncation with file spillover
//!
//! when a tool result exceeds the max size, the full output is saved to a file
//! and the model is told to use grep/read on it instead of re-running the tool.
//! applied as a post-execute step in execute_tool, so individual tools don't
//! need to handle truncation themselves.

use std::path::PathBuf;

use crate::tool::ToolResult;
use mush_ai::types::ToolResultContentPart;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

/// directory for saving full tool output when truncation occurs
fn output_dir() -> PathBuf {
    let base = if let Ok(dir) = std::env::var("MUSH_DATA_DIR") {
        PathBuf::from(dir)
    } else if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(data).join("mush")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/share/mush")
    } else {
        PathBuf::from("/tmp/mush")
    };
    base.join("tool-output")
}

/// save full output to a file, returning the path
fn save_full_output(content: &str) -> Option<PathBuf> {
    let dir = output_dir();
    std::fs::create_dir_all(&dir).ok()?;

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = dir.join(format!("tool_{ts}.txt"));
    std::fs::write(&path, content).ok()?;
    Some(path)
}

/// clean up tool output files older than 24h
pub fn cleanup() {
    let dir = output_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60);
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && modified < cutoff
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// truncate a tool result if it exceeds MAX_LINES or MAX_BYTES.
/// saves the full output to a file and tells the model where to find it.
/// tools that already handle truncation (e.g. read with user-specified limit)
/// should be skipped by the caller.
pub fn truncate_tool_output(mut result: ToolResult) -> ToolResult {
    // only truncate text parts
    let text_len: usize = result
        .content
        .iter()
        .filter_map(|p| match p {
            ToolResultContentPart::Text(t) => Some(t.text.len()),
            _ => None,
        })
        .sum();

    let line_count: usize = result
        .content
        .iter()
        .filter_map(|p| match p {
            ToolResultContentPart::Text(t) => Some(t.text.lines().count()),
            _ => None,
        })
        .sum();

    if text_len <= MAX_BYTES && line_count <= MAX_LINES {
        return result;
    }

    // combine all text parts for saving
    let full_text: String = result
        .content
        .iter()
        .filter_map(|p| match p {
            ToolResultContentPart::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    let saved_path = save_full_output(&full_text);

    // truncate: keep head up to limits
    let lines: Vec<&str> = full_text.lines().collect();
    let mut truncated = String::new();
    let mut bytes = 0;
    let mut kept = 0;

    for line in &lines {
        if kept >= MAX_LINES || bytes + line.len() + 1 >= MAX_BYTES {
            break;
        }
        if !truncated.is_empty() {
            truncated.push('\n');
            bytes += 1;
        }
        truncated.push_str(line);
        bytes += line.len();
        kept += 1;
    }

    let remaining_lines = lines.len() - kept;
    let remaining_bytes = full_text.len() - bytes;

    let hint = if let Some(ref path) = saved_path {
        format!(
            "\n\n[output truncated: {remaining_lines} more lines, {remaining_bytes} more bytes. \
             full output saved to: {}\n\
             use grep to search it or read with offset/limit to view sections. \
             do NOT re-run the command.]",
            path.display()
        )
    } else {
        format!(
            "\n\n[output truncated: {remaining_lines} more lines, {remaining_bytes} more bytes. \
             use more targeted commands to get the output you need.]"
        )
    };

    truncated.push_str(&hint);

    // replace text parts with truncated version
    result.content.retain(|p| !matches!(p, ToolResultContentPart::Text(_)));
    result.content.insert(
        0,
        ToolResultContentPart::Text(mush_ai::types::TextContent {
            text: truncated,
        }),
    );

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_not_truncated() {
        let result = ToolResult::text("hello world");
        let out = truncate_tool_output(result);
        match &out.content[0] {
            ToolResultContentPart::Text(t) => {
                assert_eq!(t.text, "hello world");
                assert!(!t.text.contains("truncated"));
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn large_output_truncated() {
        let big: String = (0..3000).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let result = ToolResult::text(big);
        let out = truncate_tool_output(result);
        match &out.content[0] {
            ToolResultContentPart::Text(t) => {
                assert!(t.text.contains("truncated"));
                assert!(t.text.contains("line 0"));
                assert!(!t.text.contains("line 2999"));
                assert!(t.text.lines().count() < 2100);
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn error_results_also_truncated() {
        let big: String = (0..3000).map(|i| format!("error line {i}")).collect::<Vec<_>>().join("\n");
        let result = ToolResult::error(big);
        let out = truncate_tool_output(result);
        assert!(out.outcome.is_error());
        match &out.content[0] {
            ToolResultContentPart::Text(t) => {
                assert!(t.text.contains("truncated"));
            }
            _ => panic!("expected text"),
        }
    }
}
