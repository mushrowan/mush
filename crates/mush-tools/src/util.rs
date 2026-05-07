//! shared helpers for built-in tools

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// resolve a user-provided path against the tool's working directory
pub fn resolve_path(cwd: &Path, path_str: &str) -> PathBuf {
    let p = Path::new(path_str);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// returns a one-line note when `path` is excluded from the surrounding
/// git working tree (matched by `.gitignore`, `.git/info/exclude`, or
/// the user's global excludes), otherwise `None`. used by edit / write
/// tools so the model knows the file won't appear in `git status` or
/// commit lists.
///
/// shells out to `git check-ignore --quiet -- <path>`; if git isn't
/// available or `path` isn't inside a git working tree the hint is
/// silently skipped (`None`).
///
/// stdio is fully detached (`Stdio::null()` for in/out/err) because we
/// run inside a TUI that owns the terminal: an inherited stderr would
/// leak git's `fatal: Stopping at filesystem boundary...` message into
/// the status bar when the edited file lives outside any working tree
pub fn gitignore_hint(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    let status = Command::new("git")
        .arg("-C")
        .arg(parent)
        .arg("check-ignore")
        .arg("--quiet")
        .arg("--")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    // exit 0 = ignored, 1 = not ignored, 128 = not a repo / other error
    if status.code() == Some(0) {
        Some(format!(
            "note: {} is gitignored. it won't appear in `git status` or commit lists, so don't reference it as 'tracked' or 'committed' in your commit messages",
            path.display()
        ))
    } else {
        None
    }
}

const MAX_RESULTS: usize = 200;

/// outcome of [`clean_regex_pattern`]: the pattern (possibly cleaned)
/// plus an optional human-readable note when we changed something.
/// the note is meant to be surfaced back to the agent so it can adjust
/// future calls rather than silently relying on us to fix its inputs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanedPattern {
    pub pattern: String,
    pub note: Option<String>,
}

/// strip a single trailing unmatched `"` from a regex pattern.
///
/// agents (especially across grep / find / glob) emit json values like
/// `"pattern": "mise\""` where the value contains a literal trailing
/// `"` that was never opened. fd / rg run the pattern as-is, find no
/// matches (filenames don't contain `"`), and the agent misreads the
/// empty result as "the file doesn't exist" rather than "my pattern was
/// buggy". observed at ~22% of grep calls and ~11% of find calls in
/// real session traces.
///
/// the strip rule is deliberately conservative: only remove the
/// trailing `"` when it's unescaped AND no other unescaped `"` exists
/// in the pattern. that catches `mise"`, `Caddyfile"`,
/// `accounts.email|aerc"` while leaving `\"foo\"`, `"foo"`, and
/// `[\"x\"]` (intentional `"`-matchers) alone.
///
/// returns the original pattern unchanged when the rule doesn't apply
#[must_use]
pub fn clean_regex_pattern(input: &str) -> CleanedPattern {
    match strip_trailing_unmatched_quote(input) {
        Some(stripped) => CleanedPattern {
            pattern: stripped.to_string(),
            note: Some(format!(
                "note: stripped a trailing unmatched `\"` from your pattern (was `{input}`, used `{stripped}`). regex patterns don't need quote-wrapping; pass the bare regex next time"
            )),
        },
        None => CleanedPattern {
            pattern: input.to_string(),
            note: None,
        },
    }
}

/// see [`clean_regex_pattern`]: returns `Some(stripped)` when the
/// trailing `"` should be removed, `None` otherwise
fn strip_trailing_unmatched_quote(pattern: &str) -> Option<&str> {
    let trimmed = pattern.strip_suffix('"')?;

    // the trailing `"` is escaped if preceded by an odd number of
    // backslashes. e.g. `\"` is escaped (1 backslash), `\\"` is not
    // (2 backslashes mean a literal `\` then a literal `"`)
    let trailing_backslashes = trimmed.chars().rev().take_while(|&c| c == '\\').count();
    if trailing_backslashes % 2 == 1 {
        return None;
    }

    // bail if any other unescaped `"` appears in the pattern: that
    // suggests the agent actually means to match a literal `"` somewhere,
    // and we shouldn't second-guess it
    let mut backslash_run = 0usize;
    for c in trimmed.chars() {
        match c {
            '\\' => backslash_run += 1,
            '"' if backslash_run.is_multiple_of(2) => return None,
            _ => backslash_run = 0,
        }
    }

    Some(trimmed)
}

/// inject an optional note as the prefix of `result`'s first text
/// content. used by tools that sanitise input (see
/// [`clean_regex_pattern`]) to surface the change to the agent. if
/// the result has no text content yet, a new text block is prepended
pub fn inject_note(
    mut result: mush_agent::tool::ToolResult,
    note: Option<String>,
) -> mush_agent::tool::ToolResult {
    let Some(note) = note else { return result };

    use mush_ai::types::{TextContent, ToolResultContentPart};
    if let Some(first_text) = result
        .content
        .iter_mut()
        .find_map(|p| matches!(p, ToolResultContentPart::Text(_)).then_some(p))
    {
        if let ToolResultContentPart::Text(t) = first_text {
            t.text = format!("{note}\n\n{}", t.text);
        }
    } else {
        result
            .content
            .insert(0, ToolResultContentPart::Text(TextContent { text: note }));
    }
    result
}

/// truncate line-based output to MAX_RESULTS, appending a count summary
pub fn truncate_lines(lines: &[&str], noun: &str) -> String {
    if lines.is_empty() {
        return format!("no {noun} found");
    }

    if lines.len() > MAX_RESULTS {
        let truncated: String = lines[..MAX_RESULTS].join("\n");
        format!(
            "{truncated}\n\n[{} more {noun}. narrow your search.]",
            lines.len() - MAX_RESULTS
        )
    } else {
        format!("{}\n\n{}", lines.len(), lines.join("\n"))
    }
}

#[cfg(test)]
pub fn extract_text(result: &mush_agent::tool::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|p| match p {
            mush_ai::types::ToolResultContentPart::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_relative() {
        let cwd = Path::new("/home/user/project");
        assert_eq!(
            resolve_path(cwd, "src/main.rs"),
            PathBuf::from("/home/user/project/src/main.rs")
        );
    }

    #[test]
    fn resolve_absolute() {
        let cwd = Path::new("/home/user/project");
        assert_eq!(
            resolve_path(cwd, "/etc/config"),
            PathBuf::from("/etc/config")
        );
    }

    #[test]
    fn truncate_empty() {
        let lines: Vec<&str> = vec![];
        assert_eq!(truncate_lines(&lines, "matches"), "no matches found");
    }

    #[test]
    fn truncate_short() {
        let lines = vec!["a", "b", "c"];
        let result = truncate_lines(&lines, "matches");
        assert!(result.starts_with("3\n\na\nb\nc"));
    }

    #[test]
    fn truncate_long() {
        let lines: Vec<&str> = (0..250).map(|_| "line").collect();
        let result = truncate_lines(&lines, "results");
        assert!(result.contains("[50 more results. narrow your search.]"));
    }

    /// init a fresh git repo in a tempdir and return both the dir and
    /// repo root path. tests skip silently if `git` isn't on PATH so
    /// they still pass on minimal environments
    fn temp_git_repo() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().ok()?;
        let status = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .arg("init")
            .arg("-q")
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        Some(dir)
    }

    #[test]
    fn gitignore_hint_flags_ignored_files() {
        let Some(dir) = temp_git_repo() else {
            return;
        };
        std::fs::write(dir.path().join(".gitignore"), "secrets/\n*.local\n").unwrap();
        std::fs::write(dir.path().join("notes.local"), "hi").unwrap();

        let hint = gitignore_hint(&dir.path().join("notes.local"));
        assert!(
            hint.is_some_and(|h| h.contains("gitignored")),
            "expected gitignored hint for *.local match"
        );
    }

    #[test]
    fn gitignore_hint_returns_none_for_tracked_files() {
        let Some(dir) = temp_git_repo() else {
            return;
        };
        std::fs::write(dir.path().join("README.md"), "# tracked").unwrap();

        let hint = gitignore_hint(&dir.path().join("README.md"));
        assert!(hint.is_none(), "tracked file should not get gitignore hint");
    }

    #[test]
    fn gitignore_hint_returns_none_outside_git_repo() {
        // a tempdir with no .git/ at all: hint must silently be None
        // (git check-ignore exits 128, we treat as "no info")
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("loose.txt"), "x").unwrap();
        assert!(gitignore_hint(&dir.path().join("loose.txt")).is_none());
    }

    // section: clean_regex_pattern

    #[test]
    fn clean_regex_strips_lone_trailing_quote() {
        // the canonical agent bug: `mise"` should become `mise` so fd/rg
        // can find files named `mise` instead of searching for the
        // literal sequence `mise"` (which never matches a real filename)
        let cleaned = clean_regex_pattern("mise\"");
        assert_eq!(cleaned.pattern, "mise");
        let note = cleaned.note.expect("expected a note about the strip");
        assert!(note.contains("mise\""), "note should quote the original");
        assert!(note.contains("mise"), "note should quote the cleaned form");
    }

    #[test]
    fn clean_regex_strips_trailing_quote_in_alternation() {
        // larger real example from session traces. the trailing `"` is
        // unmatched and there are no other unescaped quotes
        let cleaned = clean_regex_pattern("^(mise|Justfile)\"");
        assert_eq!(cleaned.pattern, "^(mise|Justfile)");
        assert!(cleaned.note.is_some());
    }

    #[test]
    fn clean_regex_leaves_escaped_trailing_quote_alone() {
        // `\"` is an escaped quote; agent meant to match a literal `"`.
        // touching this would change the regex semantics
        let cleaned = clean_regex_pattern("foo\\\"");
        assert_eq!(cleaned.pattern, "foo\\\"");
        assert!(cleaned.note.is_none());
    }

    #[test]
    fn clean_regex_leaves_balanced_quotes_alone() {
        // both an opening and a closing unescaped quote: looks deliberate.
        // could be matching a literal `"foo"` substring in source code
        let cleaned = clean_regex_pattern("\"foo\"");
        assert_eq!(cleaned.pattern, "\"foo\"");
        assert!(cleaned.note.is_none());
    }

    #[test]
    fn clean_regex_leaves_multiple_unescaped_quotes_alone() {
        // a pattern with 3 unescaped quotes is suspicious in either
        // direction. don't second-guess: hands off
        let cleaned = clean_regex_pattern("\"foo\"bar\"");
        assert_eq!(cleaned.pattern, "\"foo\"bar\"");
        assert!(cleaned.note.is_none());
    }

    #[test]
    fn clean_regex_leaves_internal_unescaped_quote_alone() {
        // an unescaped quote in the middle (no trailing `"`) shouldn't
        // be touched even though it's solitary
        let cleaned = clean_regex_pattern("foo\"bar");
        assert_eq!(cleaned.pattern, "foo\"bar");
        assert!(cleaned.note.is_none());
    }

    #[test]
    fn clean_regex_passes_through_when_no_trailing_quote() {
        let cleaned = clean_regex_pattern("just_a_normal_pattern");
        assert_eq!(cleaned.pattern, "just_a_normal_pattern");
        assert!(cleaned.note.is_none());
    }

    #[test]
    fn clean_regex_strips_lone_quote_only() {
        // pathological: a pattern that's just `"`. there are no other
        // characters, so by our rule we strip it to empty. fd/rg will
        // then surface a clear error rather than silently match nothing
        let cleaned = clean_regex_pattern("\"");
        assert_eq!(cleaned.pattern, "");
        assert!(cleaned.note.is_some());
    }
}
