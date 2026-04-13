//! tool output truncation with file spillover
//!
//! when a tool result exceeds the max size, the full output is saved to a file
//! and the model gets a truncated preview with instructions to use grep/read.
//! each tool declares its own `OutputLimit` via the trait method, so the agent
//! loop picks the right strategy automatically.

use std::path::PathBuf;

use crate::tool::{OutputLimit, ToolResult};
use mush_ai::types::ToolResultContentPart;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

const RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

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

/// save full batch output to a file (cap at 10MB to avoid runaway writes)
pub fn save_batch_output(content: &str) -> Option<PathBuf> {
    const MAX_SAVE_BYTES: usize = 10 * 1024 * 1024;
    if content.len() > MAX_SAVE_BYTES {
        // truncate at a line boundary
        let truncated = match content[..MAX_SAVE_BYTES].rfind('\n') {
            Some(pos) => &content[..pos],
            None => &content[..MAX_SAVE_BYTES],
        };
        let with_notice = format!(
            "{truncated}\n\n[...truncated at {MAX_SAVE_BYTES} byte cap, {} bytes omitted]",
            content.len() - truncated.len()
        );
        save_full_output(&with_notice)
    } else {
        save_full_output(content)
    }
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

/// apply truncation based on the tool's declared output limit.
/// returns the result unchanged for `SelfManaged` tools.
pub fn apply(result: ToolResult, limit: OutputLimit) -> ToolResult {
    match limit {
        OutputLimit::SelfManaged => result,
        OutputLimit::Head => truncate(result, MAX_LINES, MAX_BYTES, OutputLimit::Head),
        OutputLimit::Tail => truncate(result, MAX_LINES, MAX_BYTES, OutputLimit::Tail),
        OutputLimit::Middle => truncate(result, MAX_LINES, MAX_BYTES, OutputLimit::Middle),
    }
}

/// truncate with explicit options
pub fn truncate(
    mut result: ToolResult,
    max_lines: usize,
    max_bytes: usize,
    direction: OutputLimit,
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
        tracing::debug!(
            lines = lines.len(),
            bytes = total_bytes,
            direction = ?direction,
            "output within limits, no truncation"
        );
        return result;
    }

    tracing::info!(
        lines = lines.len(),
        bytes = total_bytes,
        max_lines,
        max_bytes,
        direction = ?direction,
        "truncating tool output"
    );

    let saved_path = save_full_output(&full_text);

    let total = lines.len();

    let truncated_text = match direction {
        OutputLimit::Head | OutputLimit::SelfManaged => {
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
        OutputLimit::Tail => {
            let (kept, _hit_bytes) = collect_tail(&lines, max_lines, max_bytes);
            let omitted = total - kept.len();
            let preview = kept.join("\n");
            let hint = actionable_hint(&saved_path);
            format!("[{omitted} lines truncated… ({total} total). {hint}]\n\n{preview}")
        }
        OutputLimit::Middle => {
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
            "Use the grep tool to search or the read tool with offset/limit to view sections (do not use bash cat). Full output: {}",
            path.display()
        ),
        None => "Use the grep tool to search or the read tool with offset/limit to view sections (do not use bash cat).".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_not_truncated() {
        let result = ToolResult::text("hello world");
        let out = apply(result, OutputLimit::Middle);
        assert_text(&out, |t| {
            assert_eq!(t, "hello world");
        });
    }

    #[test]
    fn self_managed_passes_through() {
        let big = make_lines(3000);
        let out = apply(ToolResult::text(big.clone()), OutputLimit::SelfManaged);
        assert_text(&out, |t| assert_eq!(t, big));
    }

    #[test]
    fn middle_truncates_by_line_count() {
        let big = make_lines(3000);
        let out = apply(ToolResult::text(big), OutputLimit::Middle);
        assert_text(&out, |t| {
            assert!(t.contains("line 0"));
            assert!(t.contains("line 2999"));
            assert!(t.contains("lines truncated"));
            assert!(t.contains("3000 total"));
        });
    }

    #[test]
    fn truncates_by_byte_count() {
        let big = "x".repeat(MAX_BYTES + 10000);
        let out = apply(ToolResult::text(big), OutputLimit::Middle);
        assert_text(&out, |t| {
            assert!(t.contains("lines truncated"));
        });
    }

    #[test]
    fn head_keeps_start() {
        let lines = make_lines(10);
        let out = truncate(ToolResult::text(lines), 3, usize::MAX, OutputLimit::Head);
        assert_text(&out, |t| {
            assert!(t.contains("line 0"));
            assert!(t.contains("line 2"));
            assert!(!t.contains("line 9"));
            assert!(t.contains("7 lines truncated"));
        });
    }

    #[test]
    fn tail_keeps_end() {
        let lines = make_lines(10);
        let out = truncate(ToolResult::text(lines), 3, usize::MAX, OutputLimit::Tail);
        assert_text(&out, |t| {
            assert!(!t.contains("\nline 0\n"));
            assert!(t.contains("line 7"));
            assert!(t.contains("line 9"));
            assert!(t.contains("7 lines truncated"));
        });
    }

    #[test]
    fn middle_keeps_head_and_tail() {
        let lines = make_lines(20);
        let out = truncate(ToolResult::text(lines), 6, usize::MAX, OutputLimit::Middle);
        assert_text(&out, |t| {
            assert!(t.contains("line 0"));
            assert!(t.contains("line 2"));
            assert!(t.contains("line 17"));
            assert!(t.contains("line 19"));
            assert!(!t.contains("line 10"));
            assert!(t.contains("14 lines truncated"));
        });
    }

    #[test]
    fn error_results_also_truncated() {
        let big = make_lines(3000);
        let out = apply(ToolResult::error(big), OutputLimit::Middle);
        assert!(out.outcome.is_error());
        assert_text(&out, |t| {
            assert!(t.contains("lines truncated"));
        });
    }

    // the hint text must always be complete and actionable in the truncated
    // output. if the hint gets mangled or cut, agents loop forever trying to
    // bash cat the saved file (which re-truncates, saving to another file, etc)

    const EXPECTED_HINT_FRAGMENTS: &[&str] = &[
        "Use the grep tool to search",
        "the read tool with offset/limit",
        "do not use bash cat",
    ];

    #[test]
    fn hint_survives_head_truncation() {
        let big = make_lines(5000);
        let out = truncate(ToolResult::text(big), 100, usize::MAX, OutputLimit::Head);
        assert_text(&out, |t| {
            for frag in EXPECTED_HINT_FRAGMENTS {
                assert!(
                    t.contains(frag),
                    "head truncation missing hint fragment: {frag}"
                );
            }
        });
    }

    #[test]
    fn hint_survives_tail_truncation() {
        let big = make_lines(5000);
        let out = truncate(ToolResult::text(big), 100, usize::MAX, OutputLimit::Tail);
        assert_text(&out, |t| {
            for frag in EXPECTED_HINT_FRAGMENTS {
                assert!(
                    t.contains(frag),
                    "tail truncation missing hint fragment: {frag}"
                );
            }
        });
    }

    #[test]
    fn hint_survives_middle_truncation() {
        let big = make_lines(5000);
        let out = truncate(ToolResult::text(big), 100, usize::MAX, OutputLimit::Middle);
        assert_text(&out, |t| {
            for frag in EXPECTED_HINT_FRAGMENTS {
                assert!(
                    t.contains(frag),
                    "middle truncation missing hint fragment: {frag}"
                );
            }
        });
    }

    #[test]
    fn hint_survives_extreme_truncation() {
        // even with a budget of 1 line, the hint must be complete
        let big = make_lines(5000);
        for direction in [OutputLimit::Head, OutputLimit::Tail, OutputLimit::Middle] {
            let out = truncate(ToolResult::text(big.clone()), 1, usize::MAX, direction);
            assert_text(&out, |t| {
                for frag in EXPECTED_HINT_FRAGMENTS {
                    assert!(
                        t.contains(frag),
                        "extreme {direction:?} truncation missing hint fragment: {frag}"
                    );
                }
            });
        }
    }

    #[test]
    fn hint_includes_file_path_when_saved() {
        let big = make_lines(5000);
        let out = apply(ToolResult::text(big), OutputLimit::Tail);
        assert_text(&out, |t| {
            assert!(
                t.contains("Full output:"),
                "missing 'Full output:' with file path"
            );
            // path should end in .txt and contain tool-output
            assert!(
                t.contains("tool-output/") && t.contains(".txt"),
                "file path doesn't look right: {t}"
            );
        });
    }

    #[test]
    fn truncated_output_has_line_count_and_total() {
        let big = make_lines(5000);
        for direction in [OutputLimit::Head, OutputLimit::Tail, OutputLimit::Middle] {
            let out = truncate(ToolResult::text(big.clone()), 100, usize::MAX, direction);
            assert_text(&out, |t| {
                assert!(
                    t.contains("lines truncated"),
                    "{direction:?} missing 'lines truncated'"
                );
                assert!(
                    t.contains("5000 total"),
                    "{direction:?} missing total line count"
                );
            });
        }
    }

    // helpers

    fn make_lines(n: usize) -> String {
        (0..n)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_text(result: &ToolResult, f: impl FnOnce(&str)) {
        match &result.content[0] {
            ToolResultContentPart::Text(t) => f(&t.text),
            _ => panic!("expected text"),
        }
    }
}
