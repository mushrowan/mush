use std::cell::{Cell, RefCell};
use std::fmt::Write;

use ratatui::layout::Rect;

use crate::clipboard::ClipboardImage;

mod history;

pub use mush_runtime::PendingImage;

#[derive(Debug)]
pub struct InputBuffer {
    pub text: String,
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub selection_kind: crate::screen::SelectionKind,
    pub images: Vec<PendingImage>,
    pub scroll: Cell<u16>,
    pub total_lines: Cell<u16>,
    pub visible_lines: Cell<u16>,
    pub area: Cell<Rect>,
    pub(crate) layout_cache: RefCell<Option<crate::widgets::input_box::InputLayoutCache>>,
    #[cfg(test)]
    pub(crate) layout_builds: Cell<u32>,
    goal: Option<Goal>,
    history: history::History,
}

#[derive(Debug, Clone, Copy)]
struct Goal {
    cursor: usize,
    len: usize,
    column: usize,
}

impl InputBuffer {
    #[must_use]
    pub fn selected_bytes(&self) -> Option<std::ops::Range<usize>> {
        let anchor = self.anchor?;
        let (from, to) = (anchor.min(self.cursor), anchor.max(self.cursor));
        let to = self.text[to..]
            .chars()
            .next()
            .map_or(self.text.len(), |c| to + c.len_utf8());
        if self.selection_kind == crate::screen::SelectionKind::Line {
            let start = self.text[..from].rfind('\n').map_or(0, |i| i + 1);
            let end = self.text[to..]
                .find('\n')
                .map_or(self.text.len(), |i| to + i);
            return Some(start..end);
        }
        Some(from..to)
    }

    pub fn step_back_one_char(&mut self) {
        if !self.text[..self.cursor].ends_with('\n') {
            self.cursor = self.prev_char_boundary();
        }
    }

    fn prev_char_boundary(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(byte, _)| byte)
    }

    fn next_char_boundary(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .map_or(self.text.len(), |c| self.cursor + c.len_utf8())
    }

    #[must_use]
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            anchor: None,
            selection_kind: crate::screen::SelectionKind::Char,
            images: Vec::new(),
            scroll: Cell::new(0),
            total_lines: Cell::new(0),
            visible_lines: Cell::new(0),
            area: Cell::new(Rect::default()),
            layout_cache: RefCell::new(None),
            #[cfg(test)]
            layout_builds: Cell::new(0),
            goal: None,
            history: history::History::default(),
        }
    }

    pub(crate) fn goal_column(&self) -> Option<usize> {
        self.goal
            .filter(|goal| goal.cursor == self.cursor && goal.len == self.text.len())
            .map(|goal| goal.column)
    }

    pub(crate) fn hold_goal_column(&mut self, column: usize) {
        self.goal = Some(Goal {
            cursor: self.cursor,
            len: self.text.len(),
            column,
        });
    }

    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.ensure_cursor_visible();
    }

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
            let prev = self.prev_char_boundary();
            self.remove_images_in_range(prev, self.cursor);
            self.text.drain(prev..self.cursor);
            self.cursor = prev;
            self.ensure_cursor_visible();
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let next = self.next_char_boundary();
            self.remove_images_in_range(self.cursor, next);
            self.text.drain(self.cursor..next);
            self.ensure_cursor_visible();
        }
    }

    pub fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.prev_char_boundary();
            self.ensure_cursor_visible();
        }
    }

    pub fn cursor_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.next_char_boundary();
            self.ensure_cursor_visible();
        }
    }

    pub fn cursor_up(&mut self) -> bool {
        let Some(line_start) = self.text[..self.cursor].rfind('\n').map(|i| i + 1) else {
            return false;
        };
        let column = self.text[line_start..self.cursor].chars().count();
        let above_start = self.text[..line_start - 1].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = column_offset(&self.text, above_start, line_start - 1, column);
        self.ensure_cursor_visible();
        true
    }

    pub fn cursor_down(&mut self) -> bool {
        let Some(line_end) = self.text[self.cursor..].find('\n').map(|i| self.cursor + i) else {
            return false;
        };
        let line_start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let column = self.text[line_start..self.cursor].chars().count();
        let below_start = line_end + 1;
        let below_end = self.text[below_start..]
            .find('\n')
            .map_or(self.text.len(), |i| below_start + i);
        self.cursor = column_offset(&self.text, below_start, below_end, column);
        self.ensure_cursor_visible();
        true
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

    pub fn take_text(&mut self) -> String {
        self.forget_history();
        self.cursor = 0;
        self.scroll.set(0);
        let input = std::mem::take(&mut self.text);
        let mut n = 0;
        let mut out = String::with_capacity(input.len());
        for ch in input.chars() {
            if ch == IMAGE_PLACEHOLDER {
                n += 1;
                let _ = write!(out, "[image {n}]");
            } else {
                out.push(ch);
            }
        }
        out
    }

    pub fn take_images(&mut self) -> Vec<PendingImage> {
        std::mem::take(&mut self.images)
    }

    pub fn add_image(&mut self, image: ClipboardImage) {
        let dimensions = image::load_from_memory(&image.bytes)
            .ok()
            .map(|img| (img.width(), img.height()));
        self.attach_image(PendingImage {
            data: image.bytes,
            mime_type: image.mime_type,
            dimensions,
            note: None,
        });
    }

    pub fn restore_returned(&mut self, text: &str, images: Vec<PendingImage>) {
        let mut restored = String::with_capacity(text.len());
        let mut rest = text;
        let mut placed = 0;
        for number in 1..=images.len() {
            let token = format!("[image {number}]");
            let Some(at) = rest.find(&token) else {
                break;
            };
            restored.push_str(&rest[..at]);
            restored.push(IMAGE_PLACEHOLDER);
            rest = &rest[at + token.len()..];
            placed += 1;
        }
        restored.push_str(rest);
        for _ in placed..images.len() {
            restored.push(IMAGE_PLACEHOLDER);
        }
        let end = restored.len();
        if !self.text.is_empty() {
            restored.push_str("\n\n");
        }
        self.insert_with_images(0, &restored, images);
        self.cursor = end;
        self.anchor = None;
        self.ensure_cursor_visible();
    }

    pub fn attach_image(&mut self, image: PendingImage) {
        let mut buffer = [0; 4];
        let placeholder = IMAGE_PLACEHOLDER.encode_utf8(&mut buffer);
        self.insert_with_images(self.cursor, placeholder, vec![image]);
        self.cursor += placeholder.len();
        self.ensure_cursor_visible();
    }

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

    pub fn scroll_by(&self, delta: i16) {
        let visible = self.visible_lines.get();
        let total = self.total_lines.get();
        let max_scroll = total.saturating_sub(visible);
        let current = self.scroll.get() as i16;
        let next = (current + delta).clamp(0, max_scroll as i16) as u16;
        self.scroll.set(next);
    }

    fn remove_images_in_range(&mut self, start: usize, end: usize) {
        self.take_images_in(start..end);
    }

    fn image_slots(&self, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
        let placeholders = |text: &str| text.chars().filter(|c| *c == IMAGE_PLACEHOLDER).count();
        let inside = placeholders(&self.text[range.clone()]);
        if inside == 0 {
            return 0..0;
        }
        let prior = placeholders(&self.text[..range.start]).min(self.images.len());
        prior..(prior + inside).min(self.images.len())
    }

    pub(crate) fn images_in(&self, range: std::ops::Range<usize>) -> Vec<PendingImage> {
        self.images[self.image_slots(range)].to_vec()
    }

    pub(crate) fn take_images_in(&mut self, range: std::ops::Range<usize>) -> Vec<PendingImage> {
        let slots = self.image_slots(range);
        self.images.drain(slots).collect()
    }

    pub(crate) fn insert_with_images(&mut self, at: usize, text: &str, images: Vec<PendingImage>) {
        let prior = self.text[..at]
            .chars()
            .filter(|c| *c == IMAGE_PLACEHOLDER)
            .count()
            .min(self.images.len());
        self.images.splice(prior..prior, images);
        self.text.insert_str(at, text);
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

pub const IMAGE_PLACEHOLDER: char = '\u{FFFC}';

fn column_offset(text: &str, start: usize, end: usize, column: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(column)
        .map_or(end, |(offset, _)| start + offset)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub(crate) fn word_boundary_left(s: &str, cursor: usize) -> usize {
    let before = &s[..cursor];
    if before.is_empty() {
        return 0;
    }
    let skip_end = before
        .char_indices()
        .rev()
        .find(|(_, c)| is_word_char(*c))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    if skip_end == 0 {
        return 0;
    }
    before[..skip_end]
        .char_indices()
        .rev()
        .find(|(_, c)| !is_word_char(*c))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0)
}

pub(crate) fn word_boundary_right(s: &str, cursor: usize) -> usize {
    let after = &s[cursor..];
    let mut chars = after.char_indices();
    let word_end = chars
        .by_ref()
        .find(|(_, c)| !is_word_char(*c))
        .map(|(i, _)| i)
        .unwrap_or(after.len());
    let past_ws = after[word_end..]
        .char_indices()
        .find(|(_, c)| is_word_char(*c))
        .map(|(i, _)| word_end + i)
        .unwrap_or(after.len());
    cursor + past_ws
}
