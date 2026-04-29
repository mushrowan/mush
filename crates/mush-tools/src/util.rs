//! shared helpers for built-in tools

use std::path::{Path, PathBuf};
use std::process::Command;

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
/// silently skipped (`None`)
pub fn gitignore_hint(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    let status = Command::new("git")
        .arg("-C")
        .arg(parent)
        .arg("check-ignore")
        .arg("--quiet")
        .arg("--")
        .arg(path)
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
}
