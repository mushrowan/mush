use crate::turn_activity::StreamPart;

#[derive(Debug, Clone, Default)]
pub struct StreamingState {
    pub text: String,
    pub thinking: String,
    pub active: bool,
    pub tool_args: String,
    pub last_part: Option<StreamPart>,
    pub streamed_chars: usize,
    text_tw: TypewriterState,
    think_tw: TypewriterState,
}

#[derive(Debug, Clone, Default)]
struct TypewriterState {
    total_chars: usize,
    visible_chars: usize,
    visible_byte_offset: usize,
}

impl TypewriterState {
    const MIN_CHARS_PER_TICK: usize = 1;
    const MAX_CHARS_PER_TICK: usize = 12;

    fn reset(&mut self) {
        self.total_chars = 0;
        self.visible_chars = 0;
        self.visible_byte_offset = 0;
    }

    fn record_push(&mut self, delta_chars: usize) {
        self.total_chars += delta_chars;
    }

    fn advance(&mut self, buf: &str) {
        if self.visible_chars >= self.total_chars {
            return;
        }
        let remaining = self.total_chars - self.visible_chars;
        let step = remaining
            .div_ceil(4)
            .clamp(Self::MIN_CHARS_PER_TICK, Self::MAX_CHARS_PER_TICK);
        let new_visible = (self.visible_chars + step).min(self.total_chars);

        let chars_to_skip = new_visible - self.visible_chars;
        let tail = &buf[self.visible_byte_offset..];
        if let Some((byte_delta, _)) = tail.char_indices().nth(chars_to_skip) {
            self.visible_byte_offset += byte_delta;
        } else {
            self.visible_byte_offset = buf.len();
        }
        self.visible_chars = new_visible;
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
        self.active = true;
        self.text.clear();
        self.thinking.clear();
        self.tool_args.clear();
        self.last_part = None;
        self.streamed_chars = 0;
        self.text_tw.reset();
        self.think_tw.reset();
    }

    pub fn push_text(&mut self, delta: &str) {
        let delta = if self.text.is_empty() {
            delta.trim_start_matches('\n')
        } else {
            delta
        };
        let chars = delta.chars().count();
        self.text_tw.record_push(chars);
        self.text.push_str(delta);
        self.streamed(StreamPart::Text, chars);
    }

    pub fn push_thinking(&mut self, delta: &str) {
        let chars = delta.chars().count();
        self.think_tw.record_push(chars);
        self.thinking.push_str(delta);
        self.streamed(StreamPart::Thinking, chars);
    }

    pub fn push_tool_args(&mut self, delta: &str) {
        self.tool_args.push_str(delta);
        self.streamed(StreamPart::ToolArgs, delta.chars().count());
    }

    fn streamed(&mut self, part: StreamPart, chars: usize) {
        self.last_part = Some(part);
        self.streamed_chars += chars;
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
        self.visible_text().chars().count()
    }

    pub fn advance_typewriter(&mut self) {
        self.text_tw.advance(&self.text);
        self.think_tw.advance(&self.thinking);
    }

    pub fn take(&mut self) -> (String, Option<String>) {
        self.active = false;
        let thinking = if self.thinking.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.thinking))
        };
        let text = std::mem::take(&mut self.text);
        self.tool_args.clear();
        self.last_part = None;
        self.streamed_chars = 0;
        self.text_tw.reset();
        self.think_tw.reset();
        (text, thinking)
    }
}

#[cfg(test)]
pub(crate) fn char_prefix(s: &str, n: usize) -> &str {
    s.char_indices()
        .nth(n)
        .map_or(s, |(byte_pos, _)| &s[..byte_pos])
}
