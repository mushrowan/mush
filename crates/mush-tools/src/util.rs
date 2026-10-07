use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub fn resolve_path(cwd: &Path, path_str: &str) -> PathBuf {
    let p = Path::new(path_str);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

pub(crate) fn canonical_or_given(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn read_lock<T: ?Sized>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn write_lock<T: ?Sized>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn take_flag(args: &mut serde_json::Value, key: &str) -> bool {
    let set = args
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if let serde_json::Value::Object(map) = args {
        map.remove(key);
    }
    set
}

pub(crate) fn with_property(
    mut schema: serde_json::Value,
    key: &str,
    property: serde_json::Value,
) -> serde_json::Value {
    if let Some(serde_json::Value::Object(props)) = schema.get_mut("properties") {
        props.insert(key.into(), property);
    }
    schema
}

#[cfg(unix)]
pub(crate) fn signal_group(pgid: i32, signal: libc::c_int) -> bool {
    // SAFETY: negating the process group id targets the whole group.
    unsafe { libc::kill(-pgid, signal) == 0 }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanedPattern {
    pub pattern: String,
    pub note: Option<String>,
}

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

fn strip_trailing_unmatched_quote(pattern: &str) -> Option<&str> {
    let trimmed = pattern.strip_suffix('"')?;

    let trailing_backslashes = trimmed.chars().rev().take_while(|&c| c == '\\').count();
    if trailing_backslashes % 2 == 1 {
        return None;
    }

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

pub fn inject_note(
    mut result: mush_agent::tool::ToolResult,
    note: Option<String>,
) -> mush_agent::tool::ToolResult {
    let Some(note) = note else { return result };

    use mush_ai::types::{TextContent, ToolResultContentPart};
    let first_text = result.content.iter_mut().find_map(|part| match part {
        ToolResultContentPart::Text(text) => Some(text),
        ToolResultContentPart::Image(_) => None,
    });
    match first_text {
        Some(text) => text.text = format!("{note}\n\n{}", text.text),
        None => result
            .content
            .insert(0, ToolResultContentPart::Text(TextContent { text: note })),
    }
    result
}

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
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("loose.txt"), "x").unwrap();
        assert!(gitignore_hint(&dir.path().join("loose.txt")).is_none());
    }

    #[test]
    fn clean_regex_strips_lone_trailing_quote() {
        let cleaned = clean_regex_pattern("mise\"");
        assert_eq!(cleaned.pattern, "mise");
        let note = cleaned.note.expect("expected a note about the strip");
        assert!(note.contains("mise\""), "note should quote the original");
        assert!(note.contains("mise"), "note should quote the cleaned form");
    }

    #[test]
    fn clean_regex_strips_trailing_quote_in_alternation() {
        let cleaned = clean_regex_pattern("^(mise|Justfile)\"");
        assert_eq!(cleaned.pattern, "^(mise|Justfile)");
        assert!(cleaned.note.is_some());
    }

    #[test]
    fn clean_regex_leaves_escaped_trailing_quote_alone() {
        let cleaned = clean_regex_pattern("foo\\\"");
        assert_eq!(cleaned.pattern, "foo\\\"");
        assert!(cleaned.note.is_none());
    }

    #[test]
    fn clean_regex_leaves_balanced_quotes_alone() {
        let cleaned = clean_regex_pattern("\"foo\"");
        assert_eq!(cleaned.pattern, "\"foo\"");
        assert!(cleaned.note.is_none());
    }

    #[test]
    fn clean_regex_leaves_multiple_unescaped_quotes_alone() {
        let cleaned = clean_regex_pattern("\"foo\"bar\"");
        assert_eq!(cleaned.pattern, "\"foo\"bar\"");
        assert!(cleaned.note.is_none());
    }

    #[test]
    fn clean_regex_leaves_internal_unescaped_quote_alone() {
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
        let cleaned = clean_regex_pattern("\"");
        assert_eq!(cleaned.pattern, "");
        assert!(cleaned.note.is_some());
    }
}
