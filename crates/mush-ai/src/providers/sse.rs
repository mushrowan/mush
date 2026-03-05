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
