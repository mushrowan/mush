use std::path::Path;
use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use mush_ai::types::Api;
use serde::Deserialize;
use thiserror::Error;

use crate::util::{resolve_path, run_blocking};

use mush_agent::truncation::{MAX_BYTES, MAX_LINES};

const CONTENT_LINE_HEADROOM: usize = 5;
const MAX_LINE_CHARS: usize = 500;
const DEFAULT_AROUND_CONTEXT: usize = 20;

const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

fn truncate_line(line: &str, max: usize) -> &str {
    if line.len() <= max {
        return line;
    }
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
    #[serde(alias = "file_path")]
    path: String,
    #[serde(default, deserialize_with = "mush_agent::de::opt_uint")]
    offset: Option<usize>,
    #[serde(default, deserialize_with = "mush_agent::de::opt_uint")]
    limit: Option<usize>,
    #[serde(default, deserialize_with = "mush_agent::de::opt_uint")]
    start_line: Option<usize>,
    #[serde(default, deserialize_with = "mush_agent::de::opt_uint")]
    end_line: Option<usize>,
    #[serde(default, deserialize_with = "mush_agent::de::opt_uint")]
    around_line: Option<usize>,
    #[serde(default, deserialize_with = "mush_agent::de::opt_uint")]
    context_before: Option<usize>,
    #[serde(default, deserialize_with = "mush_agent::de::opt_uint")]
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
            } => (
                Some(start_line),
                Some((end_line - start_line).saturating_add(1)),
            ),
            Self::Around {
                around_line,
                context_before,
                context_after,
            } => {
                let start = around_line.saturating_sub(context_before).max(1);
                let window = context_before
                    .saturating_add(1)
                    .saturating_add(context_after);
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
            (_, _, Some(around_line)) => ReadWindow::Around {
                around_line,
                context_before: self.context_before.unwrap_or(DEFAULT_AROUND_CONTEXT),
                context_after: self.context_after.unwrap_or(DEFAULT_AROUND_CONTEXT),
            },
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
                start_line: start_line.saturating_add(self.offset.unwrap_or(0)),
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
    cwd: Arc<Path>,
    max_image_bytes: usize,
    ledger: crate::read_ledger::ReadLedger,
}

impl ReadTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self {
            cwd,
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
            ledger: crate::read_ledger::ReadLedger::shared(),
        }
    }

    #[must_use]
    pub fn with_read_ledger(mut self, ledger: crate::read_ledger::ReadLedger) -> Self {
        self.ledger = ledger;
        self
    }

    #[must_use]
    pub fn with_max_image_bytes(mut self, max_image_bytes: Option<usize>) -> Self {
        if let Some(max) = max_image_bytes {
            self.max_image_bytes = max;
        }
        self
    }
}

#[async_trait::async_trait]
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
         large files, start_line/end_line for exact spans, or around_line for contextual reads. \
         When start_line and offset are both set, offset is a relative line skip from start_line."
    }

    fn description_for(&self, api: Api) -> &str {
        match api {
            Api::AnthropicMessages => {
                "Read a file's contents with 1-indexed line numbers. Supports text files and \
                 images (jpg, png, gif, webp). Images are returned as attachments. Use offset \
                 to start reading from a given line and limit to cap how many lines are read."
            }
            _ => self.description(),
        }
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
                    "description": "line number to start reading from (1-indexed). when used with start_line, offset is a relative line skip from start_line"
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

    fn parameters_schema_for(&self, api: Api) -> serde_json::Value {
        match api {
            Api::AnthropicMessages => serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
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
                    }
                },
                "required": ["file_path"]
            }),
            _ => self.parameters_schema(),
        }
    }

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Head
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
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
        let max_image_bytes = self.max_image_bytes;
        let ledger = self.ledger.clone();

        run_blocking(move || {
            let result = read_file(&path, args.offset, args.limit, json_output, max_image_bytes);
            if result.outcome.is_success() {
                ledger.record(&path);
            }
            result
        })
        .await
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
    max_image_bytes: usize,
) -> ToolResult {
    if !path.exists() {
        return ToolResult::error(format!("file not found: {}", path.display()));
    }

    if !path.is_file() {
        return ToolResult::error(format!("not a file: {}", path.display()));
    }

    if is_image(path) {
        return read_image(path, max_image_bytes);
    }

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("failed to read file: {e}")),
    };

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if total_lines == 0 {
        return ToolResult::text(format!("(empty file, {} bytes)", content.len()));
    }

    let start = offset.unwrap_or(1).saturating_sub(1);
    if start >= total_lines {
        return ToolResult::error(format!(
            "line {} is past the end of {} ({total_lines} lines)",
            start.saturating_add(1),
            path.display()
        ));
    }
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

pub(crate) const DEFAULT_MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

const MIN_IMAGE_DIM: u32 = 64;

pub(crate) struct ResizedImage {
    pub(crate) bytes: Vec<u8>,
    pub(crate) mime: mush_ai::types::ImageMimeType,
    pub(crate) orig_dimensions: (u32, u32),
    pub(crate) new_dimensions: (u32, u32),
}

pub(crate) fn resize_image_to_fit(data: &[u8], max_bytes: usize) -> Result<ResizedImage, String> {
    const QUALITY: u8 = 85;

    let img = image::load_from_memory(data).map_err(|error| error.to_string())?;
    let orig_dimensions = (img.width(), img.height());
    let mut current = img;

    loop {
        let rgb = current.to_rgb8();
        let (w, h) = rgb.dimensions();
        let bytes = crate::attachment::jpeg(&rgb, QUALITY)?;

        if bytes.len() <= max_bytes || (w <= MIN_IMAGE_DIM && h <= MIN_IMAGE_DIM) {
            return Ok(ResizedImage {
                bytes,
                mime: mush_ai::types::ImageMimeType::Jpeg,
                orig_dimensions,
                new_dimensions: (w, h),
            });
        }

        let nw = (w * 3 / 4).max(1);
        let nh = (h * 3 / 4).max(1);
        current = current.resize(nw, nh, image::imageops::FilterType::Triangle);
    }
}

fn read_image(path: &Path, max_image_bytes: usize) -> ToolResult {
    use mush_ai::types::{ImageContent, TextContent, ToolResultContentPart};

    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => return ToolResult::error(format!("failed to read image: {e}")),
    };

    use base64::Engine;
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);

    if data.len() <= max_image_bytes {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("png")
            .to_lowercase();
        return ToolResult::image(
            encode(&data),
            mush_ai::types::ImageMimeType::from_extension(&ext),
        );
    }

    let mb = |bytes: usize| bytes as f64 / 1_048_576.0;
    match resize_image_to_fit(&data, max_image_bytes) {
        Ok(resized) => {
            let (ow, oh) = resized.orig_dimensions;
            let (nw, nh) = resized.new_dimensions;
            let mut note = format!(
                "[image resized: original {ow}x{oh} ({:.1} MB) exceeded the {:.0} MB limit, \
                 downscaled to {nw}x{nh} ({:.1} MB, jpeg)]",
                mb(data.len()),
                mb(max_image_bytes),
                mb(resized.bytes.len()),
            );
            if let Some(saved) = write_resized_to_temp(path, &resized.bytes) {
                note.push_str(&format!("\n[resized copy saved to {}]", saved.display()));
            }
            ToolResult {
                content: vec![
                    ToolResultContentPart::Text(TextContent { text: note }),
                    ToolResultContentPart::Image(ImageContent {
                        data: encode(&resized.bytes),
                        mime_type: resized.mime,
                    }),
                ],
                outcome: mush_ai::types::ToolOutcome::Success,
                details: None,
            }
        }
        Err(e) => ToolResult::error(format!(
            "image is {:.1} MB, over the {:.0} MB limit, and could not be resized: {e}. \
             not attached.",
            mb(data.len()),
            mb(max_image_bytes),
        )),
    }
}

fn write_resized_to_temp(original: &Path, bytes: &[u8]) -> Option<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};

    let stem = original
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    original.hash(&mut hasher);
    let path = std::env::temp_dir().join(format!("mush-resized-{:x}-{stem}.jpg", hasher.finish()));
    std::fs::write(&path, bytes).ok().map(|()| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;
    use mush_ai::types::Api;
    use std::fs;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn make_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::ImageBuffer::from_fn(w, h, |x, y| {
            image::Rgb([
                (x ^ y) as u8,
                x.wrapping_mul(y) as u8,
                (x.wrapping_add(y)) as u8,
            ])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn resize_image_shrinks_oversized_to_fit() {
        let data = make_png(512, 512);
        let max = 8_000;
        assert!(
            data.len() > max,
            "precondition: test image ({} bytes) must exceed the cap",
            data.len()
        );
        let resized = resize_image_to_fit(&data, max).expect("decodes a valid png");
        assert!(
            resized.bytes.len() <= max,
            "resized to {} bytes, must be <= {max}",
            resized.bytes.len()
        );
        assert_eq!(resized.mime, mush_ai::types::ImageMimeType::Jpeg);
        assert_eq!(resized.orig_dimensions, (512, 512));
        assert!(
            resized.new_dimensions.0 < resized.orig_dimensions.0,
            "must have downscaled from {:?}, got {:?}",
            resized.orig_dimensions,
            resized.new_dimensions
        );
    }

    #[test]
    fn resize_image_errors_on_non_image_bytes() {
        assert!(resize_image_to_fit(b"this is not an image", 1_000).is_err());
    }

    fn image_attachment(result: &ToolResult) -> &mush_ai::types::ImageContent {
        result
            .content
            .iter()
            .find_map(|p| match p {
                mush_ai::types::ToolResultContentPart::Image(img) => Some(img),
                _ => None,
            })
            .expect("an image attachment")
    }

    #[test]
    fn read_file_resizes_oversized_image_and_notes_it() {
        let dir = temp_dir();
        let file = dir.path().join("big.png");
        fs::write(&file, make_png(512, 512)).unwrap();

        let max = 8_000;
        let result = read_file(&file, None, None, false, max);
        assert!(result.outcome.is_success());

        let text = extract_text(&result);
        assert!(
            text.contains("resized"),
            "expected a resize note, got: {text}"
        );

        use base64::Engine;
        let img = image_attachment(&result);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&img.data)
            .unwrap();
        assert!(
            decoded.len() <= max,
            "attached image {} bytes must be <= {max}",
            decoded.len()
        );
        assert_eq!(img.mime_type, mush_ai::types::ImageMimeType::Jpeg);
    }

    #[test]
    fn read_file_attaches_small_image_unchanged() {
        let dir = temp_dir();
        let file = dir.path().join("small.png");
        let data = make_png(16, 16);
        fs::write(&file, &data).unwrap();

        let result = read_file(&file, None, None, false, 10_000_000);
        assert!(result.outcome.is_success());

        use base64::Engine;
        let img = image_attachment(&result);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&img.data)
            .unwrap();
        assert_eq!(decoded, data, "small image must be attached unchanged");
        assert_eq!(img.mime_type, mush_ai::types::ImageMimeType::Png);
    }

    #[test]
    fn anthropic_schema_matches_claude_code_signature() {
        let tool = ReadTool::new(Arc::from(Path::new("/tmp")));
        let schema = tool.parameters_schema_for(Api::AnthropicMessages);
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        let keys: Vec<&str> = props.keys().map(|s| s.as_str()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["file_path", "limit", "offset"]);
        let required = schema.get("required").and_then(|r| r.as_array()).unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].as_str(), Some("file_path"));
    }

    #[test]
    fn non_anthropic_schema_keeps_full_signature() {
        let tool = ReadTool::new(Arc::from(Path::new("/tmp")));
        let schema = tool.parameters_schema_for(Api::OpenaiCompletions);
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        assert!(props.contains_key("path"));
        assert!(props.contains_key("start_line"));
        assert!(props.contains_key("around_line"));
    }

    #[test]
    fn parameters_schema_for_defaults_to_canonical() {
        let tool = ReadTool::new(Arc::from(Path::new("/tmp")));
        let schema = tool.parameters_schema_for(Api::OpenaiResponses);
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        assert!(props.contains_key("path"));
    }

    #[test]
    fn anthropic_description_omits_span_fields() {
        let tool = ReadTool::new(Arc::from(Path::new("/tmp")));
        let anthropic = tool.description_for(Api::AnthropicMessages);
        assert!(!anthropic.contains("start_line"), "got: {anthropic}");
        assert!(!anthropic.contains("end_line"));
        assert!(!anthropic.contains("around_line"));
        assert!(
            tool.description_for(Api::OpenaiCompletions)
                .contains("start_line")
        );
        assert!(tool.description().contains("start_line"));
    }

    #[tokio::test]
    async fn execute_accepts_file_path_alias() {
        let dir = temp_dir();
        let file = dir.path().join("alias.txt");
        fs::write(&file, "hello\nworld").unwrap();
        let tool = ReadTool::new(Arc::from(dir.path()));
        let result = tool
            .execute(serde_json::json!({ "file_path": "alias.txt" }))
            .await;
        assert!(result.outcome.is_success(), "expected success: {result:?}");
        let text = extract_text(&result);
        assert!(text.contains("hello"));
        assert!(text.contains("world"));
    }

    #[test]
    fn read_simple_file() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line 1\nline 2\nline 3").unwrap();

        let result = read_file(&file, None, None, false, DEFAULT_MAX_IMAGE_BYTES);
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("L1: line 1"));
        assert!(text.contains("L3: line 3"));
    }

    #[test]
    fn read_with_offset() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line 1\nline 2\nline 3\nline 4").unwrap();

        let result = read_file(&file, Some(3), None, false, DEFAULT_MAX_IMAGE_BYTES);
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

        let result = read_file(&file, None, Some(5), false, DEFAULT_MAX_IMAGE_BYTES);
        let text = extract_text(&result);
        assert!(text.contains("L1: line 1"));
        assert!(text.contains("L5: line 5"));
        assert!(!text.contains("L6:"));
        assert!(text.contains("95 more lines"));
        assert!(text.contains("offset=6 to continue"));
    }

    #[test]
    fn read_nonexistent_file() {
        let result = read_file(
            Path::new("/nonexistent/file.txt"),
            None,
            None,
            false,
            DEFAULT_MAX_IMAGE_BYTES,
        );
        assert!(result.outcome.is_error());
    }

    #[test]
    fn read_directory_returns_error() {
        let dir = temp_dir();
        let result = read_file(dir.path(), None, None, false, DEFAULT_MAX_IMAGE_BYTES);
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

        let result = read_file(&file, None, None, false, DEFAULT_MAX_IMAGE_BYTES);
        let text = extract_text(&result);
        assert!(text.contains("empty file"));
    }

    #[test]
    fn read_json_output() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line 1\nline 2\nline 3").unwrap();

        let result = read_file(&file, None, None, true, DEFAULT_MAX_IMAGE_BYTES);
        let text = extract_text(&result);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["total_lines"], 3);
        assert_eq!(json["start_line"], 1);
        assert_eq!(json["end_line"], 3);
        assert_eq!(json["truncated"], false);
        assert!(json["content"].as_str().unwrap().contains("line 1"));
    }

    #[tokio::test]
    async fn repeated_reads_do_not_append_session_cumulative_warning() {
        let dir = temp_dir();
        let small = dir.path().join("a.txt");
        let big = dir.path().join("b.txt");
        fs::write(&small, "x\n".repeat(5)).unwrap();
        fs::write(&big, "y\n".repeat(10)).unwrap();

        let tool = ReadTool::new(dir.path().into());

        let r1 = tool.execute(serde_json::json!({"path": "a.txt"})).await;
        let t1 = extract_text(&r1);
        assert!(
            !t1.contains("cumulative"),
            "read output should not include session-wide warnings, got: {t1}"
        );

        let r2 = tool.execute(serde_json::json!({"path": "b.txt"})).await;
        let t2 = extract_text(&r2);
        assert!(
            !t2.contains("cumulative"),
            "read output should not include session-wide warnings, got: {t2}"
        );
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

        let tool = super::ReadTool::new(dir.path().into());
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
    async fn read_accepts_stringified_line_numbers() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        let content: String = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&file, &content).unwrap();

        let tool = super::ReadTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "path": file.to_str().unwrap(),
                "start_line": "5",
                "end_line": "8",
            }))
            .await;
        let text = extract_text(&result);
        assert!(result.outcome.is_success(), "expected success: {text}");
        assert!(text.contains("L5: line 5"));
        assert!(text.contains("L8: line 8"));
        assert!(!text.contains("L4:"));
        assert!(!text.contains("L9:"));
    }

    #[test]
    fn reading_past_the_end_says_how_long_the_file_is() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "one\ntwo\n").unwrap();

        let result = read_file(&file, Some(3), None, false, DEFAULT_MAX_IMAGE_BYTES);

        assert!(result.outcome.is_error());
        assert!(extract_text(&result).contains("(2 lines)"), "{result:?}");
    }

    #[tokio::test]
    async fn read_windows_past_the_largest_line_number_do_not_panic() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "one\ntwo\n").unwrap();
        let tool = super::ReadTool::new(dir.path().into());
        let path = file.to_str().unwrap();

        for args in [
            serde_json::json!({"path": path, "start_line": 2, "offset": usize::MAX}),
            serde_json::json!({"path": path, "start_line": 1, "end_line": usize::MAX}),
            serde_json::json!({"path": path, "around_line": 1, "context_after": usize::MAX}),
        ] {
            let result = tool.execute(args.clone()).await;
            assert!(!extract_text(&result).is_empty(), "{args}");
        }
    }

    #[tokio::test]
    async fn read_start_line_accepts_offset_as_relative_skip() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        let content: String = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&file, &content).unwrap();

        let tool = super::ReadTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "path": file.to_str().unwrap(),
                "start_line": 5,
                "offset": 2,
                "limit": 3,
            }))
            .await;
        let text = extract_text(&result);

        assert!(result.outcome.is_success(), "expected success: {text}");
        assert!(!text.contains("L5:"));
        assert!(!text.contains("L6:"));
        assert!(text.contains("L7: line 7"));
        assert!(text.contains("L9: line 9"));
        assert!(!text.contains("L10:"));
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

        let tool = super::ReadTool::new(dir.path().into());
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

        let tool = super::ReadTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "path": file.to_str().unwrap(),
                "around_line": 50,
            }))
            .await;
        let text = extract_text(&result);
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

        let tool = super::ReadTool::new(dir.path().into());
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

        let tool = super::ReadTool::new(dir.path().into());
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

        let result = read_file(&file, None, None, false, DEFAULT_MAX_IMAGE_BYTES);
        let text = extract_text(&result);
        assert!(text.starts_with("L1: "));
        let content = text.strip_prefix("L1: ").unwrap();
        assert_eq!(content.len(), 500);
    }

    #[test]
    fn read_json_output_with_offset() {
        let dir = temp_dir();
        let file = dir.path().join("test.txt");
        fs::write(&file, "line 1\nline 2\nline 3\nline 4").unwrap();

        let result = read_file(&file, Some(3), None, true, DEFAULT_MAX_IMAGE_BYTES);
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
        let dir = temp_dir();
        let file = dir.path().join("big.txt");
        let content: String = (1..=5000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&file, &content).unwrap();

        let result = read_file(&file, None, None, false, DEFAULT_MAX_IMAGE_BYTES);
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

        assert!(text.contains("more lines in file"));
        assert!(text.contains("offset="));
    }
}
