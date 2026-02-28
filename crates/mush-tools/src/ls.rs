//! ls tool - directory listing with metadata

use std::path::{Path, PathBuf};

use mush_agent::tool::{AgentTool, ToolResult};

pub struct LsTool {
    cwd: PathBuf,
}

impl LsTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

impl AgentTool for LsTool {
    fn name(&self) -> &str { "ls" }
    fn label(&self) -> &str { "List" }
    fn description(&self) -> &str {
        "List files and directories. Shows file sizes and types. \
         Defaults to the current working directory."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "directory to list (defaults to cwd)"
                }
            }
        })
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let path = args["path"]
                .as_str()
                .map(|p| {
                    let path = Path::new(p);
                    if path.is_absolute() { path.to_path_buf() } else { self.cwd.join(p) }
                })
                .unwrap_or_else(|| self.cwd.clone());

            list_dir(&path)
        })
    }
}

fn list_dir(path: &Path) -> ToolResult {
    if !path.exists() {
        return ToolResult::error(format!("path not found: {}", path.display()));
    }

    if !path.is_dir() {
        return ToolResult::error(format!("not a directory: {}", path.display()));
    }

    let mut entries = match std::fs::read_dir(path) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .collect::<Vec<_>>(),
        Err(e) => return ToolResult::error(format!("failed to read directory: {e}")),
    };

    // sort: dirs first, then alphabetically
    entries.sort_by(|a, b| {
        let a_dir = a.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let b_dir = b.file_type().map(|t| t.is_dir()).unwrap_or(false);
        b_dir.cmp(&a_dir).then_with(|| a.file_name().cmp(&b.file_name()))
    });

    if entries.is_empty() {
        return ToolResult::text("(empty directory)");
    }

    let mut lines = Vec::new();

    for entry in &entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ft = entry.file_type().ok();
        let meta = entry.metadata().ok();

        let type_indicator = if ft.as_ref().is_some_and(|t| t.is_dir()) {
            "/"
        } else if ft.as_ref().is_some_and(|t| t.is_symlink()) {
            "@"
        } else {
            ""
        };

        let size = meta
            .as_ref()
            .filter(|m| m.is_file())
            .map(|m| format_size(m.len()))
            .unwrap_or_else(|| "-".into());

        lines.push(format!("{size:>8}  {name}{type_indicator}"));
    }

    ToolResult::text(format!("{} entries\n\n{}", entries.len(), lines.join("\n")))
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn list_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "hello").unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();

        let result = list_dir(dir.path());
        assert!(!result.is_error);
        let text = extract_text(&result);
        assert!(text.contains("subdir/"));
        assert!(text.contains("file.txt"));
    }

    #[test]
    fn list_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let result = list_dir(dir.path());
        let text = extract_text(&result);
        assert!(text.contains("empty directory"));
    }

    #[test]
    fn list_nonexistent() {
        let result = list_dir(Path::new("/nonexistent/path"));
        assert!(result.is_error);
    }

    #[test]
    fn list_file_not_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        fs::write(&file, "hello").unwrap();
        let result = list_dir(&file);
        assert!(result.is_error);
    }

    #[test]
    fn dirs_listed_first() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("aaa.txt"), "").unwrap();
        fs::create_dir(dir.path().join("zzz_dir")).unwrap();

        let result = list_dir(dir.path());
        let text = extract_text(&result);
        let dir_pos = text.find("zzz_dir").unwrap();
        let file_pos = text.find("aaa.txt").unwrap();
        assert!(dir_pos < file_pos, "dirs should come before files");
    }

    #[test]
    fn format_sizes() {
        assert_eq!(format_size(500), "500B");
        assert_eq!(format_size(1536), "1.5K");
        assert_eq!(format_size(2 * 1024 * 1024), "2.0M");
    }

    fn extract_text(result: &ToolResult) -> String {
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
}
