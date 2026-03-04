//! input box widget - text entry with cursor

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::app::App;

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
        let ch_width = ch.len_utf8();

        if ch == ' ' {
            col += ch_width;
            // record the position AFTER this space as a potential wrap point
            last_space = Some((byte_pos + ch_width, col));

            if col >= width {
                // space itself hits the boundary - wrap here
                segments.push(WrapSegment {
                    start: seg_start,
                    end: byte_pos + ch_width,
                    cols: col,
                });
                seg_start = byte_pos + ch_width;
                col = 0;
                last_space = None;
            }
        } else {
            col += ch_width;
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
                    let seg_end = byte_pos + ch_width;
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

        let text_before_cursor = &self.app.input[..self.app.cursor];
        let input_lines: Vec<&str> = self.app.input.split('\n').collect();
        let cursor_lines: Vec<&str> = text_before_cursor.split('\n').collect();
        // which input line the cursor is on
        let cursor_input_line = cursor_lines.len() - 1;
        // byte offset within that input line
        let cursor_in_line = cursor_lines.last().map(|s| s.len()).unwrap_or(0);

        let mut visual_line: usize = 0;
        let mut visual_col: usize = 0;

        for (line_idx, line_text) in input_lines.iter().enumerate() {
            let indent = if line_idx == 0 { 2 } else { 0 }; // "> " prompt
            let segments = word_wrap_segments(line_text, content_width, indent);

            if line_idx == cursor_input_line {
                // find which segment contains the cursor
                for (seg_i, seg) in segments.iter().enumerate() {
                    let is_last_seg = seg_i == segments.len() - 1;
                    if cursor_in_line < seg.end || (is_last_seg && cursor_in_line <= seg.end) {
                        // cursor is in this segment
                        let offset_in_seg = cursor_in_line - seg.start;
                        let seg_indent = if seg_i == 0 { indent } else { 0 };
                        visual_col = seg_indent + offset_in_seg;
                        break;
                    }
                    visual_line += 1;
                }
                break;
            } else {
                visual_line += segments.len();
            }
        }

        // offset for image attachment label lines
        let image_offset = if self.app.pending_images.is_empty() {
            0
        } else {
            1
        };

        let x = area.x + 1 + visual_col as u16;
        let y = area.y + 1 + image_offset as u16 + visual_line as u16;
        (
            x.min(area.x + area.width - 2),
            y.min(area.y + area.height - 2),
        )
    }
}

impl Widget for InputBox<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let streaming_idle = self.app.is_busy() && self.app.input.is_empty();
        let prompt = if streaming_idle { "..." } else { "> " };
        let style = if streaming_idle {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };

        let prompt_style = Style::default().fg(if streaming_idle {
            Color::DarkGray
        } else {
            Color::Cyan
        });

        let content_width = area.width.saturating_sub(2) as usize;

        // flash border between cyan and blue when there are unread messages
        let border_colour = if !streaming_idle && self.app.has_unread {
            // tick runs at ~60fps, flash at ~1hz (every 30 ticks)
            if self.app.unread_flash_on() {
                Color::Blue
            } else {
                Color::Cyan
            }
        } else if streaming_idle {
            Color::DarkGray
        } else {
            Color::Cyan
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_colour));

        if content_width == 0 {
            Paragraph::new("").block(block).render(area, buf);
            return;
        }

        let input_lines: Vec<&str> = self.app.input.split('\n').collect();
        let mut lines: Vec<Line<'_>> = Vec::new();

        // image attachment indicator
        if !self.app.pending_images.is_empty() {
            let n = self.app.pending_images.len();
            let dim_info = self
                .app
                .pending_images
                .iter()
                .filter_map(|img| img.dimensions.map(|(w, h)| format!("{w}×{h}")))
                .collect::<Vec<_>>()
                .join(", ");
            let label = if dim_info.is_empty() {
                format!(" 📎 {n} image(s) attached")
            } else {
                format!(" 📎 {n} image(s) attached ({dim_info})")
            };
            lines.push(Line::from(vec![Span::styled(
                label,
                Style::default().fg(Color::Magenta),
            )]));
        }

        for (line_idx, line_text) in input_lines.iter().enumerate() {
            let is_first = line_idx == 0;
            let is_last = line_idx == input_lines.len() - 1;
            let indent = if is_first { prompt.len() } else { 0 };
            let segments = word_wrap_segments(line_text, content_width, indent);

            for (seg_i, seg) in segments.iter().enumerate() {
                let mut spans: Vec<Span<'_>> = Vec::new();

                if is_first && seg_i == 0 {
                    spans.push(Span::styled(prompt.to_string(), prompt_style));
                }

                let seg_text = &line_text[seg.start..seg.end];
                if !seg_text.is_empty() {
                    spans.push(Span::styled(seg_text.to_string(), style));
                }

                // ghost completion on the very last segment of the last line
                if is_last
                    && seg_i == segments.len() - 1
                    && let Some(ghost) = self.app.ghost_text()
                {
                    spans.push(Span::styled(
                        ghost.to_string(),
                        Style::default().fg(Color::DarkGray),
                    ));
                }

                lines.push(Line::from(spans));
            }
        }

        let text = ratatui::text::Text::from(lines);
        Paragraph::new(text).block(block).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn word_wrap_no_wrap_needed() {
        let segs = word_wrap_segments("hello", 20, 2);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0], WrapSegment { start: 0, end: 5, cols: 7 });
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
        assert_eq!(segs[0], WrapSegment { start: 0, end: 6, cols: 6 });
        assert_eq!(segs[1], WrapSegment { start: 6, end: 10, cols: 4 });
    }

    #[test]
    fn word_wrap_empty() {
        let segs = word_wrap_segments("", 20, 2);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0], WrapSegment { start: 0, end: 0, cols: 2 });
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
    fn input_box_renders_empty() {
        let app = App::new("test".into(), 200_000);
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("> "));
    }

    #[test]
    fn input_box_renders_text() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello world".into();
        app.cursor = 11;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("hello world"));
    }

    #[test]
    fn input_box_streaming_shows_dots_when_empty() {
        let mut app = App::new("test".into(), 200_000);
        app.is_streaming = true;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("..."));
    }

    #[test]
    fn input_box_streaming_shows_prompt_when_typing() {
        let mut app = App::new("test".into(), 200_000);
        app.is_streaming = true;
        app.input = "hold on".into();
        app.cursor = 7;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("> hold on"));
        assert!(!content.contains("..."));
    }

    #[test]
    fn cursor_position_calculation() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello".into();
        app.cursor = 3;
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 10, 40, 3);
        let (x, y) = input_box.cursor_position(area);
        // col = 2 (prompt) + 3 = 5
        assert_eq!(x, 6); // border + col
        assert_eq!(y, 11); // border
    }

    #[test]
    fn cursor_position_wraps_at_word_boundary() {
        let mut app = App::new("test".into(), 200_000);
        // "this is a long prompt that wraps around!!!" in a 20-wide box
        // content_width = 18
        app.input = "this is a long prompt that wraps around!!!".into();
        app.cursor = 42;
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
        let mut app = App::new("test".into(), 200_000);
        app.completions = vec!["/help".into(), "/history".into()];
        app.input = "/h".into();
        app.cursor = 2;
        let buf = render_input(&app, 40, 3);
        let content = buffer_to_string(&buf);
        assert!(content.contains("/help"));
    }

    #[test]
    fn cursor_position_multiline() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello\nworld".into();
        app.cursor = 11;
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 0, 40, 5);
        let (x, y) = input_box.cursor_position(area);
        assert_eq!(x, 1 + 5); // border + col
        assert_eq!(y, 1 + 1); // border + line 1
    }

    #[test]
    fn cursor_position_after_newline() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello\n".into();
        app.cursor = 6;
        let input_box = InputBox::new(&app);
        let area = Rect::new(0, 0, 40, 5);
        let (x, y) = input_box.cursor_position(area);
        assert_eq!(x, 1); // border + col 0
        assert_eq!(y, 2); // border + line 1
    }

    #[test]
    fn word_wrap_preserves_words_on_render() {
        let mut app = App::new("test".into(), 200_000);
        // in a 22-wide box (content_width=20), "hello world foo bar baz"
        // should wrap at word boundaries, not mid-word
        app.input = "hello world foo bar baz".into();
        app.cursor = 23;
        let buf = render_input(&app, 22, 5);
        let content = buffer_to_string(&buf);
        // "world" should not be split across lines
        assert!(
            content.contains("world"),
            "word 'world' should appear intact: {content}"
        );
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
