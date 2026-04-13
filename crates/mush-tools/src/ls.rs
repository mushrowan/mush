//! ls tool - directory listing with metadata

use std::path::{Path, PathBuf};

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::util::resolve_path;

const MAX_ENTRIES: usize = 500;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LsArgs {
    path: Option<String>,
}

pub struct LsTool {
    cwd: PathBuf,
}

impl LsTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn label(&self) -> &str {
        "List"
    }
    fn description(&self) -> &str {
        "List files and directories. Shows file sizes and types. \
         Returns at most 500 entries. Defaults to the current working directory. \
         Use find or glob to filter large directories."
    }

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Head
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
            let args = match parse_tool_args::<LsArgs>(args) {
                Ok(args) => args,
                Err(error) => return error,
            };

            let path = args
                .path
                .as_deref()
                .map(|path| resolve_path(&self.cwd, path))
                .unwrap_or_else(|| self.cwd.clone());

            tokio::task::spawn_blocking(move || list_dir(&path))
                .await
                .unwrap_or_else(|e| ToolResult::error(format!("task join error: {e}")))
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
        Ok(rd) => rd.filter_map(|e| e.ok()).collect::<Vec<_>>(),
        Err(e) => return ToolResult::error(format!("failed to read directory: {e}")),
    };

    entries.sort_by(|a, b| {
        let a_dir = a.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let b_dir = b.file_type().map(|t| t.is_dir()).unwrap_or(false);
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.file_name().cmp(&b.file_name()))
    });

    if entries.is_empty() {
        return ToolResult::text("(empty directory)");
    }

    let total_entries = entries.len();
    let capped = total_entries > MAX_ENTRIES;
    tracing::debug!(
        path = %path.display(),
        total_entries,
        capped,
        max = MAX_ENTRIES,
        "listing directory"
    );
    if capped {
        entries.truncate(MAX_ENTRIES);
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
            .map_or_else(|| "?".to_string(), |m| format_size(m.len()));
        lines.push(format!("{size:>8}  {name}{type_indicator}"));
    }

    let mut output = lines.join("\n");
    if capped {
        let omitted = total_entries - MAX_ENTRIES;
        output.push_str(&format!(
            "\n\n[{omitted} more entries. use find or glob to filter.]"
        ));
    }

    ToolResult::text(output)
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    match bytes {
        b if b >= GB => format!("{:.1}G", b as f64 / GB as f64),
        b if b >= MB => format!("{:.1}M", b as f64 / MB as f64),
        b if b >= KB => format!("{:.1}K", b as f64 / KB as f64),
        b => format!("{b}B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;
    use std::fs;

    #[test]
    fn list_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let result = list_dir(dir.path());
        let text = extract_text(&result);
        assert_eq!(text, "(empty directory)");
    }

    #[test]
    fn list_nonexistent() {
        let result = list_dir(Path::new("/definitely/does/not/exist"));
        assert!(result.outcome.is_error());
    }

    #[test]
    fn list_file_not_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        fs::write(&file, "x").unwrap();
        let result = list_dir(&file);
        assert!(result.outcome.is_error());
    }

    #[test]
    fn list_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();

        let result = list_dir(dir.path());
        let text = extract_text(&result);
        assert!(text.contains("src/"));
        assert!(text.contains("a.txt"));
    }

    #[test]
    fn dirs_listed_first() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("zzz_dir")).unwrap();
        fs::write(dir.path().join("aaa_file.txt"), "hello").unwrap();

        let result = list_dir(dir.path());
        let text = extract_text(&result);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].contains("zzz_dir/"));
        assert!(lines[1].contains("aaa_file.txt"));
    }

    #[test]
    fn large_directory_capped_at_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..600 {
            fs::write(dir.path().join(format!("file_{i:04}.txt")), "x").unwrap();
        }
        let result = list_dir(dir.path());
        let text = extract_text(&result);
        let content_lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        // 500 entries + 1 hint line
        assert!(content_lines.len() <= MAX_ENTRIES + 1);
        assert!(text.contains("more entries"));
        assert!(text.contains("find or glob"));
    }

    #[test]
    fn output_limit_is_head() {
        use mush_agent::tool::OutputLimit;
        let tool = LsTool::new(PathBuf::from("."));
        assert_eq!(tool.output_limit(), OutputLimit::Head);
    }

    #[test]
    fn format_sizes() {
        assert_eq!(format_size(123), "123B");
        assert_eq!(format_size(2048), "2.0K");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0M");
    }
}
