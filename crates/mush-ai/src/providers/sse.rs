//! shared SSE (server-sent events) parser
//!
//! incrementally parses byte chunks into SSE events.
//! used by all three provider implementations to avoid duplicating
//! the byte-buffer → line-split → event-boundary logic.

/// a parsed SSE event (raw strings, not yet deserialised)
#[derive(Debug, Clone)]
pub struct SseRawEvent {
    /// the event type (from `event:` line), if any
    pub event: Option<String>,
    /// the data payload (from `data:` lines, joined with newlines)
    pub data: String,
}

#[must_use]
pub fn preview_bytes(bytes: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    preview_text(&text, max_chars)
}

#[must_use]
pub fn preview_text(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in text.chars().take(max_chars) {
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

/// incrementally parses SSE from byte chunks
pub struct SseParser {
    chunk_buf: Vec<u8>,
    line_buf: String,
    event_name: Option<String>,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self {
            chunk_buf: Vec::new(),
            line_buf: String::new(),
            event_name: None,
        }
    }

    /// push a chunk of bytes and return any complete SSE events
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseRawEvent> {
        self.chunk_buf.extend_from_slice(bytes);
        let mut events = Vec::new();

        while let Some(newline_pos) = self.chunk_buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.chunk_buf[..newline_pos]).to_string();
            self.chunk_buf.drain(..=newline_pos);
            let line = line.trim_end_matches('\r');

            if line.is_empty() {
                // empty line = end of SSE event
                if !self.line_buf.is_empty() {
                    events.push(SseRawEvent {
                        event: self.event_name.take(),
                        data: std::mem::take(&mut self.line_buf),
                    });
                } else {
                    self.event_name = None;
                }
                continue;
            }

            if let Some(rest) = line.strip_prefix("event:") {
                self.event_name = Some(rest.trim().to_string());
                continue;
            }

            if let Some(rest) = line.strip_prefix("data:") {
                let data = rest.strip_prefix(' ').unwrap_or(rest);
                if !self.line_buf.is_empty() {
                    self.line_buf.push('\n');
                }
                self.line_buf.push_str(data);
                continue;
            }

            // other SSE fields (id:, retry:) or non-prefixed lines
            // anthropic sends raw `data: ...` lines but also lines without prefix
            // when multi-line data is used. buffer them
            if !self.line_buf.is_empty() {
                self.line_buf.push('\n');
            }
            self.line_buf.push_str(line);
        }

        events
    }

    /// access any remaining buffered bytes (for error diagnostics)
    pub fn buffered_bytes(&self) -> &[u8] {
        &self.chunk_buf
    }
}

// shared SSE stream runner

use crate::registry::EventStream;
use crate::stream::StreamEvent;
use crate::types::*;

/// what the processor wants the stream runner to do after processing an event
pub enum ProcessResult {
    /// yield these events to the consumer
    Events(Vec<StreamEvent>),
    /// nothing to yield (e.g. [DONE] or empty payload)
    Skip,
    /// yield this event and stop the stream immediately
    Fatal(StreamEvent),
}

/// trait for provider-specific SSE event processing.
/// the stream runner handles byte reading, SSE parsing, capture, logging,
/// and error recovery. providers just implement event interpretation
pub trait SseProcessor: Send + 'static {
    /// process a single raw SSE event
    fn process(&mut self, raw: &SseRawEvent, output: &mut AssistantMessage) -> ProcessResult;

    /// called when the byte stream ends normally. clean up open blocks,
    /// adjust stop_reason, etc
    fn finish(&mut self, output: &mut AssistantMessage);

    /// whether to emit a Start event before the main loop.
    /// openai providers do this; anthropic emits Start from within process()
    fn emit_start(&self) -> bool {
        true
    }

    /// label for tracing spans (e.g. "anthropic", "openai completions")
    fn label(&self) -> &'static str;
}

const MAX_CAPTURE_BYTES: usize = 128 * 1024;

/// run an SSE stream, handling byte reading, parsing, capture, logging,
/// and error snapshot. the processor handles event interpretation
pub fn run_sse_stream(
    response: reqwest::Response,
    model_id: ModelId,
    provider_name: Provider,
    api: Api,
    mut processor: impl SseProcessor,
) -> EventStream {
    let label = processor.label();
    let event_stream = async_stream::stream! {
        let mut output = AssistantMessage {
            content: vec![],
            model: model_id.clone(),
            provider: provider_name.clone(),
            api,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::now(),
        };

        let mut parser = SseParser::new();
        let mut raw_capture: Vec<u8> = Vec::new();

        use futures::TryStreamExt;
        let mut byte_stream = response.bytes_stream();

        if processor.emit_start() {
            yield StreamEvent::Start { partial: output.clone() };
        }

        loop {
            match byte_stream.try_next().await {
                Ok(Some(chunk)) => {
                    if raw_capture.len() < MAX_CAPTURE_BYTES {
                        let remain = MAX_CAPTURE_BYTES - raw_capture.len();
                        let take = remain.min(chunk.len());
                        raw_capture.extend_from_slice(&chunk[..take]);
                    }
                    let chunk_len = chunk.len();
                    tracing::trace!(
                        model = %model_id,
                        provider = %provider_name,
                        api = ?api,
                        chunk_len,
                        chunk_preview = %preview_bytes(&chunk, 240),
                        "{label} raw stream chunk"
                    );
                    for raw in parser.push(&chunk) {
                        tracing::trace!(
                            model = %model_id,
                            provider = %provider_name,
                            api = ?api,
                            event_name = raw.event.as_deref().unwrap_or("message"),
                            data_preview = %preview_text(raw.data.trim(), 240),
                            "{label} sse event"
                        );
                        match processor.process(&raw, &mut output) {
                            ProcessResult::Events(events) => {
                                for event in events {
                                    yield event;
                                }
                            }
                            ProcessResult::Skip => {}
                            ProcessResult::Fatal(event) => {
                                yield event;
                                return;
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let capture_path = write_decode_snapshot(
                        &model_id.to_string(),
                        &provider_name.to_string(),
                        &raw_capture,
                    );
                    tracing::error!(
                        model = %model_id,
                        provider = %provider_name,
                        api = ?api,
                        error = %e,
                        captured_bytes = raw_capture.len(),
                        capture_path = capture_path.as_deref().unwrap_or("<none>"),
                        capture_preview = %preview_bytes(&raw_capture, 400),
                        "{label} body stream decode error"
                    );
                    processor.finish(&mut output);
                    output.stop_reason = StopReason::Error;
                    output.error_message = Some(super::format_error_chain(&e));
                    yield StreamEvent::Error {
                        reason: StopReason::Error,
                        message: output,
                    };
                    return;
                }
            }
        }

        processor.finish(&mut output);
        yield StreamEvent::Done {
            reason: output.stop_reason,
            message: output,
        };
    };

    Box::pin(event_stream)
}

pub(crate) fn write_decode_snapshot(
    model_id: &str,
    provider: &str,
    bytes: &[u8],
) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }

    let mut dir = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
            let mut p = std::path::PathBuf::from(home);
            p.push(".local/share");
            p
        });
    dir.push("mush");
    dir.push("stream-errors");

    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let file = format!("decode-{provider}-{model_id}-{ts}.bin");
    let path = dir.join(file);
    if crate::private_io::write_private(&path, bytes).is_ok() {
        Some(path.display().to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_event() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
        assert!(events[0].event.is_none());
    }

    #[test]
    fn event_with_type() {
        let mut parser = SseParser::new();
        let events = parser.push(b"event: message\ndata: {\"text\":\"hi\"}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, "{\"text\":\"hi\"}");
    }

    #[test]
    fn multi_line_data() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: line1\ndata: line2\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn chunked_delivery() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: hel").is_empty());
        assert!(parser.push(b"lo\n").is_empty());
        let events = parser.push(b"\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn multiple_events() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: first\n\ndata: second\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "first");
        assert_eq!(events[1].data, "second");
    }

    #[test]
    fn empty_lines_between_events() {
        let mut parser = SseParser::new();
        let events = parser.push(b"\n\ndata: hello\n\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn carriage_return_handling() {
        let mut parser = SseParser::new();
        let events = parser.push(b"data: hello\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn buffered_bytes_diagnostic() {
        let mut parser = SseParser::new();
        parser.push(b"data: partial");
        assert!(!parser.buffered_bytes().is_empty());
    }
}
