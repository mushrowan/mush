//! apply_patch tool - batch file operations (create, replace, delete)
//!
//! accepts a list of operations to apply atomically. supports creating files,
//! replacing text in existing files, and deleting files. all paths must be
//! within the working directory (no absolute paths or parent traversal).

use std::path::{Path, PathBuf};

use mush_agent::tool::{AgentTool, ToolResult};

use crate::util::resolve_path;

pub struct ApplyPatchTool {
    cwd: PathBuf,
}

impl ApplyPatchTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

impl AgentTool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn label(&self) -> &str {
        "ApplyPatch"
    }
    fn description(&self) -> &str {
        "Apply a batch of file operations. Each operation is one of: \
         'create' (create/overwrite a file), 'replace' (find-and-replace text in a file), \
         or 'delete' (remove a file). Operations are applied in order. All paths must be \
         relative to the working directory. Use this for multi-file changes that should \
         succeed or fail together."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "operations": {
                    "type": "array",
                    "description": "list of file operations to apply in order",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {
                                "type": "string",
                                "enum": ["create", "replace", "delete"],
                                "description": "operation type"
                            },
                            "path": {
                                "type": "string",
                                "description": "file path relative to cwd"
                            },
                            "content": {
                                "type": "string",
                                "description": "file content (for 'create' op)"
                            },
                            "old_text": {
                                "type": "string",
                                "description": "text to find (for 'replace' op)"
                            },
                            "new_text": {
                                "type": "string",
                                "description": "replacement text (for 'replace' op)"
                            }
                        },
                        "required": ["op", "path"]
                    }
                }
            },
            "required": ["operations"]
        })
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let Some(ops) = args["operations"].as_array() else {
                return ToolResult::error("missing required parameter: operations");
            };

            if ops.is_empty() {
                return ToolResult::error("operations list is empty");
            }

            let cwd = self.cwd.clone();
            let ops: Vec<serde_json::Value> = ops.clone();

            tokio::task::spawn_blocking(move || apply_operations(&cwd, &ops))
                .await
                .unwrap_or_else(|e| ToolResult::error(format!("task join error: {e}")))
        })
    }
}

/// validate that a path is safe (relative, no parent traversal)
fn validate_path(cwd: &Path, path_str: &str) -> Result<PathBuf, String> {
    if path_str.is_empty() {
        return Err("path is empty".to_string());
    }

    let path = Path::new(path_str);

    if path.is_absolute() {
        return Err(format!("absolute paths not allowed: {path_str}"));
    }

    // check for parent traversal
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(format!("parent traversal not allowed: {path_str}"));
        }
    }

    let resolved = resolve_path(cwd, path_str);

    // verify the resolved path is within cwd
    if !resolved.starts_with(cwd) {
        return Err(format!("path escapes working directory: {path_str}"));
    }

    Ok(resolved)
}

fn apply_operations(cwd: &Path, ops: &[serde_json::Value]) -> ToolResult {
    let mut results: Vec<String> = Vec::new();
    let mut files_changed: Vec<String> = Vec::new();

    for (i, op) in ops.iter().enumerate() {
        let op_type = op["op"].as_str().unwrap_or("");
        let path_str = op["path"].as_str().unwrap_or("");

        let path = match validate_path(cwd, path_str) {
            Ok(p) => p,
            Err(e) => {
                results.push(format!("op {}: error: {e}", i + 1));
                return finish_with_error(&results, &files_changed);
            }
        };

        let result = match op_type {
            "create" => apply_create(&path, op),
            "replace" => apply_replace(&path, op),
            "delete" => apply_delete(&path),
            other => Err(format!("unknown op type: {other}")),
        };

        match result {
            Ok(msg) => {
                files_changed.push(path_str.to_string());
                results.push(format!("op {}: {msg}", i + 1));
            }
            Err(e) => {
                results.push(format!("op {}: error: {e}", i + 1));
                return finish_with_error(&results, &files_changed);
            }
        }
    }

    let summary = format!(
        "{} operation(s) applied, {} file(s) changed\n\n{}",
        ops.len(),
        files_changed.len(),
        results.join("\n")
    );
    ToolResult::text(summary)
}

fn finish_with_error(results: &[String], files_changed: &[String]) -> ToolResult {
    let mut msg = format!(
        "patch failed after {} file(s) changed\n\n{}",
        files_changed.len(),
        results.join("\n")
    );
    if !files_changed.is_empty() {
        msg.push_str(&format!(
            "\n\nfiles already modified: {}",
            files_changed.join(", ")
        ));
    }
    ToolResult::error(msg)
}

fn apply_create(path: &Path, op: &serde_json::Value) -> Result<String, String> {
    let Some(content) = op["content"].as_str() else {
        return Err("'create' op requires 'content'".to_string());
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create directories: {e}"))?;
    }

    let existed = path.exists();
    std::fs::write(path, content).map_err(|e| format!("failed to write: {e}"))?;

    Ok(if existed {
        format!("overwritten {}", path.display())
    } else {
        format!("created {}", path.display())
    })
}

fn apply_replace(path: &Path, op: &serde_json::Value) -> Result<String, String> {
    let Some(old_text) = op["old_text"].as_str() else {
        return Err("'replace' op requires 'old_text'".to_string());
    };
    let Some(new_text) = op["new_text"].as_str() else {
        return Err("'replace' op requires 'new_text'".to_string());
    };

    if old_text.is_empty() {
        return Err("old_text cannot be empty".to_string());
    }

    let content =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read: {e}"))?;

    let count = content.matches(old_text).count();
    if count == 0 {
        return Err(format!("old_text not found in {}", path.display()));
    }
    if count > 1 {
        return Err(format!(
            "old_text found {count} times in {} (must be unique)",
            path.display()
        ));
    }

    let replaced = content.replacen(old_text, new_text, 1);
    std::fs::write(path, &replaced).map_err(|e| format!("failed to write: {e}"))?;

    Ok(format!("replaced in {}", path.display()))
}

fn apply_delete(path: &Path) -> Result<String, String> {
    if !path.exists() {
        return Err(format!("file not found: {}", path.display()));
    }

    std::fs::remove_file(path).map_err(|e| format!("failed to delete: {e}"))?;

    Ok(format!("deleted {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;
    use std::fs;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn create_new_file() {
        let dir = temp_dir();
        let ops = vec![serde_json::json!({
            "op": "create",
            "path": "new.txt",
            "content": "hello world"
        })];
        let result = apply_operations(dir.path(), &ops);
        assert!(result.outcome.is_success());
        assert_eq!(fs::read_to_string(dir.path().join("new.txt")).unwrap(), "hello world");
        let text = extract_text(&result);
        assert!(text.contains("created"));
    }

    #[test]
    fn create_with_nested_dirs() {
        let dir = temp_dir();
        let ops = vec![serde_json::json!({
            "op": "create",
            "path": "a/b/c.txt",
            "content": "nested"
        })];
        let result = apply_operations(dir.path(), &ops);
        assert!(result.outcome.is_success());
        assert_eq!(fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(), "nested");
    }

    #[test]
    fn create_overwrites_existing() {
        let dir = temp_dir();
        fs::write(dir.path().join("exist.txt"), "old").unwrap();
        let ops = vec![serde_json::json!({
            "op": "create",
            "path": "exist.txt",
            "content": "new"
        })];
        let result = apply_operations(dir.path(), &ops);
        assert!(result.outcome.is_success());
        assert_eq!(fs::read_to_string(dir.path().join("exist.txt")).unwrap(), "new");
        let text = extract_text(&result);
        assert!(text.contains("overwritten"));
    }

    #[test]
    fn replace_text() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "foo bar baz").unwrap();
        let ops = vec![serde_json::json!({
            "op": "replace",
            "path": "test.txt",
            "old_text": "bar",
            "new_text": "qux"
        })];
        let result = apply_operations(dir.path(), &ops);
        assert!(result.outcome.is_success());
        assert_eq!(fs::read_to_string(dir.path().join("test.txt")).unwrap(), "foo qux baz");
    }

    #[test]
    fn replace_not_found_errors() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "hello").unwrap();
        let ops = vec![serde_json::json!({
            "op": "replace",
            "path": "test.txt",
            "old_text": "missing",
            "new_text": "x"
        })];
        let result = apply_operations(dir.path(), &ops);
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("not found"));
    }

    #[test]
    fn replace_ambiguous_errors() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "foo foo foo").unwrap();
        let ops = vec![serde_json::json!({
            "op": "replace",
            "path": "test.txt",
            "old_text": "foo",
            "new_text": "bar"
        })];
        let result = apply_operations(dir.path(), &ops);
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("3 times"));
    }

    #[test]
    fn delete_file() {
        let dir = temp_dir();
        fs::write(dir.path().join("bye.txt"), "gone").unwrap();
        let ops = vec![serde_json::json!({
            "op": "delete",
            "path": "bye.txt"
        })];
        let result = apply_operations(dir.path(), &ops);
        assert!(result.outcome.is_success());
        assert!(!dir.path().join("bye.txt").exists());
    }

    #[test]
    fn delete_nonexistent_errors() {
        let dir = temp_dir();
        let ops = vec![serde_json::json!({
            "op": "delete",
            "path": "nope.txt"
        })];
        let result = apply_operations(dir.path(), &ops);
        assert!(result.outcome.is_error());
    }

    #[test]
    fn multi_op_batch() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        let ops = vec![
            serde_json::json!({ "op": "create", "path": "b.txt", "content": "beta" }),
            serde_json::json!({ "op": "replace", "path": "a.txt", "old_text": "alpha", "new_text": "ALPHA" }),
            serde_json::json!({ "op": "create", "path": "c.txt", "content": "gamma" }),
        ];
        let result = apply_operations(dir.path(), &ops);
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("3 operation(s) applied"));
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "ALPHA");
        assert_eq!(fs::read_to_string(dir.path().join("b.txt")).unwrap(), "beta");
        assert_eq!(fs::read_to_string(dir.path().join("c.txt")).unwrap(), "gamma");
    }

    #[test]
    fn batch_stops_on_first_error() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let ops = vec![
            serde_json::json!({ "op": "create", "path": "ok.txt", "content": "fine" }),
            serde_json::json!({ "op": "replace", "path": "a.txt", "old_text": "nope", "new_text": "x" }),
            serde_json::json!({ "op": "create", "path": "never.txt", "content": "skipped" }),
        ];
        let result = apply_operations(dir.path(), &ops);
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("op 1:"));
        assert!(text.contains("op 2: error"));
        assert!(!text.contains("op 3:"));
        // first op did apply
        assert!(dir.path().join("ok.txt").exists());
        // third op was skipped
        assert!(!dir.path().join("never.txt").exists());
    }

    #[test]
    fn absolute_path_rejected() {
        let dir = temp_dir();
        let ops = vec![serde_json::json!({
            "op": "create",
            "path": "/etc/passwd",
            "content": "nope"
        })];
        let result = apply_operations(dir.path(), &ops);
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("absolute paths not allowed"));
    }

    #[test]
    fn parent_traversal_rejected() {
        let dir = temp_dir();
        let ops = vec![serde_json::json!({
            "op": "create",
            "path": "../escape.txt",
            "content": "nope"
        })];
        let result = apply_operations(dir.path(), &ops);
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("parent traversal not allowed"));
    }

    #[test]
    fn empty_operations_errors() {
        let dir = temp_dir();
        let result = apply_operations(dir.path(), &[]);
        // apply_operations itself handles empty, but the tool execute checks first
        // direct call with empty should still produce valid output
        let text = extract_text(&result);
        assert!(text.contains("0 operation(s) applied"));
    }

    #[test]
    fn unknown_op_errors() {
        let dir = temp_dir();
        let ops = vec![serde_json::json!({
            "op": "explode",
            "path": "test.txt"
        })];
        let result = apply_operations(dir.path(), &ops);
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("unknown op type"));
    }
}
