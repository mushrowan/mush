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
    /// chars of text currently visible (typewriter effect)
    visible_text_chars: usize,
    /// chars of thinking currently visible (typewriter effect)
    visible_thinking_chars: usize,
}

impl StreamingState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            text: String::new(),
            thinking: String::new(),
            active: false,
            tool_args: String::new(),
            visible_text_chars: 0,
            visible_thinking_chars: 0,
        }
    }

    /// begin streaming a new assistant message
    pub fn start(&mut self) {
        self.active = true;
        self.text.clear();
        self.thinking.clear();
        self.tool_args.clear();
        self.visible_text_chars = 0;
        self.visible_thinking_chars = 0;
    }

    /// visible portion of streaming text (typewriter effect)
    #[must_use]
    pub fn visible_text(&self) -> &str {
        char_prefix(&self.text, self.visible_text_chars)
    }

    /// visible portion of streaming thinking (typewriter effect)
    #[must_use]
    pub fn visible_thinking(&self) -> &str {
        char_prefix(&self.thinking, self.visible_thinking_chars)
    }

    /// advance typewriter: move visible chars towards full buffer
    pub fn advance_typewriter(&mut self) {
        let text_total = self.text.chars().count();
        if self.visible_text_chars < text_total {
            let remaining = text_total - self.visible_text_chars;
            self.visible_text_chars += remaining.div_ceil(2).max(1);
        }
        let think_total = self.thinking.chars().count();
        if self.visible_thinking_chars < think_total {
            let remaining = think_total - self.visible_thinking_chars;
            self.visible_thinking_chars += remaining.div_ceil(2).max(1);
        }
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
        self.visible_text_chars = 0;
        self.visible_thinking_chars = 0;
        (text, thinking)
    }
}

impl Default for StreamingState {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn char_prefix(s: &str, n: usize) -> &str {
    s.char_indices()
        .nth(n)
        .map_or(s, |(byte_pos, _)| &s[..byte_pos])
}
