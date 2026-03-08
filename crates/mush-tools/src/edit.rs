//! edit tool - surgical find-and-replace edits
//!
//! finds exact text in a file and replaces it. the old text must match
//! exactly including whitespace.

use std::path::{Path, PathBuf};

use mush_agent::tool::{AgentTool, ToolResult};

use crate::util::resolve_path;

pub struct EditTool {
    cwd: PathBuf,
}

impl EditTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn label(&self) -> &str {
        "Edit"
    }
    fn description(&self) -> &str {
        "Edit a file by replacing exact text. The oldText must match exactly (including whitespace). \
         Use this for precise, surgical edits. The oldText must appear exactly once in the file - \
         if it matches multiple locations, the edit will fail. Include enough surrounding context \
         (nearby lines) in oldText to make the match unique."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "path to the file to edit (relative or absolute)"
                },
                "oldText": {
                    "type": "string",
                    "description": "exact text to find and replace (must match exactly)"
                },
                "newText": {
                    "type": "string",
                    "description": "new text to replace the old text with"
                }
            },
            "required": ["path", "oldText", "newText"]
        })
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let Some(path_str) = args["path"].as_str() else {
                return ToolResult::error("missing required parameter: path");
            };
            let Some(old_text) = args["oldText"].as_str() else {
                return ToolResult::error("missing required parameter: oldText");
            };
            let Some(new_text) = args["newText"].as_str() else {
                return ToolResult::error("missing required parameter: newText");
            };

            let path = resolve_path(&self.cwd, path_str);
            let old_text = old_text.to_string();
            let new_text = new_text.to_string();

            tokio::task::spawn_blocking(move || edit_file(&path, &old_text, &new_text))
                .await
                .unwrap_or_else(|e| ToolResult::error(format!("task join error: {e}")))
        })
    }
}

fn edit_file(path: &Path, old_text: &str, new_text: &str) -> ToolResult {
    if !path.exists() {
        return ToolResult::error(format!("file not found: {}", path.display()));
    }
    if old_text.is_empty() {
        return ToolResult::error("old text cannot be empty");
    }
    if old_text == new_text {
        return ToolResult::error("old text and new text are identical");
    }

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return ToolResult::error(format!("failed to read file: {e}")),
    };

    let has_bom = bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
    let text_bytes = if has_bom { &bytes[3..] } else { &bytes[..] };
    let content = match std::str::from_utf8(text_bytes) {
        Ok(s) => s,
        Err(e) => return ToolResult::error(format!("failed to read file as utf-8 text: {e}")),
    };

    let mut candidates: Vec<(String, String)> = vec![(old_text.to_string(), new_text.to_string())];

    if content.contains("\r\n") && old_text.contains('\n') && !old_text.contains("\r\n") {
        candidates.push((
            old_text.replace("\n", "\r\n"),
            new_text.replace("\n", "\r\n"),
        ));
    }

    if !content.contains("\r\n") && old_text.contains("\r\n") {
        candidates.push((
            old_text.replace("\r\n", "\n"),
            new_text.replace("\r\n", "\n"),
        ));
    }

    candidates.dedup();

    // also try with/without trailing newline (LLMs often get this wrong)
    let mut extra = Vec::new();
    for (o, n) in &candidates {
        if o.ends_with('\n') && !content.contains(o.as_str()) {
            let trimmed = o.trim_end_matches('\n').trim_end_matches('\r');
            extra.push((trimmed.to_string(), n.clone()));
        } else if !o.ends_with('\n') {
            extra.push((format!("{o}\n"), format!("{n}\n")));
            if content.contains("\r\n") {
                extra.push((format!("{o}\r\n"), format!("{n}\r\n")));
            }
        }
    }
    candidates.extend(extra);

    // trailing whitespace per-line: LLMs often omit trailing spaces/tabs on lines.
    // slide a window over content lines, compare with trimEnd on each line,
    // and yield the actual content substring (with its real whitespace) as a candidate
    for actual_old in find_line_trimmed_matches(content, old_text) {
        if actual_old != old_text {
            let adapted_new = if actual_old.contains("\r\n") && !new_text.contains("\r\n") {
                new_text.replace('\n', "\r\n")
            } else {
                new_text.to_string()
            };
            candidates.push((actual_old, adapted_new));
        }
    }

    candidates.dedup();

    let mut multiple = false;

    for (match_old, match_new) in candidates {
        let count = content.matches(&match_old).count();
        if count == 0 {
            continue;
        }
        if count > 1 {
            multiple = true;
            continue;
        }

        let replaced = content.replacen(&match_old, &match_new, 1);
        let write_result = if has_bom {
            let mut out = vec![0xEF, 0xBB, 0xBF];
            out.extend_from_slice(replaced.as_bytes());
            std::fs::write(path, out)
        } else {
            std::fs::write(path, replaced.as_bytes())
        };

        return match write_result {
            Ok(()) => {
                let diff = format_edit_diff(old_text, new_text);
                ToolResult::text(format!("edited {}\n{diff}", path.display()))
            }
            Err(e) => ToolResult::error(format!("failed to write file: {e}")),
        };
    }

    if multiple {
        // find all match locations to help the caller add context
        let first_line = old_text.lines().next().unwrap_or(old_text);
        let match_lines: Vec<usize> = content
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(first_line.trim()))
            .map(|(i, _)| i + 1)
            .collect();
        let locations = if match_lines.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nmatches near lines: {}\nhint: include more surrounding context in oldText to make the match unique",
                match_lines.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ")
            )
        };
        return ToolResult::error(format!(
            "oldText found multiple times in {}.{}",
            path.display(),
            locations
        ));
    }

    // try to provide helpful context about why the match failed
    let hint = find_near_miss(content, old_text);
    let display = path.display();

    match hint {
        NearMiss::WhitespaceDifference(line_num) => ToolResult::error(format!(
            "oldText not found in {display} (but a whitespace-normalised match was found near \
             line {line_num}). check for extra/missing spaces, tabs, or trailing whitespace"
        )),
        NearMiss::SimilarLines(lines) => {
            let mut msg = format!("oldText not found in {display}.\n\nmost similar lines:\n");
            for (num, line) in lines {
                msg.push_str(&format!("  {num}: {line}\n"));
            }
            msg.push_str(
                "\nhint: use Read to see the exact file contents, then retry with corrected oldText",
            );
            ToolResult::error(msg)
        }
        NearMiss::None => ToolResult::error(format!(
            "oldText not found in {display}. make sure it matches exactly including whitespace. \
             use Read to verify the current file contents"
        )),
    }
}

enum NearMiss {
    /// normalising whitespace would have matched, near this line
    WhitespaceDifference(usize),
    /// the most similar lines from the file (line_number, line_content)
    SimilarLines(Vec<(usize, String)>),
    /// nothing close found
    None,
}

/// find substrings in content that match old_text when each line is trimEnd'd
///
/// slides a window of old_text's line count across content's lines, comparing
/// each pair after stripping trailing spaces/tabs. yields the actual content
/// substring (with its real whitespace) for each match position
fn find_line_trimmed_matches(content: &str, old_text: &str) -> Vec<String> {
    let content_lines: Vec<&str> = content.split_inclusive('\n').collect();
    let search_lines: Vec<&str> = old_text.split_inclusive('\n').collect();

    if search_lines.is_empty() {
        return vec![];
    }

    let mut results = Vec::new();
    let max_start = content_lines.len().saturating_sub(search_lines.len());

    'outer: for i in 0..=max_start {
        for (j, search_line) in search_lines.iter().enumerate() {
            let content_line = content_lines[i + j];
            if content_line.trim_end() != search_line.trim_end() {
                continue 'outer;
            }
        }

        // all lines matched - extract the actual content substring
        let start: usize = content_lines[..i].iter().map(|l| l.len()).sum();
        let mut end: usize = content_lines[..i + search_lines.len()]
            .iter()
            .map(|l| l.len())
            .sum();

        // if old_text doesn't end with a newline, trim the match to exclude
        // the content line's trailing whitespace and newline
        if !old_text.ends_with('\n') {
            let last_content_line = content_lines[i + search_lines.len() - 1];
            let trimmed_len = last_content_line.trim_end().len();
            end = content_lines[..i + search_lines.len() - 1]
                .iter()
                .map(|l| l.len())
                .sum::<usize>()
                + trimmed_len;
        }

        results.push(content[start..end].to_string());
    }

    results
}

/// check if oldText nearly matches something in the file
fn find_near_miss(content: &str, old_text: &str) -> NearMiss {
    // check whitespace-normalised match
    let normalise = |s: &str| -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let norm_old = normalise(old_text);
    if !norm_old.is_empty() {
        let norm_content = normalise(content);
        if norm_content.contains(&norm_old) {
            // find approximate line number
            let line_num = content
                .lines()
                .enumerate()
                .find(|(_, line)| normalise(line).contains(normalise(old_text.lines().next().unwrap_or("")).as_str()))
                .map(|(i, _)| i + 1)
                .unwrap_or(1);
            return NearMiss::WhitespaceDifference(line_num);
        }
    }

    // find most similar lines using first line of oldText
    let first_old_line = old_text.lines().next().unwrap_or("").trim();
    if first_old_line.len() < 4 {
        return NearMiss::None;
    }

    let mut scored: Vec<(usize, &str, f64)> = content
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let score = line_similarity(first_old_line, line.trim());
            if score > 0.4 {
                Some((i + 1, line, score))
            } else {
                Option::None
            }
        })
        .collect();

    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(3);

    if scored.is_empty() {
        NearMiss::None
    } else {
        NearMiss::SimilarLines(
            scored
                .into_iter()
                .map(|(num, line, _)| (num, line.to_string()))
                .collect(),
        )
    }
}

/// simple similarity score between two strings (jaccard on character bigrams)
fn line_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let bigrams = |s: &str| -> std::collections::HashSet<(char, char)> {
        s.chars().zip(s.chars().skip(1)).collect()
    };

    let a_bi = bigrams(a);
    let b_bi = bigrams(b);

    if a_bi.is_empty() || b_bi.is_empty() {
        return 0.0;
    }

    let intersection = a_bi.intersection(&b_bi).count() as f64;
    let union = a_bi.union(&b_bi).count() as f64;

    intersection / union
}

/// strip common leading whitespace from both texts for readable diffs
fn dedent_pair(old_text: &str, new_text: &str) -> (String, String) {
    let min_indent = old_text
        .lines()
        .chain(new_text.lines())
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    if min_indent == 0 {
        return (old_text.to_string(), new_text.to_string());
    }

    let strip = |text: &str| -> String {
        text.lines()
            .map(|l| {
                if l.len() >= min_indent {
                    &l[min_indent..]
                } else {
                    l.trim_start()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    (strip(old_text), strip(new_text))
}

/// format a diff between old and new text for display
fn format_edit_diff(old_text: &str, new_text: &str) -> String {
    let (old_text, new_text) = dedent_pair(old_text, new_text);

    // addition-only: new text contains all of old text with extra content
    if let Some(added) = new_text.strip_prefix(old_text.as_str())
        && !added.trim().is_empty()
    {
        return added
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| format!("+ {l}"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(added) = new_text.strip_suffix(old_text.as_str())
        && !added.trim().is_empty()
    {
        return added
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| format!("+ {l}"))
            .collect::<Vec<_>>()
            .join("\n");
    }

    // show old lines as removed, new lines as added
    let mut result = String::new();
    for line in old_text.lines() {
        result.push_str(&format!("- {line}\n"));
    }
    for line in new_text.lines() {
        result.push_str(&format!("+ {line}\n"));
    }
    result.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;
    use std::fs;

    #[test]
    fn edit_replaces_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "fn main() {\n    println!(\"hello\");\n}").unwrap();

        let result = edit_file(&path, "println!(\"hello\")", "println!(\"world\")");
        assert!(result.outcome.is_success());

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("println!(\"world\")"));
        assert!(!content.contains("println!(\"hello\")"));

        // output should contain diff
        let output = result
            .content
            .iter()
            .find_map(|p| match p {
                mush_ai::types::ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(output.contains("- "));
        assert!(output.contains("+ "));
    }

    #[test]
    fn edit_fails_on_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = edit_file(&path, "nonexistent text", "replacement");
        assert!(result.outcome.is_error());
    }

    #[test]
    fn edit_fails_on_multiple_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "foo bar foo bar").unwrap();

        let result = edit_file(&path, "foo", "baz");
        assert!(result.outcome.is_error());
    }

    #[test]
    fn edit_nonexistent_file() {
        let result = edit_file(Path::new("/nonexistent/file.rs"), "old", "new");
        assert!(result.outcome.is_error());
    }

    #[test]
    fn edit_preserves_surrounding_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "before\ntarget line\nafter").unwrap();

        let result = edit_file(&path, "target line", "replaced line");
        assert!(result.outcome.is_success());

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "before\nreplaced line\nafter");
    }

    #[test]
    fn edit_supports_lf_old_text_on_crlf_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "before\r\ntarget line\r\nafter\r\n").unwrap();

        let result = edit_file(&path, "target line\nafter", "replaced\nafter");
        assert!(result.outcome.is_success());

        let bytes = fs::read(&path).unwrap();
        let content = String::from_utf8(bytes).unwrap();
        assert!(content.contains("before\r\nreplaced\r\nafter\r\n"));
    }

    #[test]
    fn edit_preserves_utf8_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bom.txt");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"hello\nworld\n");
        fs::write(&path, bytes).unwrap();

        let result = edit_file(&path, "hello", "hi");
        assert!(result.outcome.is_success());

        let out = fs::read(&path).unwrap();
        assert!(out.starts_with(&[0xEF, 0xBB, 0xBF]));
        let text = String::from_utf8(out[3..].to_vec()).unwrap();
        assert_eq!(text, "hi\nworld\n");
    }

    #[test]
    fn edit_fails_on_identical_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("same.txt");
        fs::write(&path, "unchanged").unwrap();

        let result = edit_file(&path, "unchanged", "unchanged");
        assert!(result.outcome.is_error());
    }

    #[test]
    fn diff_shows_additions_only_when_appending() {
        let diff = format_edit_diff("existing line", "existing line\nnew line");
        assert!(diff.contains("+ new line"));
        assert!(!diff.contains("- "));
    }

    #[test]
    fn diff_shows_additions_only_when_prepending() {
        let diff = format_edit_diff("existing line", "new line\nexisting line");
        assert!(diff.contains("+ new line"));
        assert!(!diff.contains("- "));
    }

    #[test]
    fn diff_shows_both_for_replacement() {
        let diff = format_edit_diff("old line", "new line");
        assert!(diff.contains("- old line"));
        assert!(diff.contains("+ new line"));
    }

    #[test]
    fn diff_dedents_common_whitespace() {
        let diff = format_edit_diff(
            "        deeply indented\n        old code",
            "        deeply indented\n        new code",
        );
        // should strip the 8-space common indent
        assert!(diff.contains("- old code"));
        assert!(diff.contains("+ new code"));
        assert!(!diff.contains("        "));
    }

    #[test]
    fn diff_dedent_preserves_relative_indent() {
        let diff = format_edit_diff(
            "    base\n        nested",
            "    base\n            more nested",
        );
        // 4-space common indent stripped, relative indent preserved
        assert!(diff.contains("- base"));
        assert!(diff.contains("-     nested"));
        assert!(diff.contains("+ base"));
        assert!(diff.contains("+         more nested"));
    }

    #[test]
    fn diff_no_dedent_when_no_common_indent() {
        let diff = format_edit_diff("old line here", "new line here");
        assert!(diff.contains("- old line here"));
        assert!(diff.contains("+ new line here"));
    }

    #[test]
    fn diff_multiline_replacement() {
        let diff = format_edit_diff("line 1\nline 2", "line A\nline B\nline C");
        assert!(diff.contains("- line 1"));
        assert!(diff.contains("- line 2"));
        assert!(diff.contains("+ line A"));
        assert!(diff.contains("+ line B"));
        assert!(diff.contains("+ line C"));
    }

    #[test]
    fn near_miss_whitespace_difference() {
        let content = "fn main() {\n    println!(\"hello\");\n}\n";
        // extra spaces in the search text
        let old_text = "fn main() {\n        println!(\"hello\");\n}\n";
        match find_near_miss(content, old_text) {
            NearMiss::WhitespaceDifference(_) => {}
            NearMiss::SimilarLines(_) => panic!("expected WhitespaceDifference, got SimilarLines"),
            NearMiss::None => panic!("expected WhitespaceDifference, got None"),
        }
    }

    #[test]
    fn near_miss_similar_lines() {
        let content = "fn process_data(input: &str) -> Result<(), Error> {\n    validate(input)?;\n    Ok(())\n}\n";
        // slightly wrong return type
        let old_text = "fn process_data(input: &str) -> Result<(), MyError> {";
        match find_near_miss(content, old_text) {
            NearMiss::SimilarLines(lines) => {
                assert!(!lines.is_empty());
                assert!(lines[0].1.contains("process_data"));
            }
            NearMiss::WhitespaceDifference(_) => panic!("expected SimilarLines, got WhitespaceDifference"),
            NearMiss::None => panic!("expected SimilarLines, got None"),
        }
    }

    #[test]
    fn near_miss_nothing_close() {
        let content = "fn main() {\n    println!(\"hello\");\n}\n";
        let old_text = "completely unrelated text that appears nowhere";
        assert!(matches!(find_near_miss(content, old_text), NearMiss::None));
    }

    #[test]
    fn edit_no_match_shows_hint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "fn process(input: &str) {\n    validate(input);\n}\n").unwrap();

        let result = edit_file(&path, "fn process(input: &String) {", "fn replaced() {");
        let text = extract_text(&result);
        assert!(text.contains("similar lines") || text.contains("whitespace"));
    }

    #[test]
    fn edit_multiple_match_shows_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "let x = 1;\nlet y = 2;\nlet x = 1;\n").unwrap();

        let result = edit_file(&path, "let x = 1;", "let x = 42;");
        let text = extract_text(&result);
        assert!(text.contains("multiple times"));
        assert!(text.contains("lines:"));
    }

    #[test]
    fn similarity_identical() {
        assert!((line_similarity("hello world", "hello world") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn similarity_empty() {
        assert_eq!(line_similarity("", "hello"), 0.0);
        assert_eq!(line_similarity("hello", ""), 0.0);
    }

    #[test]
    fn edit_trailing_newline_normalisation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "fn main() {\n    hello();\n}\n").unwrap();

        // oldText without trailing newline should still match
        let result = edit_file(&path, "fn main() {\n    hello();\n}", "fn main() {\n    world();\n}");
        assert!(result.outcome.is_success());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("world()"));
    }

    #[test]
    fn edit_extra_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "hello world").unwrap();

        // oldText with trailing newline when file doesn't have one
        let result = edit_file(&path, "hello world\n", "goodbye world\n");
        assert!(result.outcome.is_success());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("goodbye world"));
    }

    #[test]
    fn edit_matches_with_trailing_whitespace_in_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        // file has trailing spaces on lines
        fs::write(&path, "fn main() {  \n    println!(\"hello\");  \n}\n").unwrap();

        // oldText has no trailing spaces (as LLM would send)
        let result = edit_file(
            &path,
            "fn main() {\n    println!(\"hello\");\n}\n",
            "fn main() {\n    println!(\"goodbye\");\n}\n",
        );
        let text = extract_text(&result);
        assert!(!text.contains("not found"), "should match: {text}");

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("goodbye"));
        assert!(!content.contains("hello"));
    }

    #[test]
    fn edit_matches_trailing_tabs_in_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "line one\t\nline two\t\t\n").unwrap();

        let result = edit_file(&path, "line one\nline two\n", "replaced one\nreplaced two\n");
        let text = extract_text(&result);
        assert!(!text.contains("not found"), "should match: {text}");

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "replaced one\nreplaced two\n");
    }

    #[test]
    fn edit_trailing_ws_no_match_when_content_differs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "foo  \nbar  \n").unwrap();

        // oldText content differs, not just whitespace
        let result = edit_file(&path, "fox\nbaz\n", "replaced\n");
        let text = extract_text(&result);
        assert!(text.contains("not found"));
    }

    #[test]
    fn edit_trailing_ws_rejects_ambiguous_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "foo  \nbar\nfoo  \nbaz\n").unwrap();

        // "foo\n" matches twice after trimming
        let result = edit_file(&path, "foo\n", "replaced\n");
        let text = extract_text(&result);
        // should fail - ambiguous (or succeed via exact if "foo\n" appears exactly once)
        // "foo\n" doesn't appear exactly in the file (file has "foo  \n"), and
        // trimmed match finds 2 positions, so it should fail
        assert!(text.contains("not found") || text.contains("multiple"));
    }

    #[test]
    fn edit_trailing_ws_without_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "alpha  \nbeta  \ngamma\n").unwrap();

        // oldText spans two lines, no trailing newline
        let result = edit_file(
            &path,
            "alpha\nbeta",
            "one\ntwo",
        );
        let text = extract_text(&result);
        assert!(!text.contains("not found"), "should match: {text}");

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("one\ntwo"));
    }
}
