use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::turn_activity::StreamPart;

pub const REVEAL_WINDOW: Duration = Duration::from_millis(32);

#[derive(Debug, Clone, Default)]
pub struct StreamingState {
    pub text: String,
    pub thinking: String,
    pub active: bool,
    pub tool_args: String,
    pub last_part: Option<StreamPart>,
    pub streamed_chars: usize,
    phase_since: Option<Instant>,
    thinking_since: Option<Instant>,
    thinking_break: bool,
    offline_since: Option<Instant>,
    text_tw: TypewriterState,
    think_tw: TypewriterState,
}

#[derive(Debug, Clone, Default)]
struct TypewriterState {
    total_chars: usize,
    visible_chars: usize,
    visible_byte_offset: usize,
    arrivals: VecDeque<(Instant, usize)>,
}

impl TypewriterState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn record_push(&mut self, delta_chars: usize, now: Instant) {
        if delta_chars == 0 {
            return;
        }
        self.total_chars += delta_chars;
        self.arrivals.push_back((now, self.total_chars));
    }

    fn due(&mut self, now: Instant) -> usize {
        let mut due = self.visible_chars;
        while let Some(&(at, total)) = self.arrivals.front() {
            if now.saturating_duration_since(at) < REVEAL_WINDOW {
                break;
            }
            due = total;
            self.arrivals.pop_front();
        }
        due
    }

    fn advance(&mut self, buf: &str, now: Instant) {
        let due = self.due(now);
        if self.visible_chars >= self.total_chars {
            return;
        }
        let half = (self.total_chars - self.visible_chars).div_ceil(2);
        let new_visible = (self.visible_chars + half).max(due).min(self.total_chars);

        let chars_to_skip = new_visible - self.visible_chars;
        let tail = &buf[self.visible_byte_offset..];
        if let Some((byte_delta, _)) = tail.char_indices().nth(chars_to_skip) {
            self.visible_byte_offset += byte_delta;
        } else {
            self.visible_byte_offset = buf.len();
        }
        self.visible_chars = new_visible;
    }

    fn pending(&self) -> bool {
        self.visible_chars < self.total_chars
    }

    fn visible<'a>(&self, buf: &'a str) -> &'a str {
        &buf[..self.visible_byte_offset.min(buf.len())]
    }
}

impl StreamingState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start(&mut self) {
        self.start_at(Instant::now());
    }

    pub fn start_at(&mut self, now: Instant) {
        self.active = true;
        self.text.clear();
        self.thinking.clear();
        self.tool_args.clear();
        self.last_part = None;
        self.streamed_chars = 0;
        self.phase_since = Some(now);
        self.thinking_since = None;
        self.thinking_break = false;
        self.offline_since = None;
        self.text_tw.reset();
        self.think_tw.reset();
    }

    pub fn network_lost(&mut self, now: Instant) {
        self.offline_since.get_or_insert(now);
    }

    #[must_use]
    pub fn is_offline(&self) -> bool {
        self.offline_since.is_some()
    }

    pub fn network_restored(&mut self, now: Instant) -> Option<Duration> {
        self.offline_since
            .take()
            .map(|since| now.saturating_duration_since(since))
    }

    #[must_use]
    pub fn offline_for(&self, now: Instant) -> Option<Duration> {
        self.offline_since
            .map(|since| now.saturating_duration_since(since))
    }

    pub fn begin(&mut self, part: StreamPart) {
        self.begin_at(part, Instant::now());
    }

    pub fn begin_at(&mut self, part: StreamPart, now: Instant) {
        if part == StreamPart::Thinking && !self.thinking.trim().is_empty() {
            self.thinking_break = true;
        }
        self.enter(part, now);
    }

    pub fn push_text(&mut self, delta: &str) {
        self.push_text_at(delta, Instant::now());
    }

    pub fn push_text_at(&mut self, delta: &str, now: Instant) {
        let delta = if self.text.is_empty() {
            delta.trim_start_matches('\n')
        } else {
            delta
        };
        let chars = delta.chars().count();
        self.text_tw.record_push(chars, now);
        self.text.push_str(delta);
        self.streamed(StreamPart::Text, chars, now);
    }

    pub fn push_thinking(&mut self, delta: &str) {
        self.push_thinking_at(delta, Instant::now());
    }

    pub fn push_thinking_at(&mut self, delta: &str, now: Instant) {
        if self.thinking_break && !delta.trim().is_empty() {
            self.thinking_break = false;
            let separator = if self.thinking.ends_with("\n\n") {
                ""
            } else if self.thinking.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            self.think_tw.record_push(separator.len(), now);
            self.thinking.push_str(separator);
        }
        let chars = delta.chars().count();
        self.think_tw.record_push(chars, now);
        self.thinking.push_str(delta);
        self.streamed(StreamPart::Thinking, chars, now);
    }

    pub fn push_tool_args(&mut self, delta: &str) {
        self.tool_args.push_str(delta);
        self.streamed(StreamPart::ToolArgs, delta.chars().count(), Instant::now());
    }

    fn streamed(&mut self, part: StreamPart, chars: usize, now: Instant) {
        self.enter(part, now);
        self.streamed_chars += chars;
    }

    fn enter(&mut self, part: StreamPart, now: Instant) {
        if part == StreamPart::Thinking {
            self.thinking_since.get_or_insert(now);
        }
        if self.last_part != Some(part) {
            self.last_part = Some(part);
            self.phase_since = Some(now);
        }
    }

    #[must_use]
    pub fn phase_elapsed(&self, now: Instant) -> Option<Duration> {
        self.phase_since
            .map(|since| now.saturating_duration_since(since))
    }

    #[must_use]
    pub fn thinking_elapsed(&self, now: Instant) -> Option<Duration> {
        self.thinking_since
            .map(|since| now.saturating_duration_since(since))
    }

    #[must_use]
    pub fn reply_started(&self) -> bool {
        !self.text.is_empty() || self.last_part == Some(StreamPart::ToolArgs)
    }

    #[must_use]
    pub fn visible_text(&self) -> &str {
        self.text_tw.visible(&self.text)
    }

    #[must_use]
    pub fn visible_thinking(&self) -> &str {
        self.think_tw.visible(&self.thinking)
    }

    #[must_use]
    pub fn visible_text_len(&self) -> usize {
        self.text_tw.visible_chars
    }

    #[must_use]
    pub fn reveal_pending(&self) -> bool {
        self.text_tw.pending() || self.think_tw.pending()
    }

    pub fn advance_typewriter(&mut self) {
        self.advance_typewriter_at(Instant::now());
    }

    pub fn advance_typewriter_at(&mut self, now: Instant) {
        self.text_tw.advance(&self.text, now);
        self.think_tw.advance(&self.thinking, now);
    }

    pub fn take(&mut self) -> (String, Option<String>) {
        self.active = false;
        let thinking = if self.thinking.trim().is_empty() {
            self.thinking.clear();
            None
        } else {
            Some(std::mem::take(&mut self.thinking))
        };
        let text = std::mem::take(&mut self.text);
        self.tool_args.clear();
        self.last_part = None;
        self.streamed_chars = 0;
        self.phase_since = None;
        self.thinking_since = None;
        self.thinking_break = false;
        self.offline_since = None;
        self.text_tw.reset();
        self.think_tw.reset();
        (text, thinking)
    }
}
