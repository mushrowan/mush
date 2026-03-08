//! read tool - reads file contents with optional offset/limit

use std::path::{Path, PathBuf};

use mush_agent::tool::{AgentTool, ToolResult};

use crate::util::resolve_path;

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;

const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

pub struct ReadTool {
    cwd: PathBuf,
}

impl ReadTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn label(&self) -> &str {
        "Read"
    }
    fn description(&self) -> &str {
        "Read the contents of a file. Supports text files and images (jpg, png, gif, webp). \
         For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). \
         Use offset/limit for large files."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "path to the file to read (relative or absolute)"
                },
                "offset": {
                    "type": "integer",
                    "description": "line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "integer",
                    "description": "maximum number of lines to read"
                },
                "output": {
                    "type": "string",
                    "description": "output format: 'text' (default) or 'json' with metadata (total_lines, start_line, end_line, truncated)",
                    "enum": ["text", "json"]
                }
            },
            "required": ["path"]
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

            let path = resolve_path(&self.cwd, path_str);
            let offset = args["offset"].as_u64().map(|n| n as usize);
            let limit = args["limit"].as_u64().map(|n| n as usize);
            let json_output = args["output"].as_str() == Some("json");

            tokio::task::spawn_blocking(move || read_file(&path, offset, limit, json_output))
                .await
                .unwrap_or_else(|e| ToolResult::error(format!("task join error: {e}")))
        })
    }
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
}

fn read_file(path: &Path, offset: Option<usize>, limit: Option<usize>, json_output: bool) -> ToolResult {
    if !path.exists() {
        return ToolResult::error(format!("file not found: {}", path.display()));
    }

    if !path.is_file() {
        return ToolResult::error(format!("not a file: {}", path.display()));
    }

    // handle images
    if is_image(path) {
        return read_image(path);
    }

    // read text file
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("failed to read file: {e}")),
    };

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if total_lines == 0 {
        return ToolResult::text(format!("(empty file, {} bytes)", content.len()));
    }

    // apply offset (1-indexed)
    let start = offset.unwrap_or(1).saturating_sub(1).min(total_lines);
    let max_lines = limit.unwrap_or(MAX_LINES).min(MAX_LINES);

    let mut result = String::new();
    let mut bytes_written = 0;
    let mut lines_written = 0;
    let mut truncated = false;

    for (i, line) in lines.iter().enumerate().skip(start) {
        if lines_written >= max_lines || bytes_written >= MAX_BYTES {
            truncated = true;
            break;
        }

        if !result.is_empty() {
            result.push('\n');
            bytes_written += 1;
        }
        result.push_str(line);
        bytes_written += line.len();
        lines_written += 1;

        let _ = i; // used by enumerate for skip
    }

    if truncated {
        let remaining = total_lines - start - lines_written;
        result.push_str(&format!(
            "\n\n[{remaining} more lines in file. Use offset={} to continue.]",
            start + lines_written + 1
        ));
    }

    if json_output {
        let end_line = start + lines_written;
        let json = serde_json::json!({
            "path": path.display().to_string(),
            "total_lines": total_lines,
            "start_line": start + 1,
            "end_line": end_line,
            "truncated": truncated,
            "content": result,
        });
        ToolResult::text(json.to_string())
    } else {
        ToolResult::text(result)
    }
}

fn read_image(path: &Path) -> ToolResult {
    use mush_ai::types::{ImageContent, ToolResultContentPart};

    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => return ToolResult::error(format!("failed to read image: {e}")),
    };

    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&data);

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_lowercase();

    let mime_type = mush_ai::types::ImageMimeType::from_extension(&ext);

    ToolResult {
        content: vec![ToolResultContentPart::Image(ImageContent {
            data: encoded,
            mime_type,
        })],
        outcome: mush_ai::types::ToolOutcome::Success,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;
    use std::fs;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn read_simple_file() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line 1\nline 2\nline 3").unwrap();

        let result = read_file(&file, None, None, false);
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("line 1"));
        assert!(text.contains("line 3"));
    }

    #[test]
    fn read_with_offset() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line 1\nline 2\nline 3\nline 4").unwrap();

        let result = read_file(&file, Some(3), None, false);
        let text = extract_text(&result);
        assert!(!text.contains("line 1"));
        assert!(!text.contains("line 2"));
        assert!(text.contains("line 3"));
        assert!(text.contains("line 4"));
    }

    #[test]
    fn read_with_limit() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        let content: String = (1..=100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&file, &content).unwrap();

        let result = read_file(&file, None, Some(5), false);
        let text = extract_text(&result);
        assert!(text.contains("line 1"));
        assert!(text.contains("line 5"));
        assert!(!text.contains("line 6"));
        assert!(text.contains("more lines in file"));
    }

    #[test]
    fn read_nonexistent_file() {
        let result = read_file(Path::new("/nonexistent/file.txt"), None, None, false);
        assert!(result.outcome.is_error());
    }

    #[test]
    fn read_directory_returns_error() {
        let dir = temp_dir();
        let result = read_file(dir.path(), None, None, false);
        assert!(result.outcome.is_error());
    }

    #[test]
    fn is_image_detection() {
        assert!(is_image(Path::new("photo.jpg")));
        assert!(is_image(Path::new("photo.PNG")));
        assert!(is_image(Path::new("photo.webp")));
        assert!(!is_image(Path::new("code.rs")));
        assert!(!is_image(Path::new("data.json")));
    }

    #[test]
    fn read_empty_file() {
        let dir = temp_dir();
        let file = dir.path().join("empty.txt");
        fs::write(&file, "").unwrap();

        let result = read_file(&file, None, None, false);
        let text = extract_text(&result);
        assert!(text.contains("empty file"));
    }

    #[test]
    fn read_json_output() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line 1\nline 2\nline 3").unwrap();

        let result = read_file(&file, None, None, true);
        let text = extract_text(&result);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["total_lines"], 3);
        assert_eq!(json["start_line"], 1);
        assert_eq!(json["end_line"], 3);
        assert_eq!(json["truncated"], false);
        assert!(json["content"].as_str().unwrap().contains("line 1"));
    }

    #[test]
    fn read_json_output_with_offset() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line 1\nline 2\nline 3\nline 4").unwrap();

        let result = read_file(&file, Some(3), None, true);
        let text = extract_text(&result);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["total_lines"], 4);
        assert_eq!(json["start_line"], 3);
        assert_eq!(json["truncated"], false);
        let content = json["content"].as_str().unwrap();
        assert!(!content.contains("line 1"));
        assert!(content.contains("line 3"));
    }
}
