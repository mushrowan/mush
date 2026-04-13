//! read tool - reads file contents with optional offset/limit

use std::path::{Path, PathBuf};

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;
use thiserror::Error;

use crate::util::resolve_path;

use mush_agent::truncation::{MAX_BYTES, MAX_LINES};

/// headroom so read's hint footer fits within the central truncation limit
const CONTENT_LINE_HEADROOM: usize = 5;
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

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReadOutput {
    #[default]
    Text,
    Json,
}

impl ReadOutput {
    fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    around_line: Option<usize>,
    context_before: Option<usize>,
    context_after: Option<usize>,
    #[serde(default)]
    output: ReadOutput,
}

struct ResolvedReadArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
    output: ReadOutput,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
enum ReadArgsError {
    #[error("end_line requires start_line")]
    EndLineRequiresStartLine,
    #[error("around_line cannot be combined with start_line or end_line")]
    AroundLineConflictsWithSpan,
    #[error("around_line cannot be combined with offset or limit")]
    AroundLineConflictsWithOffsetLimit,
    #[error("start_line cannot be combined with offset")]
    StartLineConflictsWithOffset,
    #[error("context_before/context_after require around_line")]
    ContextRequiresAroundLine,
    #[error("end_line cannot be combined with limit")]
    EndLineConflictsWithLimit,
    #[error("end_line ({end_line}) must be >= start_line ({start_line})")]
    EndBeforeStart { start_line: usize, end_line: usize },
}

enum ReadWindow {
    Offset {
        offset: Option<usize>,
        limit: Option<usize>,
    },
    Span {
        start_line: usize,
        limit: Option<usize>,
    },
    ExactSpan {
        start_line: usize,
        end_line: usize,
    },
    Around {
        around_line: usize,
        context_before: usize,
        context_after: usize,
    },
}

impl ReadWindow {
    fn into_offset_limit(self) -> (Option<usize>, Option<usize>) {
        match self {
            Self::Offset { offset, limit } => (offset, limit),
            Self::Span { start_line, limit } => (Some(start_line), limit),
            Self::ExactSpan {
                start_line,
                end_line,
            } => (Some(start_line), Some(end_line - start_line + 1)),
            Self::Around {
                around_line,
                context_before,
                context_after,
            } => {
                let start = around_line.saturating_sub(context_before).max(1);
                let window = context_before + 1 + context_after;
                (Some(start), Some(window))
            }
        }
    }
}

impl ReadArgs {
    fn resolve(self) -> Result<ResolvedReadArgs, ReadArgsError> {
        let window = match (self.start_line, self.end_line, self.around_line) {
            (_, Some(_), None) if self.start_line.is_none() => {
                return Err(ReadArgsError::EndLineRequiresStartLine);
            }
            (_, _, Some(_)) if self.start_line.is_some() || self.end_line.is_some() => {
                return Err(ReadArgsError::AroundLineConflictsWithSpan);
            }
            (_, _, Some(_)) if self.offset.is_some() || self.limit.is_some() => {
                return Err(ReadArgsError::AroundLineConflictsWithOffsetLimit);
            }
            (_, _, Some(around_line))
                if self.context_before.is_some() || self.context_after.is_some() =>
            {
                ReadWindow::Around {
                    around_line,
                    context_before: self.context_before.unwrap_or(20),
                    context_after: self.context_after.unwrap_or(20),
                }
            }
            (_, _, Some(around_line)) => ReadWindow::Around {
                around_line,
                context_before: 20,
                context_after: 20,
            },
            (Some(_), _, _) if self.offset.is_some() => {
                return Err(ReadArgsError::StartLineConflictsWithOffset);
            }
            (Some(_), _, _) if self.context_before.is_some() || self.context_after.is_some() => {
                return Err(ReadArgsError::ContextRequiresAroundLine);
            }
            (Some(_), Some(_), _) if self.limit.is_some() => {
                return Err(ReadArgsError::EndLineConflictsWithLimit);
            }
            (Some(start_line), Some(end_line), _) if end_line < start_line => {
                return Err(ReadArgsError::EndBeforeStart {
                    start_line,
                    end_line,
                });
            }
            (Some(start_line), Some(end_line), _) => ReadWindow::ExactSpan {
                start_line,
                end_line,
            },
            (Some(start_line), None, _) => ReadWindow::Span {
                start_line,
                limit: self.limit,
            },
            (None, Some(_), _) => return Err(ReadArgsError::EndLineRequiresStartLine),
            (None, None, None) if self.context_before.is_some() || self.context_after.is_some() => {
                return Err(ReadArgsError::ContextRequiresAroundLine);
            }
            (None, None, None) => ReadWindow::Offset {
                offset: self.offset,
                limit: self.limit,
            },
        };

        let (offset, limit) = window.into_offset_limit();
        Ok(ResolvedReadArgs {
            path: self.path,
            offset,
            limit,
            output: self.output,
        })
    }
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

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Head
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let args = match parse_tool_args::<ReadArgs>(args) {
                Ok(args) => args,
                Err(error) => return error,
            };
            let args = match args.resolve() {
                Ok(args) => args,
                Err(error) => return ToolResult::error(error.to_string()),
            };

            let path = resolve_path(&self.cwd, &args.path);
            let json_output = args.output.is_json();

            tokio::task::spawn_blocking(move || {
                read_file(&path, args.offset, args.limit, json_output)
            })
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

fn read_file(
    path: &Path,
    offset: Option<usize>,
    limit: Option<usize>,
    json_output: bool,
) -> ToolResult {
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
    let content_max = MAX_LINES - CONTENT_LINE_HEADROOM;
    let max_lines = limit.unwrap_or(content_max).min(content_max);

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
        let content: String = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
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
        let content: String = (1..=100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
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
        let content: String = (1..=100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
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

    #[test]
    fn read_args_resolve_returns_typed_error() {
        let result = ReadArgs {
            path: "todo.md".into(),
            offset: None,
            limit: None,
            start_line: None,
            end_line: Some(5),
            around_line: None,
            context_before: None,
            context_after: None,
            output: ReadOutput::Text,
        }
        .resolve();

        match result {
            Err(error) => {
                assert_eq!(error, ReadArgsError::EndLineRequiresStartLine);
                assert_eq!(error.to_string(), "end_line requires start_line");
            }
            Ok(_) => panic!("expected read args error"),
        }
    }

    #[test]
    fn large_file_output_within_central_truncation_limits() {
        // read's output for large files must fit within the central
        // truncation limits so the central pass is a no-op and read's
        // semantic hint ("[N more lines. Use offset=X]") survives
        let dir = temp_dir();
        let file = dir.path().join("big.txt");
        let content: String = (1..=5000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&file, &content).unwrap();

        let result = read_file(&file, None, None, false);
        let text = extract_text(&result);

        let line_count = text.lines().count();
        let byte_count = text.len();

        let max_lines = mush_agent::truncation::MAX_LINES;
        let max_bytes = mush_agent::truncation::MAX_BYTES;

        assert!(
            line_count <= max_lines,
            "read output {line_count} lines exceeds central limit {max_lines}, \
             central truncation would strip read's hint"
        );
        assert!(
            byte_count <= max_bytes,
            "read output {byte_count} bytes exceeds central limit {max_bytes}, \
             central truncation would strip read's hint"
        );

        // verify the hint is present
        assert!(text.contains("more lines in file"));
        assert!(text.contains("offset="));
    }
}
