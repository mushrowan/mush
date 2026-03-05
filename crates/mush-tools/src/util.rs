//! shared helpers for built-in tools

use std::path::{Path, PathBuf};

/// resolve a user-provided path against the tool's working directory
pub fn resolve_path(cwd: &Path, path_str: &str) -> PathBuf {
    let p = Path::new(path_str);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
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
        assert_eq!(resolve_path(cwd, "src/main.rs"), PathBuf::from("/home/user/project/src/main.rs"));
    }

    #[test]
    fn resolve_absolute() {
        let cwd = Path::new("/home/user/project");
        assert_eq!(resolve_path(cwd, "/etc/config"), PathBuf::from("/etc/config"));
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
}
