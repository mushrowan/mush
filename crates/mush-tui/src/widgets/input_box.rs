//! input box widget - text entry with cursor

use std::cell::Ref;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Widget};
use unicode_width::UnicodeWidthChar;

use crate::app::{App, InputBuffer, PendingImage};

/// display width of a char (0 for control chars, 2 for CJK/emoji, 1 otherwise)
fn char_width(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

/// expanded input with image placeholders replaced by display tokens
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedInput {
    pub text: String,
    pub cursor: usize,
    /// (start, end) byte ranges in `text` that are image tokens
    pub image_spans: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputLayoutKey {
    text: String,
    cursor: usize,
    image_dimensions: Vec<Option<(u32, u32)>>,
    content_width: usize,
}

impl InputLayoutKey {
    fn from_input(input: &InputBuffer, content_width: usize) -> Self {
        Self {
            text: input.text.clone(),
            cursor: input.cursor,
            image_dimensions: input.images.iter().map(|image| image.dimensions).collect(),
            content_width,
        }
    }

    fn matches(&self, input: &InputBuffer, content_width: usize) -> bool {
        self.content_width == content_width
            && self.cursor == input.cursor
            && self.text == input.text
            && self
                .image_dimensions
                .iter()
                .copied()
                .eq(input.images.iter().map(|image| image.dimensions))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InputLayoutCache {
    key: InputLayoutKey,
    layout: InputLayout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WrappedInputLine {
    pub start: usize,
    pub end: usize,
    pub show_prompt: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct InputLayout {
    pub expanded: ExpandedInput,
    pub wrapped_lines: Vec<WrappedInputLine>,
    pub total_lines: u16,
    pub cursor_visual_line: usize,
    pub cursor_visual_col: usize,
}

impl InputBuffer {
    pub(crate) fn layout(&self, content_width: usize) -> Ref<'_, InputLayout> {
        let needs_rebuild = self
            .layout_cache
            .borrow()
            .as_ref()
            .is_none_or(|cache| !cache.key.matches(self, content_width));

        if needs_rebuild {
            #[cfg(test)]
            self.layout_builds.set(self.layout_builds.get() + 1);

            let key = InputLayoutKey::from_input(self, content_width);
            let layout = build_input_layout(&self.text, self.cursor, &self.images, content_width);
            *self.layout_cache.borrow_mut() = Some(InputLayoutCache { key, layout });
        }

        Ref::map(self.layout_cache.borrow(), |cache| {
            if let Some(cache) = cache.as_ref() {
                &cache.layout
            } else {
                unreachable!("layout cache should be populated")
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn reset_layout_builds(&self) {
        self.layout_builds.set(0);
        self.layout_cache.borrow_mut().take();
    }

    #[cfg(test)]
    pub(crate) fn layout_builds(&self) -> u32 {
        self.layout_builds.get()
    }
}

fn build_input_layout(
    input: &str,
    cursor: usize,
    images: &[PendingImage],
    content_width: usize,
) -> InputLayout {
    let expanded = expand_input(input, cursor, images);
    let (cursor_visual_line, cursor_visual_col) =
        cursor_visual_position(&expanded.text, expanded.cursor, content_width);
    let mut wrapped_lines = Vec::new();
    let mut global_offset = 0usize;

    for (line_idx, line_text) in expanded.text.split('\n').enumerate() {
        let indent = if line_idx == 0 { 2 } else { 0 };
        let segments = word_wrap_segments(line_text, content_width, indent);

        for (seg_idx, seg) in segments.iter().enumerate() {
            wrapped_lines.push(WrappedInputLine {
                start: global_offset + seg.start,
                end: global_offset + seg.end,
                show_prompt: line_idx == 0 && seg_idx == 0,
            });
        }

        global_offset += line_text.len() + 1;
    }

    let total_lines = wrapped_lines.len().max(1).min(u16::MAX as usize) as u16;

    InputLayout {
        expanded,
        wrapped_lines,
        total_lines,
        cursor_visual_line,
        cursor_visual_col,
    }
}

/// format an image token for inline display
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputLayoutSummary {
    pub total_lines: u16,
    pub cursor_visual_line: usize,
    pub cursor_visual_col: usize,
    pub image_span_count: usize,
}

#[doc(hidden)]
pub fn benchmark_build_input_layout(
    input: &str,
    cursor: usize,
    images: &[PendingImage],
    content_width: usize,
) -> InputLayoutSummary {
    let layout = build_input_layout(input, cursor, images, content_width);
    InputLayoutSummary {
        total_lines: layout.total_lines,
        cursor_visual_line: layout.cursor_visual_line,
        cursor_visual_col: layout.cursor_visual_col,
        image_span_count: layout.expanded.image_spans.len(),
    }
}

fn image_token(img: Option<&crate::app::PendingImage>) -> String {
    match img.and_then(|i| i.dimensions) {
        Some((w, h)) => format!("[📷 {w}x{h}]"),
        None => "[📷]".to_string(),
    }
}

/// expand image placeholders in input to display tokens, mapping cursor
pub fn expand_input(
    input: &str,
    cursor: usize,
    images: &[crate::app::PendingImage],
) -> ExpandedInput {
    use crate::app::IMAGE_PLACEHOLDER;
    let mut text = String::with_capacity(input.len());
    let mut display_cursor = 0;
    let mut img_idx = 0;
    let mut image_spans = Vec::new();
    let mut cursor_mapped = false;

    for (byte_pos, ch) in input.char_indices() {
        if byte_pos == cursor {
            display_cursor = text.len();
            cursor_mapped = true;
        }
        if ch == IMAGE_PLACEHOLDER {
            let start = text.len();
            let token = image_token(images.get(img_idx));
            text.push_str(&token);
            image_spans.push((start, text.len()));
            img_idx += 1;
        } else {
            text.push(ch);
        }
    }
    if !cursor_mapped {
        display_cursor = text.len();
    }

    ExpandedInput {
        text,
        cursor: display_cursor,
        image_spans,
    }
}

/// a segment of text that fits on one visual line
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapSegment {
    /// byte offset of start within the input line
    pub start: usize,
    /// byte offset of end (exclusive) within the input line
    pub end: usize,
    /// visual column width this segment occupies
    pub cols: usize,
}

/// compute word-wrapped segments for a single input line
///
/// breaks at whitespace boundaries when possible, falls back to
/// character-level wrapping for words longer than the available width.
/// `indent` is the column offset for the first segment (e.g. 2 for "> ")
pub fn word_wrap_segments(text: &str, width: usize, indent: usize) -> Vec<WrapSegment> {
    if text.is_empty() {
        return vec![WrapSegment {
            start: 0,
            end: 0,
            cols: indent,
        }];
    }

    let mut segments = Vec::new();
    let mut seg_start: usize = 0;
    let mut col: usize = indent;
    // (byte_offset_after_space, col_after_space) - where we'd resume on a new line
    let mut last_space: Option<(usize, usize)> = None;

    for (byte_pos, ch) in text.char_indices() {
        let ch_bytes = ch.len_utf8();
        let ch_cols = char_width(ch);

        if ch == ' ' {
            col += ch_cols;
            // record the position AFTER this space as a potential wrap point
            last_space = Some((byte_pos + ch_bytes, col));

            if col >= width {
                // space itself hits the boundary - wrap here
                segments.push(WrapSegment {
                    start: seg_start,
                    end: byte_pos + ch_bytes,
                    cols: col,
                });
                seg_start = byte_pos + ch_bytes;
                col = 0;
                last_space = None;
            }
        } else {
            col += ch_cols;
            if col >= width {
                if let Some((sp_byte, sp_col)) = last_space.take() {
                    // wrap at last space
                    segments.push(WrapSegment {
                        start: seg_start,
                        end: sp_byte,
                        cols: sp_col,
                    });
                    seg_start = sp_byte;
                    col -= sp_col;
                } else {
                    // no space found, hard wrap at this character boundary
                    let seg_end = byte_pos + ch_bytes;
                    segments.push(WrapSegment {
                        start: seg_start,
                        end: seg_end,
                        cols: col,
                    });
                    seg_start = seg_end;
                    col -= width;
                }
            }
        }
    }

    // final segment
    segments.push(WrapSegment {
        start: seg_start,
        end: text.len(),
        cols: col,
    });

    segments
}

/// compute cursor visual position in wrapped lines
fn cursor_visual_position(
    expanded_text: &str,
    cursor: usize,
    content_width: usize,
) -> (usize, usize) {
    let text_before_cursor = &expanded_text[..cursor];
    let input_lines: Vec<&str> = expanded_text.split('\n').collect();
    let cursor_lines: Vec<&str> = text_before_cursor.split('\n').collect();
    let cursor_input_line = cursor_lines.len() - 1;
    let cursor_in_line = cursor_lines.last().map(|s| s.len()).unwrap_or(0);

    let mut visual_line: usize = 0;
    let mut visual_col: usize = 0;

    for (line_idx, line_text) in input_lines.iter().enumerate() {
        let indent = if line_idx == 0 { 2 } else { 0 };
        let segments = word_wrap_segments(line_text, content_width, indent);

        if line_idx == cursor_input_line {
            for (seg_i, seg) in segments.iter().enumerate() {
                let is_last_seg = seg_i == segments.len() - 1;
                if cursor_in_line < seg.end || (is_last_seg && cursor_in_line <= seg.end) {
                    let seg_text = &line_text[seg.start..cursor_in_line];
                    let visual_offset: usize = seg_text.chars().map(char_width).sum();
                    let seg_indent = if seg_i == 0 { indent } else { 0 };
                    visual_col = seg_indent + visual_offset;
                    break;
                }
                visual_line += 1;
            }
            break;
        }

        visual_line += segments.len();
    }

    (visual_line, visual_col)
}

/// visual wrapped line index for the cursor
pub fn cursor_visual_line(expanded_text: &str, cursor: usize, content_width: usize) -> usize {
    cursor_visual_position(expanded_text, cursor, content_width).0
}

/// renders the input box at the bottom of the screen
pub struct InputBox<'a> {
    app: &'a App,
}

impl<'a> InputBox<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// the cursor position within the input box (for terminal cursor placement)
    pub fn cursor_position(&self, area: Rect) -> (u16, u16) {
        let content_width = area.width.saturating_sub(2) as usize;
        if content_width == 0 {
            return (area.x + 1, area.y + 1);
        }

        let layout = self.app.input.layout(content_width);
        let visual_line = layout.cursor_visual_line;
        let visual_col = layout.cursor_visual_col;

        let scroll = self.app.input.scroll.get() as usize;
        let visible_line = visual_line.saturating_sub(scroll);

        let x = area.x + 1 + visual_col as u16;
        let y = area.y + 1 + visible_line as u16;
        (
            x.min(area.x + area.width - 2),
            y.min(area.y + area.height - 2),
        )
    }
}

impl Widget for InputBox<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let streaming_idle = self.app.is_busy() && self.app.input.text.is_empty();
        // 2 cols to match `"> "` so the cursor column doesn't jump when the
        // stream starts or ends. `… ` = 1 cell ellipsis + 1 trailing space
        let prompt = if streaming_idle { "… " } else { "> " };
        let style = if streaming_idle {
            self.app.theme.dim
        } else {
            Style::default()
        };

        let prompt_style = if streaming_idle {
            self.app.theme.dim
        } else {
            self.app.theme.input_border_active
        };

        let content_width = area.width.saturating_sub(2) as usize;

        // border colour signals whose turn it is and whether there are unread messages
        let border_style = if self.app.navigation.has_unread && !streaming_idle {
            if self.app.input.text.is_empty() {
                // empty input + unread: blink between active and highlight
                if self.app.unread_flash_on() {
                    self.app.theme.scroll_indicator
                } else {
                    self.app.theme.input_border_active
                }
            } else {
                // typing + unread: solid highlight
                self.app.theme.scroll_indicator
            }
        } else if self.app.is_busy() {
            // agent's turn: muted border
            self.app.theme.input_border
        } else {
            // our turn, no unread: normal
            self.app.theme.input_border_active
        };

        let block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .padding(Padding::horizontal(1))
            .border_style(border_style);

        if content_width == 0 {
            self.app.input.visible_lines.set(0);
            self.app.input.total_lines.set(0);
            self.app.input.scroll.set(0);
            Paragraph::new("").block(block).render(area, buf);
            return;
        }

        let layout = self.app.input.layout(content_width);
        let mut lines: Vec<Line<'_>> = Vec::new();
        let image_style = self.app.theme.image_label;

        for (line_idx, line) in layout.wrapped_lines.iter().enumerate() {
            let mut spans: Vec<Span<'_>> = Vec::new();

            if line.show_prompt {
                spans.push(Span::styled(prompt.to_string(), prompt_style));
            }

            let seg_text = &layout.expanded.text[line.start..line.end];
            if !seg_text.is_empty() {
                spans.extend(styled_with_images(
                    seg_text,
                    line.start,
                    &layout.expanded.image_spans,
                    style,
                    image_style,
                ));
            }

            if line_idx == layout.wrapped_lines.len() - 1
                && let Some(ghost) = self.app.ghost_text()
            {
                spans.push(Span::styled(ghost.to_string(), self.app.theme.dim));
            }

            lines.push(Line::from(spans));
        }

        let total_lines = layout.total_lines;
        let visible_lines = area.height.saturating_sub(2);
        self.app.input.total_lines.set(total_lines);
        self.app.input.visible_lines.set(visible_lines);

        let max_scroll = total_lines.saturating_sub(visible_lines);
        let scroll = self.app.input.scroll.get().min(max_scroll);
        self.app.input.scroll.set(scroll);

        let text = ratatui::text::Text::from(lines);
        Paragraph::new(text)
            .block(block)
            .scroll((scroll, 0))
            .render(area, buf);
    }
}

/// split segment text at image token boundaries and style them differently
fn styled_with_images(
    seg_text: &str,
    seg_global_start: usize,
    image_spans: &[(usize, usize)],
    text_style: Style,
    image_style: Style,
) -> Vec<Span<'static>> {
    let seg_global_end = seg_global_start + seg_text.len();

    let has_overlap = image_spans
        .iter()
        .any(|(s, e)| *e > seg_global_start && *s < seg_global_end);

    if !has_overlap {
        return vec![Span::styled(seg_text.to_string(), text_style)];
    }

    let mut spans = Vec::new();
    let mut pos = 0usize;

    for &(img_start, img_end) in image_spans {
        if img_end <= seg_global_start || img_start >= seg_global_end {
            continue;
        }
        let local_start = img_start
            .saturating_sub(seg_global_start)
            .min(seg_text.len());
        let local_end = (img_end - seg_global_start).min(seg_text.len());

        if local_start > pos {
            spans.push(Span::styled(
                seg_text[pos..local_start].to_string(),
                text_style,
            ));
        }
        if local_end > local_start {
            spans.push(Span::styled(
                seg_text[local_start..local_end].to_string(),
                image_style,
            ));
        }
        pos = local_end;
    }
    if pos < seg_text.len() {
        spans.push(Span::styled(seg_text[pos..].to_string(), text_style));
    }
    if spans.is_empty() {
        spans.push(Span::styled(seg_text.to_string(), text_style));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::TokenCount;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn word_wrap_no_wrap_needed() {
        let segs = word_wrap_segments("hello", 20, 2);
        assert_eq!(segs.len(), 1);
        assert_eq!(
            segs[0],
            WrapSegment {
                start: 0,
                end: 5,
                cols: 7
            }
        );
    }

    #[test]
    fn word_wrap_at_space() {
        // "hello world" with width=10, indent=2
        // "  hello " = 8 cols, then "world" = 5 cols
        let segs = word_wrap_segments("hello world", 10, 2);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs[0].end, 6); // "hello " (includes trailing space)
        assert_eq!(segs[1].start, 6);
        assert_eq!(segs[1].end, 11); // "world"
    }

    #[test]
    fn word_wrap_long_word_hard_break() {
        // no spaces, falls back to character wrapping
        let segs = word_wrap_segments("abcdefghij", 6, 0);
        assert_eq!(segs.len(), 2);
        assert_eq!(
            segs[0],
            WrapSegment {
                start: 0,
                end: 6,
                cols: 6
            }
        );
        assert_eq!(
            segs[1],
            WrapSegment {
                start: 6,
                end: 10,
                cols: 4
            }
        );
    }

    #[test]
    fn word_wrap_empty() {
        let segs = word_wrap_segments("", 20, 2);
        assert_eq!(segs.len(), 1);
        assert_eq!(
            segs[0],
            WrapSegment {
                start: 0,
                end: 0,
                cols: 2
            }
        );
    }

    #[test]
    fn word_wrap_multiple_lines() {
        // "the quick brown fox" with width=12, indent=2
        // line 0: "  the quick " (12 cols) -> wrap
        // line 1: "brown fox" (9 cols)
        let segs = word_wrap_segments("the quick brown fox", 12, 2);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].end, 10); // "the quick "
        assert_eq!(segs[1].start, 10);
        assert_eq!(segs[1].end, 19);
    }

    #[test]
    fn word_wrap_multibyte_chars() {
        // ¬ is U+00AC: 2 bytes in UTF-8 but 1 display column
        // "a¬b" = 3 display columns, 4 bytes
        let segs = word_wrap_segments("a¬b", 20, 0);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].cols, 3); // 3 display columns, not 4 bytes
    }

    #[test]
    fn cursor_position_multibyte_char() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        // "a¬b" - cursor after ¬ (byte offset 3, but visual col 2)
        app.input.text = "a¬b".into();
        app.input.cursor = 3; // byte offset after ¬
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 10, 40, 3);
        let (x, _y) = input_box.cursor_position(area);
        // col = 2 (prompt "> ") + 2 (display width of "a¬") = 4
        // x = border(1) + 4 = 5
        assert_eq!(x, 5);
    }

    #[test]
    fn expand_input_no_images() {
        let expanded = expand_input("hello", 3, &[]);
        assert_eq!(expanded.text, "hello");
        assert_eq!(expanded.cursor, 3);
        assert!(expanded.image_spans.is_empty());
    }

    #[test]
    fn expand_input_with_image() {
        use crate::app::{IMAGE_PLACEHOLDER, PendingImage};
        use mush_ai::types::ImageMimeType;
        let img = PendingImage {
            data: vec![],
            mime_type: ImageMimeType::Png,
            dimensions: Some((100, 200)),
        };
        let input = format!("hi{IMAGE_PLACEHOLDER}world");
        let cursor = 2 + IMAGE_PLACEHOLDER.len_utf8(); // after the placeholder
        let expanded = expand_input(&input, cursor, &[img]);
        // should expand to "hi[📷 100x200]world"
        assert!(expanded.text.starts_with("hi["));
        assert!(expanded.text.contains("100x200"));
        assert!(expanded.text.ends_with("world"));
        // cursor should be mapped past the expanded token
        assert!(expanded.cursor > 2);
        assert_eq!(&expanded.text[expanded.cursor..], "world");
        assert_eq!(expanded.image_spans.len(), 1);
    }

    #[test]
    fn backspace_removes_image_placeholder() {
        use crate::app::IMAGE_PLACEHOLDER;
        use crate::clipboard::ClipboardImage;
        use mush_ai::types::ImageMimeType;
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "hello".into();
        app.input.cursor = 5;
        // simulate pasting an image (inserts placeholder at cursor)
        app.input.add_image(ClipboardImage {
            bytes: vec![],
            mime_type: ImageMimeType::Png,
        });
        assert_eq!(app.input.images.len(), 1);
        assert!(app.input.text.contains(IMAGE_PLACEHOLDER));
        // backspace should remove the placeholder and the image
        app.input.backspace();
        assert_eq!(app.input.images.len(), 0);
        assert!(!app.input.text.contains(IMAGE_PLACEHOLDER));
        assert_eq!(app.input.text, "hello");
    }

    #[test]
    fn input_box_renders_empty() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("> "));
    }

    #[test]
    fn input_box_has_no_side_borders() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let buf = render_input(&app, 40, 3);
        // top and bottom should be horizontal rules
        let top_row: String = (0..40).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        let bot_row: String = (0..40).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert!(
            top_row.contains("─"),
            "top should be horizontal rule: {top_row}"
        );
        assert!(
            bot_row.contains("─"),
            "bottom should be horizontal rule: {bot_row}"
        );
        // sides should NOT have │ border characters
        let left_mid = buf[(0, 1)].symbol();
        let right_mid = buf[(39, 1)].symbol();
        assert_ne!(left_mid, "│", "left side should not have border");
        assert_ne!(right_mid, "│", "right side should not have border");
    }

    #[test]
    fn input_box_renders_text() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "hello world".into();
        app.input.cursor = 11;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("hello world"));
    }

    #[test]
    fn input_box_streaming_shows_dots_when_empty() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.stream.active = true;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("…"));
    }

    #[test]
    fn input_box_streaming_shows_prompt_when_typing() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.stream.active = true;
        app.input.text = "hold on".into();
        app.input.cursor = 7;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("> hold on"));
        assert!(!content.contains("…"));
    }

    #[test]
    fn cursor_position_calculation() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "hello".into();
        app.input.cursor = 3;
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 10, 40, 3);
        let (x, y) = input_box.cursor_position(area);
        // col = 2 (prompt) + 3 = 5
        assert_eq!(x, 6); // border + col
        assert_eq!(y, 11); // border
    }

    #[test]
    fn cursor_position_wraps_at_word_boundary() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        // "this is a long prompt that wraps around!!!" in a 20-wide box
        // content_width = 18
        app.input.text = "this is a long prompt that wraps around!!!".into();
        app.input.cursor = 42;
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 0, 20, 8);
        let (x, y) = input_box.cursor_position(area);
        // word_wrap_segments("this is a long prompt that wraps around!!!", 18, 2):
        // seg 0: "this is a long " (0..15), cols=17
        // seg 1: "prompt that wraps " (15..33), cols=18
        // seg 2: "around!!!" (33..42), cols=9
        // cursor at byte 42 -> seg 2, offset=9, col=9
        assert_eq!(y, 1 + 2); // border + line 2
        assert_eq!(x, 1 + 9); // border + col 9
    }

    #[test]
    fn input_box_shows_ghost_completion() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.completion.completions = vec!["/help".into(), "/history".into()];
        app.input.text = "/h".into();
        app.input.cursor = 2;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("/help"));
    }

    #[test]
    fn cursor_position_multiline() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "hello\nworld".into();
        app.input.cursor = 11;
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 0, 40, 5);
        let (x, y) = input_box.cursor_position(area);
        assert_eq!(x, 1 + 5); // border + col
        assert_eq!(y, 1 + 1); // border + line 1
    }

    #[test]
    fn cursor_position_after_newline() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "hello\n".into();
        app.input.cursor = 6;
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 0, 40, 5);
        let (x, y) = input_box.cursor_position(area);
        assert_eq!(x, 1); // border + col 0
        assert_eq!(y, 2); // border + line 1
    }

    #[test]
    fn cursor_position_applies_input_scroll() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "one two three four five six seven eight nine ten".into();
        app.input.cursor = app.input.text.len();
        app.input.scroll.set(1);
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 0, 20, 6);
        let (_x, y) = input_box.cursor_position(area);
        assert_eq!(y, 3);
    }

    #[test]
    fn input_box_updates_input_scroll_metrics() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.input.text = "line1\nline2\nline3\nline4\nline5".into();
        app.input.cursor = app.input.text.len();
        app.input.scroll.set(99);
        let _buf = render_input(&app, 20, 4);
        assert_eq!(app.input.visible_lines.get(), 2);
        assert!(app.input.total_lines.get() >= 5);
        assert!(app.input.scroll.get() <= app.input.total_lines.get());
    }

    #[test]
    fn word_wrap_preserves_words_on_render() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        // in a 22-wide box (content_width=20), "hello world foo bar baz"
        // should wrap at word boundaries, not mid-word
        app.input.text = "hello world foo bar baz".into();
        app.input.cursor = 23;
        let buf = render_input(&app, 22, 5);
        let content = buffer_to_string(&buf);
        // "world" should not be split across lines
        assert!(
            content.contains("world"),
            "word 'world' should appear intact: {content}"
        );
    }

    #[test]
    fn benchmark_layout_summary_reports_wrapping() {
        let summary =
            benchmark_build_input_layout("one two three four five six seven", 33, &[], 16);
        assert!(summary.total_lines >= 3);
        assert!(summary.cursor_visual_line >= 2);
        assert!(summary.cursor_visual_col < 16);
    }

    fn render_input(app: &App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(InputBox::new(app), area);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_to_string(buf: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }
}
