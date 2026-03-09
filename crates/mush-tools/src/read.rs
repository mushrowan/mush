//! read tool - reads file contents with optional offset/limit

use std::path::{Path, PathBuf};

use mush_agent::tool::{AgentTool, ToolResult};

use crate::util::resolve_path;

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;
/// per-line length cap (chars). longer lines silently truncated
const MAX_LINE_CHARS: usize = 500;

const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

/// truncate a line to at most `max` chars
fn truncate_line(line: &str, max: usize) -> &str {
    if line.len() <= max {
        return line;
    }
    // find a char boundary at or before max
    &line[..line.floor_char_boundary(max)]
}

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
        "Read a file's contents with 1-indexed line numbers. Supports text files and images \
         (jpg, png, gif, webp). Images are returned as attachments. Use offset/limit for \
         large files, start_line/end_line for exact spans, or around_line for contextual reads."
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
                "start_line": {
                    "type": "integer",
                    "description": "first line to read (1-indexed). use with end_line for exact spans"
                },
                "end_line": {
                    "type": "integer",
                    "description": "last line to read (1-indexed, inclusive). requires start_line"
                },
                "around_line": {
                    "type": "integer",
                    "description": "centre the view on this line number (1-indexed). uses context_before/context_after for window size"
                },
                "context_before": {
                    "type": "integer",
                    "description": "lines of context before around_line (default 20)"
                },
                "context_after": {
                    "type": "integer",
                    "description": "lines of context after around_line (default 20)"
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
            let json_output = args["output"].as_str() == Some("json");

            let start_line = args["start_line"].as_u64().map(|n| n as usize);
            let end_line = args["end_line"].as_u64().map(|n| n as usize);
            let around_line = args["around_line"].as_u64().map(|n| n as usize);
            let context_before = args["context_before"].as_u64().map(|n| n as usize);
            let context_after = args["context_after"].as_u64().map(|n| n as usize);

            // resolve span params into offset/limit
            let (offset, limit) = match (start_line, end_line, around_line) {
                (_, Some(_), None) if start_line.is_none() => {
                    return ToolResult::error("end_line requires start_line");
                }
                (Some(s), Some(e), _) if e < s => {
                    return ToolResult::error(format!(
                        "end_line ({e}) must be >= start_line ({s})"
                    ));
                }
                (Some(s), Some(e), _) => (Some(s), Some(e - s + 1)),
                (Some(s), None, _) => (Some(s), args["limit"].as_u64().map(|n| n as usize)),
                (_, _, Some(a)) => {
                    let before = context_before.unwrap_or(20);
                    let after = context_after.unwrap_or(20);
                    let start = a.saturating_sub(before);
                    let window = before + 1 + after;
                    (Some(start.max(1)), Some(window))
                }
                _ => {
                    let offset = args["offset"].as_u64().map(|n| n as usize);
                    let limit = args["limit"].as_u64().map(|n| n as usize);
                    (offset, limit)
                }
            };

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

        // format as L{n}: {content} with per-line length cap
        let line_num = i + 1;
        let display_line = truncate_line(line, MAX_LINE_CHARS);
        let formatted = format!("L{line_num}: {display_line}");
        result.push_str(&formatted);
        bytes_written += formatted.len();
        lines_written += 1;
    }

    if truncated {
        let end_line = start + lines_written;
        let next_offset = end_line + 1;
        let remaining = total_lines - end_line;
        result.push_str(&format!(
            "\n\n[{remaining} more lines in file. Use offset={next_offset} to continue]",
        ));
    }

    if json_output {
        let end_line = start + lines_written;
        let meta = std::fs::metadata(path);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta
            .as_ref()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        let mut json = serde_json::json!({
            "path": path.display().to_string(),
            "total_lines": total_lines,
            "start_line": start + 1,
            "end_line": end_line,
            "truncated": truncated,
            "size_bytes": size,
            // content already has L{n}: prefixes
            "content": result,
        });
        if let Some(mtime) = modified {
            json["modified_epoch"] = serde_json::json!(mtime);
        }
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
        // output now has L{n}: prefixes
        assert!(text.contains("L1: line 1"));
        assert!(text.contains("L3: line 3"));
    }

    #[test]
    fn read_with_offset() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line 1\nline 2\nline 3\nline 4").unwrap();

        let result = read_file(&file, Some(3), None, false);
        let text = extract_text(&result);
        assert!(!text.contains("L1:"));
        assert!(!text.contains("L2:"));
        assert!(text.contains("L3: line 3"));
        assert!(text.contains("L4: line 4"));
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
        assert!(text.contains("L1: line 1"));
        assert!(text.contains("L5: line 5"));
        assert!(!text.contains("L6:"));
        assert!(text.contains("95 more lines"));
        assert!(text.contains("offset=6 to continue"));
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

    #[tokio::test]
    async fn read_start_end_window() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        let content: String = (1..=20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        fs::write(&file, &content).unwrap();

        let tool = super::ReadTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": file.to_str().unwrap(),
                "start_line": 5,
                "end_line": 8,
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("L5: line 5"));
        assert!(text.contains("L8: line 8"));
        assert!(!text.contains("L4:"));
        assert!(!text.contains("L9:"));
    }

    #[tokio::test]
    async fn read_around_line_window() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        let content: String = (1..=100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        fs::write(&file, &content).unwrap();

        let tool = super::ReadTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": file.to_str().unwrap(),
                "around_line": 50,
                "context_before": 3,
                "context_after": 3,
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("L47: line 47"));
        assert!(text.contains("L50: line 50"));
        assert!(text.contains("L53: line 53"));
        assert!(!text.contains("L46:"));
        assert!(!text.contains("L54:"));
    }

    #[tokio::test]
    async fn read_around_line_default_context() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        let content: String = (1..=100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        fs::write(&file, &content).unwrap();

        let tool = super::ReadTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": file.to_str().unwrap(),
                "around_line": 50,
            }))
            .await;
        let text = extract_text(&result);
        // default context is 20 before + 20 after = lines 30-70
        assert!(text.contains("L30: line 30"));
        assert!(text.contains("L70: line 70"));
        assert!(!text.contains("L29:"));
        assert!(!text.contains("L71:"));
    }

    #[tokio::test]
    async fn read_end_line_without_start_errors() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line 1\nline 2").unwrap();

        let tool = super::ReadTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": file.to_str().unwrap(),
                "end_line": 5,
            }))
            .await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("requires start_line"));
    }

    #[tokio::test]
    async fn read_end_before_start_errors() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line 1\nline 2\nline 3").unwrap();

        let tool = super::ReadTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "path": file.to_str().unwrap(),
                "start_line": 5,
                "end_line": 2,
            }))
            .await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("must be >="));
    }

    #[test]
    fn read_long_lines_truncated() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        let long_line = "x".repeat(1000);
        fs::write(&file, &long_line).unwrap();

        let result = read_file(&file, None, None, false);
        let text = extract_text(&result);
        // should be capped at MAX_LINE_CHARS (500)
        assert!(text.starts_with("L1: "));
        let content = text.strip_prefix("L1: ").unwrap();
        assert_eq!(content.len(), 500);
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
