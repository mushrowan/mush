//! edit tool - surgical find-and-replace edits
//!
//! finds exact text in a file and replaces it. the old text must match
//! exactly including whitespace.

use std::path::{Path, PathBuf};

use mush_agent::tool::{AgentTool, ToolResult};

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

            let path = if Path::new(path_str).is_absolute() {
                PathBuf::from(path_str)
            } else {
                self.cwd.join(path_str)
            };

            edit_file(&path, old_text, new_text)
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
        Ok(()) => ToolResult::text(format!("Successfully replaced text in {}.", path.display())),
        Err(e) => ToolResult::error(format!("failed to write file: {e}")),
    }
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
        assert!(!result.is_error);

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("println!(\"world\")"));
        assert!(!content.contains("println!(\"hello\")"));
    }

    #[test]
    fn edit_fails_on_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "fn main() {}").unwrap();

        let result = edit_file(&path, "nonexistent text", "replacement");
        assert!(result.is_error);
    }

    #[test]
    fn edit_fails_on_multiple_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        fs::write(&path, "foo bar foo bar").unwrap();

        let result = edit_file(&path, "foo", "baz");
        assert!(result.is_error);
    }

    #[test]
    fn edit_nonexistent_file() {
        let result = edit_file(Path::new("/nonexistent/file.rs"), "old", "new");
        assert!(result.is_error);
    }

    #[test]
    fn edit_preserves_surrounding_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "before\ntarget line\nafter").unwrap();

        let result = edit_file(&path, "target line", "replaced line");
        assert!(!result.is_error);

        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "before\nreplaced line\nafter");
    }
}
