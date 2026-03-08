//! tool output truncation with file spillover
//!
//! when a tool result exceeds the max size, the full output is saved to a file
//! and the model gets a truncated preview with instructions to use grep/read.
//! applied as a post-execute step in the agent loop so individual tools don't
//! need to handle truncation themselves.

use std::path::PathBuf;

use crate::tool::ToolResult;
use mush_ai::types::ToolResultContentPart;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

const RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// which end to keep when truncating
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Head,
    Tail,
}

/// directory for saving full tool output
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

/// save full output to a file, returning the path.
/// falls back to temp dir if the preferred location isn't writable
fn save_full_output(content: &str) -> Option<PathBuf> {
    let primary = output_dir();
    let fallback = std::env::temp_dir().join("mush").join("tool-output");

    let dir = if std::fs::create_dir_all(&primary).is_ok() {
        primary
    } else {
        std::fs::create_dir_all(&fallback).ok()?;
        fallback
    };

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = dir.join(format!("tool_{ts}.txt"));
    std::fs::write(&path, content).ok()?;
    Some(path)
}

/// clean up tool output files older than 7 days
pub fn cleanup() {
    let dir = output_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let cutoff = std::time::SystemTime::now() - RETENTION;
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && modified < cutoff
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// truncate a tool result if it exceeds limits.
/// saves the full output to a file and tells the model where to find it.
pub fn truncate_tool_output(result: ToolResult) -> ToolResult {
    truncate_tool_output_with(result, MAX_LINES, MAX_BYTES, Direction::Head)
}

/// truncate with explicit options
pub fn truncate_tool_output_with(
    mut result: ToolResult,
    max_lines: usize,
    max_bytes: usize,
    direction: Direction,
) -> ToolResult {
    let full_text: String = result
        .content
        .iter()
        .filter_map(|p| match p {
            ToolResultContentPart::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    let lines: Vec<&str> = full_text.lines().collect();
    let total_bytes = full_text.len();

    if lines.len() <= max_lines && total_bytes <= max_bytes {
        return result;
    }

    let saved_path = save_full_output(&full_text);

    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0;
    let mut hit_bytes = false;

    match direction {
        Direction::Head => {
            for line in &lines {
                if kept.len() >= max_lines {
                    break;
                }
                let size = line.len() + if kept.is_empty() { 0 } else { 1 };
                if bytes + size > max_bytes {
                    hit_bytes = true;
                    break;
                }
                kept.push(line);
                bytes += size;
            }
        }
        Direction::Tail => {
            for line in lines.iter().rev() {
                if kept.len() >= max_lines {
                    break;
                }
                let size = line.len() + if kept.is_empty() { 0 } else { 1 };
                if bytes + size > max_bytes {
                    hit_bytes = true;
                    break;
                }
                kept.insert(0, line);
                bytes += size;
            }
        }
    }

    let removed = if hit_bytes {
        total_bytes - bytes
    } else {
        lines.len() - kept.len()
    };
    let unit = if hit_bytes { "bytes" } else { "lines" };
    let preview = kept.join("\n");

    let hint = match &saved_path {
        Some(path) => format!(
            "output truncated, full content saved to: {}\nuse grep to narrow first\nuse read with start_line/end_line for exact spans",
            path.display()
        ),
        None => "output truncated. use grep to narrow results or read with offset/limit".to_string(),
    };

    let truncated = match direction {
        Direction::Head => format!("{preview}\n\n...{removed} {unit} truncated...\n\n{hint}"),
        Direction::Tail => format!("...{removed} {unit} truncated...\n\n{hint}\n\n{preview}"),
    };

    // replace text parts with truncated version
    result
        .content
        .retain(|p| !matches!(p, ToolResultContentPart::Text(_)));
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
        assert_text(&out, |t| {
            assert_eq!(t, "hello world");
        });
    }

    #[test]
    fn truncates_by_line_count() {
        let big = make_lines(3000);
        let out = truncate_tool_output(ToolResult::text(big));
        assert_text(&out, |t| {
            assert!(t.contains("line 0"));
            assert!(t.contains("...1000 lines truncated..."));
            assert!(!t.contains("line 2999"));
        });
    }

    #[test]
    fn truncates_by_byte_count() {
        // single long line exceeding MAX_BYTES
        let big = "x".repeat(MAX_BYTES + 10000);
        let out = truncate_tool_output(ToolResult::text(big));
        assert_text(&out, |t| {
            assert!(t.contains("bytes truncated"));
        });
    }

    #[test]
    fn head_keeps_start() {
        let lines = make_numbered_lines(10);
        let out = truncate_tool_output_with(ToolResult::text(lines), 3, usize::MAX, Direction::Head);
        assert_text(&out, |t| {
            assert!(t.contains("line 0"));
            assert!(t.contains("line 2"));
            assert!(!t.contains("line 9"));
            assert!(t.contains("...7 lines truncated..."));
        });
    }

    #[test]
    fn tail_keeps_end() {
        let lines = make_numbered_lines(10);
        let out =
            truncate_tool_output_with(ToolResult::text(lines), 3, usize::MAX, Direction::Tail);
        assert_text(&out, |t| {
            assert!(!t.contains("\nline 0\n"));
            assert!(t.contains("line 7"));
            assert!(t.contains("line 9"));
            assert!(t.contains("...7 lines truncated..."));
        });
    }

    #[test]
    fn error_results_also_truncated() {
        let big = make_lines(3000);
        let out = truncate_tool_output(ToolResult::error(big));
        assert!(out.outcome.is_error());
        assert_text(&out, |t| {
            assert!(t.contains("truncated"));
        });
    }

    #[test]
    fn saves_full_output_to_file() {
        let big = make_lines(3000);
        let out = truncate_tool_output(ToolResult::text(big));
        assert_text(&out, |t| {
            assert!(t.contains("full content saved to:"));
            assert!(t.contains("grep/read"));
        });
    }

    // helpers

    fn make_lines(n: usize) -> String {
        (0..n).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")
    }

    fn make_numbered_lines(n: usize) -> String {
        make_lines(n)
    }

    fn assert_text(result: &ToolResult, f: impl FnOnce(&str)) {
        match &result.content[0] {
            ToolResultContentPart::Text(t) => f(&t.text),
            _ => panic!("expected text"),
        }
    }
}
