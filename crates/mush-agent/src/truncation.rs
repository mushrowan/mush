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
    Head,
    Tail,
    /// keep both head and tail with a marker in the middle (codex-style)
    #[default]
    Middle,
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

/// tools that handle their own output truncation and shouldn't be
/// double-truncated by the agent loop
pub fn self_truncating(tool_name: &str) -> bool {
    matches!(tool_name, "read" | "Read" | "batch")
}

/// truncate a tool result if it exceeds limits.
/// saves the full output to a file and tells the model where to find it.
pub fn truncate_tool_output(result: ToolResult) -> ToolResult {
    truncate_tool_output_with(result, MAX_LINES, MAX_BYTES, Direction::Middle)
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

    let total = lines.len();

    let truncated_text = match direction {
        Direction::Head => {
            let (kept, hit_bytes) = collect_head(&lines, max_lines, max_bytes);
            let omitted = total - kept.len();
            let preview = kept.join("\n");
            let hint = actionable_hint(&saved_path);
            if hit_bytes {
                format!(
                    "{preview}\n\n[…{omitted} lines truncated ({} total). {hint}]",
                    total
                )
            } else {
                format!("{preview}\n\n[…{omitted} lines truncated ({total} total). {hint}]")
            }
        }
        Direction::Tail => {
            let (kept, _hit_bytes) = collect_tail(&lines, max_lines, max_bytes);
            let omitted = total - kept.len();
            let preview = kept.join("\n");
            let hint = actionable_hint(&saved_path);
            format!("[{omitted} lines truncated… ({total} total). {hint}]\n\n{preview}")
        }
        Direction::Middle => {
            let half_lines = max_lines / 2;
            let half_bytes = max_bytes / 2;
            let (head, _) = collect_head(&lines, half_lines, half_bytes);
            let (tail, _) = collect_tail(&lines, half_lines, half_bytes);
            let omitted = total.saturating_sub(head.len() + tail.len());
            let head_text = head.join("\n");
            let tail_text = tail.join("\n");
            let hint = actionable_hint(&saved_path);
            format!(
                "{head_text}\n\n[…{omitted} lines truncated ({total} total). {hint}]\n\n{tail_text}"
            )
        }
    };

    // replace text parts with truncated version
    result
        .content
        .retain(|p| !matches!(p, ToolResultContentPart::Text(_)));
    result.content.insert(
        0,
        ToolResultContentPart::Text(mush_ai::types::TextContent {
            text: truncated_text,
        }),
    );

    result
}

/// collect lines from the start
fn collect_head<'a>(lines: &[&'a str], max_lines: usize, max_bytes: usize) -> (Vec<&'a str>, bool) {
    let mut kept = Vec::new();
    let mut bytes = 0;
    for line in lines {
        if kept.len() >= max_lines {
            break;
        }
        let size = line.len() + if kept.is_empty() { 0 } else { 1 };
        if bytes + size > max_bytes {
            return (kept, true);
        }
        kept.push(*line);
        bytes += size;
    }
    (kept, false)
}

/// collect lines from the end
fn collect_tail<'a>(lines: &[&'a str], max_lines: usize, max_bytes: usize) -> (Vec<&'a str>, bool) {
    let mut kept = Vec::new();
    let mut bytes = 0;
    for line in lines.iter().rev() {
        if kept.len() >= max_lines {
            break;
        }
        let size = line.len() + if kept.is_empty() { 0 } else { 1 };
        if bytes + size > max_bytes {
            kept.reverse();
            return (kept, true);
        }
        kept.push(*line);
        bytes += size;
    }
    kept.reverse();
    (kept, false)
}

/// actionable hint telling the model what to do with truncated output
fn actionable_hint(saved_path: &Option<PathBuf>) -> String {
    match saved_path {
        Some(path) => format!(
            "Use grep to search or read with offset/limit to view sections. Full output: {}",
            path.display()
        ),
        None => "Use grep to search or read with offset/limit to view sections.".into(),
    }
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
    fn truncates_by_line_count_middle_out() {
        let big = make_lines(3000);
        let out = truncate_tool_output(ToolResult::text(big));
        assert_text(&out, |t| {
            // head preserved
            assert!(t.contains("line 0"));
            // tail preserved
            assert!(t.contains("line 2999"));
            // middle-out marker
            assert!(t.contains("lines truncated"));
            assert!(t.contains("3000 total"));
        });
    }

    #[test]
    fn truncates_by_byte_count() {
        // single long line exceeding MAX_BYTES
        let big = "x".repeat(MAX_BYTES + 10000);
        let out = truncate_tool_output(ToolResult::text(big));
        assert_text(&out, |t| {
            assert!(t.contains("lines truncated"));
        });
    }

    #[test]
    fn head_keeps_start() {
        let lines = make_numbered_lines(10);
        let out =
            truncate_tool_output_with(ToolResult::text(lines), 3, usize::MAX, Direction::Head);
        assert_text(&out, |t| {
            assert!(t.contains("line 0"));
            assert!(t.contains("line 2"));
            assert!(!t.contains("line 9"));
            assert!(t.contains("7 lines truncated"));
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
            assert!(t.contains("7 lines truncated"));
        });
    }

    #[test]
    fn middle_keeps_head_and_tail() {
        let lines = make_numbered_lines(20);
        let out =
            truncate_tool_output_with(ToolResult::text(lines), 6, usize::MAX, Direction::Middle);
        assert_text(&out, |t| {
            // head (3 lines: 0, 1, 2)
            assert!(t.contains("line 0"));
            assert!(t.contains("line 2"));
            // tail (3 lines: 17, 18, 19)
            assert!(t.contains("line 17"));
            assert!(t.contains("line 19"));
            // middle omitted
            assert!(!t.contains("line 10"));
            assert!(t.contains("14 lines truncated"));
        });
    }

    #[test]
    fn error_results_also_truncated() {
        let big = make_lines(3000);
        let out = truncate_tool_output(ToolResult::error(big));
        assert!(out.outcome.is_error());
        assert_text(&out, |t| {
            assert!(t.contains("lines truncated"));
        });
    }

    #[test]
    fn saves_full_output_to_file() {
        let big = make_lines(3000);
        let out = truncate_tool_output(ToolResult::text(big));
        assert_text(&out, |t| {
            assert!(t.contains("Full output:"));
        });
    }

    #[test]
    fn includes_actionable_hint() {
        let big = make_lines(3000);
        let out = truncate_tool_output(ToolResult::text(big));
        assert_text(&out, |t| {
            assert!(t.contains("Use grep to search or read with offset/limit"));
        });
    }

    // helpers

    fn make_lines(n: usize) -> String {
        (0..n)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
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
