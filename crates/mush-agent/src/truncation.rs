use std::path::PathBuf;

use crate::tool::{OutputLimit, ToolResult};
use mush_ai::dirs::saved_output_name;
use mush_ai::types::ToolResultContentPart;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

const RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

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

#[must_use]
pub fn saved_output_here(path: &std::path::Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    if path.exists() || path.parent()?.file_name()? != "tool-output" || !saved_output_name(name) {
        return None;
    }
    let here = output_dir().join(name);
    here.is_file().then_some(here)
}

fn save_full_output(content: &str) -> Option<PathBuf> {
    save_to_output_dir("tool", content)
}

pub fn save_rejected_content(content: &str) -> Option<PathBuf> {
    save_to_output_dir("rejected", content)
}

fn save_to_output_dir(prefix: &str, content: &str) -> Option<PathBuf> {
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
    let path = dir.join(format!("{prefix}_{ts}.txt"));
    mush_ai::private_io::write_private(&path, content).ok()?;
    Some(path)
}

pub fn truncate_to_budget(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let budget = text.floor_char_boundary(budget);
    match text[..budget].rfind('\n') {
        Some(pos) => &text[..pos],
        None => &text[..budget],
    }
}

pub fn save_batch_output(content: &str) -> Option<PathBuf> {
    const MAX_SAVE_BYTES: usize = 10 * 1024 * 1024;
    if content.len() > MAX_SAVE_BYTES {
        let truncated = truncate_to_budget(content, MAX_SAVE_BYTES);
        let with_notice = format!(
            "{truncated}\n\n[...truncated at {MAX_SAVE_BYTES} byte cap, {} bytes omitted]",
            content.len() - truncated.len()
        );
        save_full_output(&with_notice)
    } else {
        save_full_output(content)
    }
}

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

pub fn apply(result: ToolResult, limit: OutputLimit) -> ToolResult {
    apply_for_tool(result, limit, None)
}

pub fn apply_for_tool(
    mut result: ToolResult,
    limit: OutputLimit,
    tool_name: Option<&str>,
) -> ToolResult {
    if let Some(details) = result.details.as_mut() {
        mush_ai::types::cap_detail_text(details, mush_ai::types::DETAIL_TEXT_LIMIT);
    }
    truncate_for_tool(result, MAX_LINES, MAX_BYTES, limit, tool_name)
}

pub fn truncate(
    result: ToolResult,
    max_lines: usize,
    max_bytes: usize,
    direction: OutputLimit,
) -> ToolResult {
    truncate_for_tool(result, max_lines, max_bytes, direction, None)
}

fn truncate_for_tool(
    mut result: ToolResult,
    max_lines: usize,
    max_bytes: usize,
    direction: OutputLimit,
    tool_name: Option<&str>,
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
        OutputLimit::Head => {
            let kept = collect_head(&lines, max_lines, max_bytes);
            let omitted = total - kept.len();
            let preview = if kept.is_empty() && !lines.is_empty() {
                let sample = sample_line_head(lines[0], max_bytes);
                format!("{sample}\n[partial line]")
            } else {
                kept.join("\n")
            };
            let hint = actionable_hint(&saved_path, tool_name);
            format!("[…{omitted} lines truncated ({total} total). {hint}]\n\n{preview}")
        }
        OutputLimit::Tail => {
            let kept = collect_tail(&lines, max_lines, max_bytes);
            let omitted = total - kept.len();
            let preview = if kept.is_empty() && !lines.is_empty() {
                let sample = sample_line_tail(lines[lines.len() - 1], max_bytes);
                format!("[partial line]\n{sample}")
            } else {
                kept.join("\n")
            };
            let hint = actionable_hint(&saved_path, tool_name);
            format!("[{omitted} lines truncated… ({total} total). {hint}]\n\n{preview}")
        }
    };

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

fn collect_head<'a>(lines: &[&'a str], max_lines: usize, max_bytes: usize) -> Vec<&'a str> {
    collect_within(lines.iter(), max_lines, max_bytes)
}

fn collect_tail<'a>(lines: &[&'a str], max_lines: usize, max_bytes: usize) -> Vec<&'a str> {
    let mut kept = collect_within(lines.iter().rev(), max_lines, max_bytes);
    kept.reverse();
    kept
}

fn collect_within<'a, 'b>(
    lines: impl Iterator<Item = &'b &'a str>,
    max_lines: usize,
    max_bytes: usize,
) -> Vec<&'a str>
where
    'a: 'b,
{
    let mut kept = Vec::new();
    let mut bytes = 0;
    for line in lines.take(max_lines) {
        let size = line.len() + usize::from(!kept.is_empty());
        if bytes + size > max_bytes {
            break;
        }
        kept.push(*line);
        bytes += size;
    }
    kept
}

fn actionable_hint(saved_path: &Option<PathBuf>, tool_name: Option<&str>) -> String {
    if tool_name == Some("read") {
        return match saved_path {
            Some(path) => format!(
                "Use the Read tool with offset/limit to view more of the original file. Full output: {}",
                path.display()
            ),
            None => "Use the Read tool with offset/limit to view more of the original file.".into(),
        };
    }

    match saved_path {
        Some(path) => format!(
            "Full output: {}. Read the saved file to inspect omitted output.",
            path.display()
        ),
        None => "Full output could not be saved.".into(),
    }
}

fn sample_line_head(line: &str, max_bytes: usize) -> &str {
    &line[..line.floor_char_boundary(max_bytes)]
}

fn sample_line_tail(line: &str, max_bytes: usize) -> &str {
    &line[line.ceil_char_boundary(line.len().saturating_sub(max_bytes))..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_landing_mid_char_keeps_whole_chars() {
        let text = "é".repeat(10);
        assert_eq!(truncate_to_budget(&text, 5), "éé");
    }

    #[test]
    fn a_budget_past_a_newline_breaks_at_the_newline() {
        assert_eq!(truncate_to_budget("aa\nbb\ncc", 6), "aa\nbb");
    }

    #[test]
    fn a_budget_bigger_than_the_text_keeps_all_of_it() {
        assert_eq!(truncate_to_budget("aa\nbb", 99), "aa\nbb");
    }

    #[test]
    fn small_output_not_truncated() {
        let result = ToolResult::text("hello world");
        let out = apply(result, OutputLimit::Head);
        assert_text(&out, |t| {
            assert_eq!(t, "hello world");
        });
    }

    #[test]
    fn all_directions_enforce_truncation() {
        let big = make_lines(3000);
        for direction in [OutputLimit::Head, OutputLimit::Tail] {
            let out = apply(ToolResult::text(big.clone()), direction);
            assert_text(&out, |t| {
                assert!(
                    t.contains("lines truncated"),
                    "{direction:?} must enforce truncation"
                );
            });
        }
    }

    #[test]
    fn truncates_by_byte_count() {
        let big = "x".repeat(MAX_BYTES + 10000);
        let out = apply(ToolResult::text(big), OutputLimit::Head);
        assert_text(&out, |t| {
            assert!(t.contains("lines truncated"));
        });
    }

    #[test]
    fn byte_overflow_preview_contains_partial_line_content() {
        let huge = "x".repeat(MAX_BYTES + 10_000);
        for direction in [OutputLimit::Head, OutputLimit::Tail] {
            let out = apply(ToolResult::text(huge.clone()), direction);
            assert_text(&out, |t| {
                let preview = t.split("\n\n").nth(1).unwrap_or("");
                assert!(
                    preview.contains("xx"),
                    "{direction:?} preview should sample line content, got: {preview:?}"
                );
                assert!(
                    t.contains("[partial line]") || t.contains("[…line clipped]"),
                    "{direction:?} should mark the partial line, got: {t:?}"
                );
            });
        }
    }

    #[test]
    fn byte_overflow_preview_respects_byte_budget() {
        let huge = "x".repeat(MAX_BYTES * 4);
        for direction in [OutputLimit::Head, OutputLimit::Tail] {
            let out = apply(ToolResult::text(huge.clone()), direction);
            assert_text(&out, |t| {
                assert!(
                    t.len() <= MAX_BYTES + 1024,
                    "{direction:?} preview {} bytes exceeded budget",
                    t.len()
                );
            });
        }
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
    fn error_results_also_truncated() {
        let big = make_lines(3000);
        let out = apply(ToolResult::error(big), OutputLimit::Head);
        assert!(out.outcome.is_error());
        assert_text(&out, |t| {
            assert!(t.contains("lines truncated"));
        });
    }

    #[test]
    fn hint_appears_at_top_for_all_directions() {
        let big = make_lines(5000);
        for direction in [OutputLimit::Head, OutputLimit::Tail] {
            let out = apply(ToolResult::text(big.clone()), direction);
            assert_text(&out, |t| {
                assert!(
                    t.starts_with('['),
                    "{direction:?} should put hint at start, got prefix {:?}",
                    &t[..t.len().min(80)]
                );
                assert!(
                    t.contains("Full output:"),
                    "{direction:?} top hint missing recovery action"
                );
                assert!(
                    t.contains("5000 total"),
                    "{direction:?} top hint missing total"
                );
            });
        }
    }

    #[test]
    fn a_saved_output_path_written_on_another_machine_resolves_here() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MUSH_DATA_DIR", tmp.path());
        }
        let saved = save_full_output("synced from dot").unwrap();
        let name = saved.file_name().unwrap().to_str().unwrap().to_owned();
        let elsewhere =
            std::path::Path::new("/home/nobody-here/.local/share/mush/tool-output").join(&name);
        assert_eq!(saved_output_here(&elsewhere), Some(saved.clone()));
        assert_eq!(saved_output_here(&saved), None);
        assert_eq!(
            saved_output_here(std::path::Path::new(
                "/home/nobody-here/.local/share/mush/tool-output/tool_1.txt"
            )),
            None
        );
        assert_eq!(
            saved_output_here(&std::path::Path::new("/home/nobody-here/elsewhere").join(&name)),
            None
        );
        assert!(!saved_output_name("notes.txt"));
        assert!(saved_output_name("rejected_17.txt"));
    }

    #[test]
    fn non_read_hint_points_to_saved_output_without_search_tools() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MUSH_DATA_DIR", tmp.path());
        }

        let out = truncate_for_tool(
            ToolResult::text(make_lines(10)),
            3,
            usize::MAX,
            OutputLimit::Tail,
            Some("bash"),
        );

        assert_text(&out, |t| {
            assert!(t.contains("Full output:"), "saved path missing: {t}");
            assert!(
                !t.contains("Grep tool") && !t.contains("Read tool"),
                "non-read tools should not suggest search/read recovery: {t}"
            );
        });
    }

    #[test]
    fn non_read_hint_explains_saved_file_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MUSH_DATA_DIR", tmp.path());
        }

        let out = truncate_for_tool(
            ToolResult::text(make_lines(10)),
            3,
            usize::MAX,
            OutputLimit::Tail,
            Some("bash"),
        );

        assert_text(&out, |t| {
            assert!(
                t.contains("Read the saved file"),
                "bash truncation hint should explain how to recover output: {t}"
            );
        });
    }

    #[test]
    fn read_hint_uses_read_offset_recovery() {
        let out = apply_for_tool(
            ToolResult::text(make_lines(5000)),
            OutputLimit::Head,
            Some("read"),
        );

        assert_text(&out, |t| {
            assert!(
                t.contains("Use the Read tool with offset/limit"),
                "read output should suggest read offsets: {t}"
            );
        });
    }

    #[test]
    fn head_keeps_first_lines_after_top_hint() {
        let lines = make_lines(10);
        let out = truncate(ToolResult::text(lines), 3, usize::MAX, OutputLimit::Head);
        assert_text(&out, |t| {
            assert!(t.starts_with('['), "hint at top");
            assert!(t.contains("line 0"));
            assert!(t.contains("line 2"));
            assert!(!t.contains("line 9"), "Head must not include the tail");
            assert!(t.contains("7 lines truncated"));
        });
    }

    const EXPECTED_HINT_FRAGMENTS: &[&str] = &["Full output:"];

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
    fn oversized_details_are_capped_with_the_content() {
        let stdout = "y\n".repeat(mush_ai::types::DETAIL_TEXT_LIMIT);
        let result =
            ToolResult::text("short").with_details(serde_json::json!({ "stdout": stdout }));

        let out = apply(result, OutputLimit::Tail);

        let kept = out.details.unwrap()["stdout"].as_str().unwrap().len();
        assert!(kept <= mush_ai::types::DETAIL_TEXT_LIMIT + 64, "{kept}");
    }

    #[test]
    fn hint_survives_extreme_truncation() {
        let big = make_lines(5000);
        for direction in [OutputLimit::Head, OutputLimit::Tail] {
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
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process so mutating
        unsafe {
            std::env::set_var("MUSH_DATA_DIR", tmp.path());
        }
        let big = make_lines(5000);
        let out = apply(ToolResult::text(big), OutputLimit::Tail);
        assert_text(&out, |t| {
            assert!(
                t.contains("Full output:"),
                "missing 'Full output:' with file path"
            );
            assert!(
                t.contains("tool-output/") && t.contains(".txt"),
                "file path doesn't look right: {t}"
            );
        });
    }

    #[test]
    fn truncated_output_has_line_count_and_total() {
        let big = make_lines(5000);
        for direction in [OutputLimit::Head, OutputLimit::Tail] {
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
