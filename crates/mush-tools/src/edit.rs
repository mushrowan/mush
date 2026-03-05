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
         Use this for precise, surgical edits."
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

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("failed to read file: {e}")),
    };

    // count occurrences
    let count = content.matches(old_text).count();
    if count == 0 {
        return ToolResult::error(format!(
            "old text not found in {}. make sure it matches exactly including whitespace",
            path.display()
        ));
    }
    if count > 1 {
        return ToolResult::error(format!(
            "old text found {count} times in {}. it must be unique for a safe edit",
            path.display()
        ));
    }

    let new_content = content.replacen(old_text, new_text, 1);
    match std::fs::write(path, &new_content) {
        Ok(()) => {
            let diff = format_edit_diff(old_text, new_text);
            ToolResult::text(format!("edited {}\n{diff}", path.display()))
        }
        Err(e) => ToolResult::error(format!("failed to write file: {e}")),
    }
}

/// format a diff between old and new text for display
fn format_edit_diff(old_text: &str, new_text: &str) -> String {
    // addition-only: new text contains all of old text with extra content
    if let Some(added) = new_text.strip_prefix(old_text)
        && !added.trim().is_empty() {
            return added
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| format!("+ {l}"))
                .collect::<Vec<_>>()
                .join("\n");
        }
    if let Some(added) = new_text.strip_suffix(old_text)
        && !added.trim().is_empty() {
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
    fn diff_multiline_replacement() {
        let diff = format_edit_diff("line 1\nline 2", "line A\nline B\nline C");
        assert!(diff.contains("- line 1"));
        assert!(diff.contains("- line 2"));
        assert!(diff.contains("+ line A"));
        assert!(diff.contains("+ line B"));
        assert!(diff.contains("+ line C"));
    }
}
