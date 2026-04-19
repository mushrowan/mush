//! input state and helpers

use std::cell::{Cell, RefCell};

use mush_ai::types::ImageMimeType;
use ratatui::layout::Rect;

use crate::clipboard::ClipboardImage;

/// an image attached to the next user message (not yet sent)
#[derive(Debug, Clone)]
pub struct PendingImage {
    pub data: Vec<u8>,
    pub mime_type: ImageMimeType,
    /// image dimensions (width, height) if decoded
    pub dimensions: Option<(u32, u32)>,
}

/// user input buffer with cursor, images, and scroll state
#[derive(Debug)]
pub struct InputBuffer {
    /// the input text
    pub text: String,
    /// cursor byte position in text
    pub cursor: usize,
    /// images attached to the next user message (not yet sent)
    pub images: Vec<PendingImage>,
    /// current input scroll offset (lines from top)
    pub scroll: Cell<u16>,
    /// total wrapped input lines (set during render by InputBox)
    pub total_lines: Cell<u16>,
    /// visible input lines (set during render by InputBox)
    pub visible_lines: Cell<u16>,
    /// latest input area rect (set during render by Ui)
    pub area: Cell<Rect>,
    pub(crate) layout_cache: RefCell<Option<crate::widgets::input_box::InputLayoutCache>>,
    #[cfg(test)]
    pub(crate) layout_builds: Cell<u32>,
}

impl InputBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            images: Vec::new(),
            scroll: Cell::new(0),
            total_lines: Cell::new(0),
            visible_lines: Cell::new(0),
            area: Cell::new(Rect::default()),
            layout_cache: RefCell::new(None),
            #[cfg(test)]
            layout_builds: Cell::new(0),
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.ensure_cursor_visible();
    }

    /// insert a string at the cursor (used for bracketed paste)
    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.ensure_cursor_visible();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.remove_images_in_range(prev, self.cursor);
            self.text.drain(prev..self.cursor);
            self.cursor = prev;
            self.ensure_cursor_visible();
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let next = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.remove_images_in_range(self.cursor, next);
            self.text.drain(self.cursor..next);
            self.ensure_cursor_visible();
        }
    }

    pub fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.ensure_cursor_visible();
        }
    }

    pub fn cursor_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.ensure_cursor_visible();
        }
    }

    pub fn cursor_word_left(&mut self) {
        self.cursor = word_boundary_left(&self.text, self.cursor);
        self.ensure_cursor_visible();
    }

    pub fn cursor_word_right(&mut self) {
        self.cursor = word_boundary_right(&self.text, self.cursor);
        self.ensure_cursor_visible();
    }

    pub fn cursor_home(&mut self) {
        self.cursor = 0;
        self.ensure_cursor_visible();
    }

    pub fn cursor_end(&mut self) {
        self.cursor = self.text.len();
        self.ensure_cursor_visible();
    }

    pub fn delete_word_backward(&mut self) {
        let boundary = word_boundary_left(&self.text, self.cursor);
        self.remove_images_in_range(boundary, self.cursor);
        self.text.drain(boundary..self.cursor);
        self.cursor = boundary;
        self.ensure_cursor_visible();
    }

    pub fn delete_word_forward(&mut self) {
        let boundary = word_boundary_right(&self.text, self.cursor);
        self.remove_images_in_range(self.cursor, boundary);
        self.text.drain(self.cursor..boundary);
        self.ensure_cursor_visible();
    }

    pub fn delete_to_end(&mut self) {
        self.remove_images_in_range(self.cursor, self.text.len());
        self.text.truncate(self.cursor);
        self.ensure_cursor_visible();
    }

    pub fn delete_to_start(&mut self) {
        self.remove_images_in_range(0, self.cursor);
        self.text.drain(..self.cursor);
        self.cursor = 0;
        self.ensure_cursor_visible();
    }

    /// take the input text, clearing the buffer and returning text where
    /// each IMAGE_PLACEHOLDER is replaced by a visible `[image N]` marker
    /// (1-indexed). the markers let the LLM bind "the second image" style
    /// references to the correct attachment even though images are sent
    /// as separate content blocks
    pub fn take_text(&mut self) -> String {
        self.cursor = 0;
        self.scroll.set(0);
        let input = std::mem::take(&mut self.text);
        let mut n = 0;
        let mut out = String::with_capacity(input.len());
        for ch in input.chars() {
            if ch == IMAGE_PLACEHOLDER {
                n += 1;
                out.push_str(&format!("[image {n}]"));
            } else {
                out.push(ch);
            }
        }
        out
    }

    /// take pending images (clearing them from the buffer)
    pub fn take_images(&mut self) -> Vec<PendingImage> {
        std::mem::take(&mut self.images)
    }

    /// add a clipboard image, inserting a placeholder at the cursor
    pub fn add_image(&mut self, image: ClipboardImage) {
        let dimensions = image::load_from_memory(&image.bytes)
            .ok()
            .map(|img| (img.width(), img.height()));
        self.images.push(PendingImage {
            data: image.bytes,
            mime_type: image.mime_type,
            dimensions,
        });
        self.text.insert(self.cursor, IMAGE_PLACEHOLDER);
        self.cursor += IMAGE_PLACEHOLDER.len_utf8();
        self.ensure_cursor_visible();
    }

    /// remove the last pending image (and its placeholder in the input)
    pub fn remove_last_image(&mut self) {
        if self.images.pop().is_some()
            && let Some(pos) = self.text.rfind(IMAGE_PLACEHOLDER)
        {
            let end = pos + IMAGE_PLACEHOLDER.len_utf8();
            self.text.drain(pos..end);
            if self.cursor > pos {
                self.cursor = self.cursor.saturating_sub(IMAGE_PLACEHOLDER.len_utf8());
            }
            self.ensure_cursor_visible();
        }
    }

    /// ensure input scroll keeps the cursor visible
    pub fn ensure_cursor_visible(&self) {
        let content_width = self.area.get().width.saturating_sub(2);
        let visible = self.visible_lines.get();
        if content_width == 0 || visible == 0 {
            self.scroll.set(0);
            return;
        }

        let layout = self.layout(content_width as usize);
        let cursor_line = layout.cursor_visual_line as u16;
        let total = layout.total_lines;
        self.total_lines.set(total);

        let mut current_scroll = self.scroll.get();
        if cursor_line < current_scroll {
            current_scroll = cursor_line;
        } else {
            let bottom = current_scroll.saturating_add(visible.saturating_sub(1));
            if cursor_line > bottom {
                current_scroll = cursor_line.saturating_sub(visible.saturating_sub(1));
            }
        }

        let max_scroll = total.saturating_sub(visible);
        self.scroll.set(current_scroll.min(max_scroll));
    }

    /// scroll the input viewport by delta lines
    pub fn scroll_by(&self, delta: i16) {
        let visible = self.visible_lines.get();
        let total = self.total_lines.get();
        let max_scroll = total.saturating_sub(visible);
        let current = self.scroll.get() as i16;
        let next = (current + delta).clamp(0, max_scroll as i16) as u16;
        self.scroll.set(next);
    }

    /// whether the mouse position is over the input box
    pub fn is_mouse_over(&self, column: u16, row: u16) -> bool {
        self.area
            .get()
            .contains(ratatui::layout::Position::new(column, row))
    }

    /// remove pending images whose placeholders fall within text[start..end]
    fn remove_images_in_range(&mut self, start: usize, end: usize) {
        let range = &self.text[start..end];
        if !range.contains(IMAGE_PLACEHOLDER) {
            return;
        }
        let prior = self.text[..start]
            .chars()
            .filter(|c| *c == IMAGE_PLACEHOLDER)
            .count();
        let count = range.chars().filter(|c| *c == IMAGE_PLACEHOLDER).count();
        for i in (0..count).rev() {
            let idx = prior + i;
            if idx < self.images.len() {
                self.images.remove(idx);
            }
        }
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// object replacement character, marks image positions in input text
/// each occurrence maps to the Nth entry in pending_images (by order)
pub const IMAGE_PLACEHOLDER: char = '\u{FFFC}';

/// find the byte offset of the previous word boundary
pub(crate) fn word_boundary_left(s: &str, cursor: usize) -> usize {
    let before = &s[..cursor];
    if before.is_empty() {
        return 0;
    }
    // skip trailing whitespace and punctuation (non-word chars)
    let skip_end = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphanumeric() || *c == '_')
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    if skip_end == 0 {
        return 0;
    }
    // now find start of the word
    before[..skip_end]
        .char_indices()
        .rev()
        .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0)
}

/// find the byte offset of the next word boundary
pub(crate) fn word_boundary_right(s: &str, cursor: usize) -> usize {
    let after = &s[cursor..];
    // skip current word chars, then skip whitespace
    let mut chars = after.char_indices();
    // skip word chars
    let word_end = chars
        .by_ref()
        .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
        .map(|(i, _)| i)
        .unwrap_or(after.len());
    // skip whitespace/punctuation
    let past_ws = after[word_end..]
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric() || *c == '_')
        .map(|(i, _)| word_end + i)
        .unwrap_or(after.len());
    cursor + past_ws
}
