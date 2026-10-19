use crate::registry::EventStream;
use crate::stream::StreamEvent;
use crate::types::*;

#[derive(Debug, Clone)]
pub struct SseRawEvent {
    pub event: Option<String>,
    pub data: String,
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

#[must_use]
pub fn safe_preview_text(text: &str, max_chars: usize) -> String {
    preview_text(
        &crate::request_snapshot::redact_secret_text(text),
        max_chars,
    )
}

#[must_use]
pub fn safe_preview_bytes(bytes: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    safe_preview_text(&text, max_chars)
}

#[derive(Default)]
pub struct SseParser {
    chunk_buf: Vec<u8>,
    line_buf: String,
    event_name: Option<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseRawEvent> {
        let Self {
            chunk_buf,
            line_buf,
            event_name,
        } = self;
        let mut search = chunk_buf.len();
        chunk_buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut start = 0;

        while let Some(offset) = chunk_buf[search..].iter().position(|&b| b == b'\n') {
            let end = search + offset;
            let line = String::from_utf8_lossy(&chunk_buf[start..end]);
            start = end + 1;
            search = start;
            let line = line.trim_end_matches('\r');

            if line.is_empty() {
                if !line_buf.is_empty() {
                    events.push(SseRawEvent {
                        event: event_name.take(),
                        data: std::mem::take(line_buf),
                    });
                } else {
                    *event_name = None;
                }
                continue;
            }

            if let Some(rest) = line.strip_prefix("event:") {
                *event_name = Some(rest.trim().to_string());
                continue;
            }

            if let Some(rest) = line.strip_prefix("data:") {
                let data = rest.strip_prefix(' ').unwrap_or(rest);
                if !line_buf.is_empty() {
                    line_buf.push('\n');
                }
                line_buf.push_str(data);
                continue;
            }

            if !line_buf.is_empty() {
                line_buf.push('\n');
            }
            line_buf.push_str(line);
        }

        chunk_buf.drain(..start);
        events
    }
}

pub enum ProcessResult {
    Events(Vec<StreamEvent>),
    Skip,
    Fatal(StreamEvent),
}

pub trait SseProcessor: Send + 'static {
    fn process(&mut self, raw: &SseRawEvent, output: &mut AssistantMessage) -> ProcessResult;

    fn finish(&mut self, output: &mut AssistantMessage);

    fn emit_start(&self) -> bool {
        true
    }

    fn label(&self) -> &'static str;
}

const MAX_CAPTURE_BYTES: usize = 128 * 1024;

#[derive(Default)]
struct RawCapture(Vec<u8>);

impl RawCapture {
    fn push(&mut self, chunk: &[u8]) {
        let take = MAX_CAPTURE_BYTES
            .saturating_sub(self.0.len())
            .min(chunk.len());
        self.0.extend_from_slice(&chunk[..take]);
    }
}

pub fn run_sse_stream(
    response: reqwest::Response,
    model: &Model,
    call_id: u64,
    mut processor: impl SseProcessor,
) -> EventStream {
    let label = processor.label();
    let model_id = model.id.clone();
    let provider_name = model.provider.clone();
    let api = model.api;
    let request_id = response
        .headers()
        .get("request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let event_stream = async_stream::stream! {
        let mut output = AssistantMessage::builder()
            .model(model_id.clone())
            .provider(provider_name.clone())
            .api(api)
            .timestamp_ms(Timestamp::now())
            .maybe_request_id(request_id)
            .build();

        let mut parser = SseParser::new();
        let mut raw_capture = RawCapture::default();
        let mut sse_seq: u64 = 0;

        use futures::TryStreamExt;
        let mut byte_stream = response.bytes_stream();

        if processor.emit_start() {
            yield StreamEvent::Start { partial: output.clone() };
        }

        loop {
            match byte_stream.try_next().await {
                Ok(Some(chunk)) => {
                    raw_capture.push(&chunk);
                    let chunk_len = chunk.len();
                    tracing::trace!(
                        target: "mush_ai::providers::sse::wire",
                        model = %model_id,
                        provider = %provider_name,
                        api = ?api,
                        chunk_len,
                        chunk_preview = %safe_preview_bytes(&chunk, 240),
                        "{label} raw stream chunk"
                    );
                    for raw in parser.push(&chunk) {
                        tracing::trace!(
                            target: "mush_ai::providers::sse::wire",
                            model = %model_id,
                            provider = %provider_name,
                            api = ?api,
                            event_name = raw.event.as_deref().unwrap_or("message"),
                            data_preview = %safe_preview_text(raw.data.trim(), 240),
                            "{label} sse event"
                        );
                        crate::wire_log::log_sse(
                            call_id,
                            sse_seq,
                            raw.event.as_deref().unwrap_or("message"),
                            &raw.data,
                        );
                        sse_seq += 1;
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
                    let raw_capture = raw_capture.0;
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
                        capture_preview = %safe_preview_bytes(&raw_capture, 400),
                        "{label} body stream decode error"
                    );
                    processor.finish(&mut output);
                    output.stop_reason = StopReason::Error;
                    let error = super::format_error_chain(&e);
                    output.error_message = Some(if crate::registry::route_lost(&e) {
                        format!("{}: {error}", crate::registry::NETWORK_LOST)
                    } else {
                        error
                    });
                    crate::wire_log::log_assembled(call_id, &output);
                    yield StreamEvent::Error {
                        reason: StopReason::Error,
                        message: output,
                    };
                    return;
                }
            }
        }

        processor.finish(&mut output);
        crate::wire_log::log_assembled(call_id, &output);
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
    write_decode_snapshot_in(
        &crate::dirs::data_dir().join("stream-errors"),
        model_id,
        provider,
        bytes,
    )
}

fn write_decode_snapshot_in(
    dir: &std::path::Path,
    model_id: &str,
    provider: &str,
    bytes: &[u8],
) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    std::fs::create_dir_all(dir).ok()?;

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let model = model_id.replace(['/', '\\'], "_");
    let path = dir.join(format!("decode-{provider}-{model}-{ts}.bin"));
    crate::private_io::write_private(&path, bytes).ok()?;
    Some(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decode_snapshot_for_a_namespaced_model_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_decode_snapshot_in(dir.path(), "openai/gpt-5", "openrouter", b"\xff")
            .expect("snapshot written");
        assert_eq!(std::path::Path::new(&path).parent(), Some(dir.path()));
        assert_eq!(std::fs::read(&path).unwrap(), b"\xff");
    }

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
    fn byte_by_byte_delivery_matches_one_push() {
        let stream = "event: a\ndata: h\u{e9}llo\r\n\ndata: one\ndata: two\n\nevent: b\ndata: tail\n\ndata: open";
        let whole = SseParser::new().push(stream.as_bytes());
        let mut parser = SseParser::new();
        let split: Vec<_> = stream
            .as_bytes()
            .chunks(1)
            .flat_map(|byte| parser.push(byte))
            .collect();
        let pairs = |events: &[SseRawEvent]| {
            events
                .iter()
                .map(|event| (event.event.clone(), event.data.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(pairs(&split), pairs(&whole));
        assert_eq!(whole.len(), 3);
        assert_eq!(whole[0].data, "h\u{e9}llo");
        assert_eq!(parser.chunk_buf.as_slice(), b"data: open");
    }

    struct Quiet;

    impl SseProcessor for Quiet {
        fn process(&mut self, _raw: &SseRawEvent, _output: &mut AssistantMessage) -> ProcessResult {
            ProcessResult::Skip
        }

        fn finish(&mut self, _output: &mut AssistantMessage) {}

        fn label(&self) -> &'static str {
            "test"
        }
    }

    async fn body_failure(failure: std::io::Error) -> String {
        use futures::StreamExt;
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![Err(failure)];
        let response = http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
            .unwrap();
        let model = crate::models::all_models().swap_remove(0);
        let events: Vec<StreamEvent> =
            run_sse_stream(reqwest::Response::from(response), &model, 0, Quiet)
                .collect()
                .await;
        match events.last() {
            Some(StreamEvent::Error { message, .. }) => {
                message.error_message.clone().unwrap_or_default()
            }
            other => panic!("expected a stream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_body_that_loses_its_route_says_the_network_went_away() {
        use crate::registry::{NETWORK_LOST, StreamErrorKind, classify_stream_error_message};
        let lost = body_failure(std::io::ErrorKind::NetworkUnreachable.into()).await;
        assert!(lost.starts_with(NETWORK_LOST), "{lost}");
        assert_eq!(
            classify_stream_error_message(&lost),
            StreamErrorKind::Unreachable
        );
        let reset = body_failure(std::io::ErrorKind::ConnectionReset.into()).await;
        assert!(!reset.contains(NETWORK_LOST), "{reset}");
        assert_eq!(
            classify_stream_error_message(&reset),
            StreamErrorKind::Transient
        );
    }

    #[test]
    fn buffered_bytes_diagnostic() {
        let mut parser = SseParser::new();
        parser.push(b"data: partial");
        assert!(!parser.chunk_buf.is_empty());
    }

    #[test]
    fn safe_preview_redacts_secret_shaped_values() {
        let preview = safe_preview_text(
            "data: {\"text\":\"Bearer sk-stream-secret-123 and sess-private-token-456\"}",
            200,
        );

        assert!(
            !preview.contains("sk-stream-secret-123"),
            "api key leaked: {preview}"
        );
        assert!(
            !preview.contains("sess-private-token-456"),
            "session token leaked: {preview}"
        );
        assert!(
            preview.contains("<redacted>"),
            "redaction marker missing: {preview}"
        );
    }
}
