//! streaming response state and typewriter helpers

/// tracks the in-progress assistant response (text, thinking, tool args)
#[derive(Debug, Clone)]
pub struct StreamingState {
    /// current text being streamed in
    pub text: String,
    /// current thinking being streamed in
    pub thinking: String,
    /// whether we're currently streaming a response
    pub active: bool,
    /// tool args streaming in (partial JSON from ToolCallDelta)
    pub tool_args: String,
    /// typewriter state for text
    text_tw: TypewriterState,
    /// typewriter state for thinking
    think_tw: TypewriterState,
}

/// tracks char/byte offsets for O(1) visible slicing
#[derive(Debug, Clone, Default)]
struct TypewriterState {
    /// total chars in the buffer (updated on push)
    total_chars: usize,
    /// chars currently visible
    visible_chars: usize,
    /// byte offset corresponding to visible_chars (avoids O(n) scan)
    visible_byte_offset: usize,
}

impl TypewriterState {
    fn reset(&mut self) {
        self.total_chars = 0;
        self.visible_chars = 0;
        self.visible_byte_offset = 0;
    }

    /// record that `delta` chars were appended to the buffer
    fn record_push(&mut self, delta_chars: usize) {
        self.total_chars += delta_chars;
    }

    /// advance visible position towards total, return new visible_chars
    fn advance(&mut self, buf: &str) {
        if self.visible_chars >= self.total_chars {
            return;
        }
        let remaining = self.total_chars - self.visible_chars;
        let step = remaining.div_ceil(2).max(1);
        let new_visible = (self.visible_chars + step).min(self.total_chars);

        // walk forward from current byte offset to find new boundary
        let chars_to_skip = new_visible - self.visible_chars;
        let tail = &buf[self.visible_byte_offset..];
        if let Some((byte_delta, _)) = tail.char_indices().nth(chars_to_skip) {
            self.visible_byte_offset += byte_delta;
        } else {
            // past the end, show everything
            self.visible_byte_offset = buf.len();
        }
        self.visible_chars = new_visible;
    }

    /// get the visible slice of `buf`
    fn visible<'a>(&self, buf: &'a str) -> &'a str {
        &buf[..self.visible_byte_offset.min(buf.len())]
    }
}

impl StreamingState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            text: String::new(),
            thinking: String::new(),
            active: false,
            tool_args: String::new(),
            text_tw: TypewriterState::default(),
            think_tw: TypewriterState::default(),
        }
    }

    /// begin streaming a new assistant message
    pub fn start(&mut self) {
        self.active = true;
        self.text.clear();
        self.thinking.clear();
        self.tool_args.clear();
        self.text_tw.reset();
        self.think_tw.reset();
    }

    /// append text delta (tracks char count incrementally)
    pub fn push_text(&mut self, delta: &str) {
        self.text_tw.record_push(delta.chars().count());
        self.text.push_str(delta);
    }

    /// append thinking delta (tracks char count incrementally)
    pub fn push_thinking(&mut self, delta: &str) {
        self.think_tw.record_push(delta.chars().count());
        self.thinking.push_str(delta);
    }

    /// visible portion of streaming text (typewriter effect)
    #[must_use]
    pub fn visible_text(&self) -> &str {
        self.text_tw.visible(&self.text)
    }

    /// visible portion of streaming thinking (typewriter effect)
    #[must_use]
    pub fn visible_thinking(&self) -> &str {
        self.think_tw.visible(&self.thinking)
    }

    /// advance typewriter: move visible chars towards full buffer
    pub fn advance_typewriter(&mut self) {
        self.text_tw.advance(&self.text);
        self.think_tw.advance(&self.thinking);
    }

    /// finish streaming: take the text and thinking, deactivate
    pub fn take(&mut self) -> (String, Option<String>) {
        self.active = false;
        let thinking = if self.thinking.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.thinking))
        };
        let text = std::mem::take(&mut self.text);
        self.tool_args.clear();
        self.text_tw.reset();
        self.think_tw.reset();
        (text, thinking)
    }
}

impl Default for StreamingState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
pub(crate) fn char_prefix(s: &str, n: usize) -> &str {
    s.char_indices()
        .nth(n)
        .map_or(s, |(byte_pos, _)| &s[..byte_pos])
}
