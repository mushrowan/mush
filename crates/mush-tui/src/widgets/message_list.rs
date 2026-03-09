//! message list widget - renders conversation history

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use throbber_widgets_tui::{BRAILLE_SIX, Throbber, WhichUse};

use mush_ai::types::TokenCount;

use crate::app::{App, DisplayMessage, ImageRenderArea, MessageRole, ToolCallStatus};

/// renders the full message list including any active stream
pub struct MessageList<'a> {
    app: &'a App,
}

impl<'a> MessageList<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }
}

impl Widget for MessageList<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut lines: Vec<Line<'_>> = Vec::new();
        let mut image_placeholders: Vec<ImagePlaceholder> = Vec::new();

        let in_scroll_mode = self.app.mode == crate::app::AppMode::Scroll;
        let selection_range = self.app.selection_range();
        for (i, msg) in self.app.messages.iter().enumerate() {
            if msg.queued {
                continue; // rendered after streaming content
            }
            let in_selection = selection_range.is_some_and(|(start, end)| i >= start && i <= end);
            let sel = SelectionHint {
                selected: in_scroll_mode && (self.app.selected_message == Some(i) || in_selection),
                is_cursor: in_scroll_mode && self.app.selected_message == Some(i),
                has_visual: self.app.has_selection(),
            };
            render_message(msg, i, &mut lines, sel, &mut image_placeholders, area.width);
            lines.push(Line::raw(""));
        }

        // streaming content
        if self.app.is_streaming {
            let dim = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM);
            let throbber = Throbber::default()
                .throbber_set(BRAILLE_SIX)
                .use_type(WhichUse::Spin);
            let spinner_span = throbber.to_symbol_span(&self.app.throbber_state);

            let stream_label = truncate_model_id(&self.app.model_id);
            lines.push(Line::from(vec![Span::styled(
                stream_label.to_string(),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )]));

            if !self.app.streaming_thinking.is_empty()
                && self.app.thinking_display != crate::app::ThinkingDisplay::Hidden
            {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    spinner_span.clone().style(dim),
                    Span::styled(" thinking", dim),
                ]));
                let visible_thinking = self.app.visible_streaming_thinking();
                for line in visible_thinking.lines() {
                    lines.push(Line::styled(format!("  {line}"), dim));
                }
                lines.push(Line::raw(""));
            }
            if !self.app.streaming_text.is_empty() {
                let visible_text = self.app.visible_streaming_text();
                let md_text = render_markdown(visible_text);
                for line in md_text.lines {
                    let mut spans: Vec<Span<'_>> = vec![Span::raw("  ")];
                    spans.extend(line.spans);
                    lines.push(Line::from(spans));
                }
            }
            if self.app.streaming_text.is_empty() && self.app.streaming_thinking.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    spinner_span.clone().style(dim),
                    Span::styled(" working", dim),
                ]));
            }
        }

        // streaming tool args (model is building tool call, not yet executing)
        if self.app.active_tools.is_empty() && !self.app.streaming_tool_args.is_empty() {
            let throbber = Throbber::default()
                .throbber_set(BRAILLE_SIX)
                .use_type(WhichUse::Spin);
            let spinner_span = throbber.to_symbol_span(&self.app.throbber_state);
            // show a truncated preview of the args being built
            let preview = truncate_line(&self.app.streaming_tool_args, 60);
            lines.push(Line::from(vec![
                Span::raw("  "),
                spinner_span.style(Style::default().fg(Color::DarkGray)),
                Span::styled(" building ", Style::default().fg(Color::DarkGray)),
                Span::styled(preview, Style::default().fg(Color::DarkGray)),
            ]));
        }

        // queued (steering) messages always appear at the bottom
        for (i, msg) in self.app.messages.iter().enumerate() {
            if !msg.queued {
                continue;
            }
            let in_selection = selection_range.is_some_and(|(start, end)| i >= start && i <= end);
            let sel = SelectionHint {
                selected: in_scroll_mode && (self.app.selected_message == Some(i) || in_selection),
                is_cursor: in_scroll_mode && self.app.selected_message == Some(i),
                has_visual: self.app.has_selection(),
            };
            render_message(msg, i, &mut lines, sel, &mut image_placeholders, area.width);
            lines.push(Line::raw(""));
        }

        // pre-compute y positions for image placeholders before moving lines
        // into Text (uses simple char-width approximation, close enough for
        // image overlay positioning)
        let w = area.width as usize;
        let img_y_positions: Vec<u16> = if !image_placeholders.is_empty() && w > 0 {
            image_placeholders
                .iter()
                .map(|ph| {
                    lines[..ph.line_idx]
                        .iter()
                        .map(|line| {
                            let lw = line.width();
                            if lw <= w { 1u16 } else { lw.div_ceil(w) as u16 }
                        })
                        .sum()
                })
                .collect()
        } else {
            Vec::new()
        };

        let text = Text::from(lines);

        // use ratatui's own word-wrap logic for accurate line count
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        let total_lines = paragraph.line_count(area.width).min(u16::MAX as usize) as u16;
        let visible = area.height;
        let max_scroll = total_lines.saturating_sub(visible);
        let scroll = max_scroll.saturating_sub(self.app.scroll_offset);

        // expose scroll geometry for the status bar
        self.app.total_content_lines.set(total_lines);
        self.app.visible_area_height.set(visible);

        // compute image render areas based on scroll position
        let mut render_areas = Vec::new();
        if !image_placeholders.is_empty() && area.width > 0 {
            for (i, ph) in image_placeholders.iter().enumerate() {
                let y_before = img_y_positions[i];
                // skip the label line, image starts on the line after
                let img_y = y_before.saturating_add(1).saturating_sub(scroll);
                let img_height = IMAGE_HEIGHT.saturating_sub(1); // minus the label
                // check if visible
                if img_y < visible && img_y + img_height > 0 {
                    let visible_y = area.y + img_y;
                    let visible_h = img_height.min(visible.saturating_sub(img_y));
                    // indent 4 chars, leave some right margin
                    let img_x = area.x + 4;
                    let img_w = area.width.saturating_sub(8); // 4 left + 4 right margin
                    if img_w > 0 && visible_h > 0 {
                        render_areas.push(ImageRenderArea {
                            msg_idx: ph.msg_idx,
                            tc_idx: ph.tc_idx,
                            area: Rect::new(img_x, visible_y, img_w, visible_h),
                        });
                    }
                }
            }
        }
        *self.app.image_render_areas.borrow_mut() = render_areas;

        paragraph.scroll((scroll, 0)).render(area, buf);
    }
}

fn truncate_model_id(id: &str) -> &str {
    // model ids are ASCII so byte slicing is safe, but guard anyway
    if id.len() > 20 {
        &id[..id.floor_char_boundary(20)]
    } else {
        id
    }
}

/// height reserved for inline image rendering (in lines)
const IMAGE_HEIGHT: u16 = 12;

/// tracks where an image placeholder starts in the lines vec
struct ImagePlaceholder {
    msg_idx: usize,
    tc_idx: usize,
    /// line index in the lines vec where the placeholder starts
    line_idx: usize,
}

/// scroll/selection state passed to render_message
struct SelectionHint {
    /// message is highlighted (cursor or within visual range)
    selected: bool,
    /// message is the cursor position (shows hint text)
    is_cursor: bool,
    /// visual selection is active
    has_visual: bool,
}

#[allow(clippy::too_many_arguments)]
fn render_message(
    msg: &DisplayMessage,
    msg_idx: usize,
    lines: &mut Vec<Line<'_>>,
    sel: SelectionHint,
    image_placeholders: &mut Vec<ImagePlaceholder>,
    width: u16,
) {
    let (label, label_style) = match msg.role {
        MessageRole::User if msg.queued => (
            "you (queued)".to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ),
        MessageRole::User => (
            "you".to_string(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        MessageRole::Assistant => {
            let name = msg
                .model_id
                .as_deref()
                .map(|id| truncate_model_id(id))
                .unwrap_or("mush");
            (
                name.to_string(),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )
        }
        MessageRole::System => ("system".to_string(), Style::default().fg(Color::Yellow)),
    };

    let mut label_spans = Vec::new();
    if sel.selected {
        label_spans.push(Span::styled("▌ ", Style::default().fg(Color::Cyan)));
    }
    label_spans.push(Span::styled(label, label_style));
    // show hint only on the cursor message
    if sel.is_cursor {
        let hint = if sel.has_visual {
            " (y to copy range)"
        } else {
            " (v to select, y to copy)"
        };
        label_spans.push(Span::styled(hint, Style::default().fg(Color::DarkGray)));
    }
    lines.push(Line::from(label_spans));

    // thinking block
    if let Some(ref thinking) = msg.thinking {
        if msg.thinking_expanded {
            for line in thinking.lines() {
                lines.push(Line::styled(
                    format!("  {line}"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ));
            }
        } else {
            let preview = thinking.lines().next().unwrap_or("...");
            let trimmed = if preview.chars().count() > 60 {
                let truncated: String = preview.chars().take(57).collect();
                format!("{truncated}...")
            } else {
                preview.to_string()
            };
            lines.push(Line::from(vec![
                Span::styled("  💭 ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    trimmed,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    " [ctrl+o]",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
            ]));
        }
    }

    // main content (markdown rendered)
    if msg.queued {
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        for line in msg.content.lines() {
            lines.push(Line::styled(format!("  {line}"), dim));
        }
    } else {
        let md_text = render_markdown(&msg.content);
        for line in md_text.lines {
            let mut spans: Vec<Span<'_>> = vec![Span::raw("  ")];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    }

    // tool calls (skip running ones, they're shown in the tool panels)
    for (tc_idx, tc) in msg.tool_calls.iter().enumerate() {
        if tc.status == ToolCallStatus::Running {
            continue;
        }
        let (icon, colour) = match tc.status {
            ToolCallStatus::Running => unreachable!(),
            ToolCallStatus::Done => ("✓", Color::Green),
            ToolCallStatus::Error => ("✗", Color::Red),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {icon} "), Style::default().fg(colour)),
            Span::styled(tc.name.clone(), Style::default().fg(colour)),
            Span::raw(" "),
            Span::styled(tc.summary.clone(), Style::default().fg(Color::DarkGray)),
        ]));
        // image: reserve space for inline rendering
        if tc.image_data.is_some() {
            image_placeholders.push(ImagePlaceholder {
                msg_idx,
                tc_idx,
                line_idx: lines.len(),
            });
            // first line: label
            lines.push(Line::from(vec![
                Span::styled("    📷 ", Style::default()),
                Span::styled(
                    "image",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
            // remaining lines: blank space for the image overlay
            for _ in 1..IMAGE_HEIGHT {
                lines.push(Line::raw(""));
            }
        }
        // tool output preview with diff colouring
        if let Some(ref output) = tc.output_preview {
            render_diff_output(output, lines, width);
        }
    }

    // usage line (compact: total tokens + cost, with cache reuse + write ratios)
    if let Some(ref usage) = msg.usage {
        let total = usage.total_tokens();
        if total > TokenCount::ZERO {
            let mut parts = vec![format!("  {total}tok")];
            let reuse_base = usage.cache_read_tokens + usage.input_tokens;
            if reuse_base > TokenCount::ZERO {
                let reuse_pct = usage.cache_read_tokens.percent_of(reuse_base) as u32;
                parts.push(format!("reuse {reuse_pct}%"));
            }
            if usage.cache_write_tokens > TokenCount::ZERO {
                let write_pct = usage
                    .cache_write_tokens
                    .percent_of(usage.total_input_tokens()) as u32;
                parts.push(format!("write {write_pct}%"));
            }
            if let Some(c) = msg.cost {
                parts.push(format!("{c}"));
            }
            lines.push(Line::styled(
                parts.join(" | "),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ));
        }
    }
}

/// truncate a string to at most `max` characters (not bytes), adding ellipsis
fn truncate_line(s: &str, max: usize) -> String {
    // take first line, strip whitespace
    let line = s.lines().next().unwrap_or(s).trim();
    let char_count = line.chars().count();
    if char_count <= max {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(max).collect();
        format!("{truncated}…")
    }
}

/// render markdown text to styled ratatui Text
fn render_markdown(source: &str) -> Text<'static> {
    if source.is_empty() {
        return Text::default();
    }
    crate::markdown::render(source)
}

/// minimum usable width for side-by-side diff (indent + two panels + separator)
const SIDE_BY_SIDE_MIN_WIDTH: u16 = 60;

/// render tool output, using side-by-side layout for diffs when wide enough
fn render_diff_output(output: &str, lines: &mut Vec<Line<'_>>, width: u16) {
    let indent = 4;
    let all_lines: Vec<&str> = output.lines().collect();

    // check if this looks like a diff (has + or - prefixed lines)
    let has_removed = all_lines.iter().any(|l| l.starts_with("- "));
    let has_added = all_lines.iter().any(|l| l.starts_with("+ "));
    let is_diff = has_removed && has_added;

    if !is_diff || width < SIDE_BY_SIDE_MIN_WIDTH {
        // fall back to unified diff / plain output
        for line in &all_lines {
            let style = if line.starts_with("+ ") {
                Style::default().fg(Color::Green)
            } else if line.starts_with("- ") {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM)
            };
            lines.push(Line::styled(format!("{:indent$}{line}", ""), style));
        }
        return;
    }

    // collect removed and added lines
    let removed: Vec<&str> = all_lines
        .iter()
        .filter(|l| l.starts_with("- "))
        .map(|l| l.strip_prefix("- ").unwrap_or(l))
        .collect();
    let added: Vec<&str> = all_lines
        .iter()
        .filter(|l| l.starts_with("+ "))
        .map(|l| l.strip_prefix("+ ").unwrap_or(l))
        .collect();

    // also pass through any non-diff lines (e.g. "edited path/to/file") first
    for line in &all_lines {
        if !line.starts_with("+ ") && !line.starts_with("- ") {
            lines.push(Line::styled(
                format!("{:indent$}{line}", ""),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ));
        }
    }

    // each panel gets half the available width (minus indent, separator)
    let usable = (width as usize).saturating_sub(indent + 3); // 3 for " │ "
    let half = usable / 2;
    let row_count = removed.len().max(added.len());

    let red = Style::default().fg(Color::Red);
    let green = Style::default().fg(Color::Green);
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);

    for i in 0..row_count {
        let left = removed.get(i).copied().unwrap_or("");
        let right = added.get(i).copied().unwrap_or("");

        // truncate to fit panel width (char-safe)
        let left_display = if left.chars().count() > half {
            let truncated: String = left.chars().take(half.saturating_sub(1)).collect();
            format!("{truncated}…")
        } else {
            left.to_string()
        };
        let right_display = if right.chars().count() > half {
            let truncated: String = right.chars().take(half.saturating_sub(1)).collect();
            format!("{truncated}…")
        } else {
            right.to_string()
        };

        let left_style = if left.is_empty() { dim } else { red };
        let right_style = if right.is_empty() { dim } else { green };

        let pad_left = half.saturating_sub(left_display.len());
        let spans = vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(left_display, left_style),
            Span::raw(" ".repeat(pad_left)),
            Span::styled(" │ ", dim),
            Span::styled(right_display, right_style),
        ];
        lines.push(Line::from(spans));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{Dollars, Usage};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render_app(app: &App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(MessageList::new(app), area);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn empty_app_renders() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let buf = render_app(&app, 40, 10);
        // should be mostly empty
        let content = buffer_to_string(&buf);
        assert!(content.trim().is_empty());
    }

    #[test]
    fn user_message_renders() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.push_user_message("hello world");
        let buf = render_app(&app, 40, 10);
        let content = buffer_to_string(&buf);
        assert!(content.contains("you"));
        assert!(content.contains("hello world"));
    }

    #[test]
    fn assistant_message_renders() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.start_streaming();
        app.push_text_delta("i can help");
        app.finish_streaming(None, None);
        let buf = render_app(&app, 40, 10);
        let content = buffer_to_string(&buf);
        // model_id is "test" (from App::new)
        assert!(content.contains("test"));
        assert!(content.contains("i can help"));
    }

    #[test]
    fn streaming_shows_partial_text() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.start_streaming();
        app.push_text_delta("partial");
        // tick several times to let typewriter catch up
        for _ in 0..10 {
            app.tick();
        }
        // don't finish - still streaming
        let buf = render_app(&app, 40, 10);
        let content = buffer_to_string(&buf);
        assert!(content.contains("partial"));
    }

    #[test]
    fn usage_line_shows_reuse_and_write_ratios() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "done".into(),
            tool_calls: vec![],
            thinking: None,
            thinking_expanded: false,
            usage: Some(Usage {
                input_tokens: TokenCount::new(100),
                output_tokens: TokenCount::new(20),
                cache_read_tokens: TokenCount::new(150),
                cache_write_tokens: TokenCount::new(50),
            }),
            cost: Some(Dollars::new(0.0012)),
            model_id: None,
            queued: false,
        });

        let buf = render_app(&app, 70, 10);
        let content = buffer_to_string(&buf);
        assert!(content.contains("320tok"));
        assert!(content.contains("reuse 60%"));
        assert!(content.contains("write 16%"));
        assert!(content.contains("$0.0012"));
    }

    #[test]
    fn tool_calls_render() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "let me check".into(),
            tool_calls: vec![
                crate::app::DisplayToolCall {
                    name: "bash".into(),
                    summary: "ls -la".into(),
                    status: ToolCallStatus::Done,
                    output_preview: Some("file1.txt\nfile2.txt".into()),
                    image_data: None,
                },
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "src/main.rs".into(),
                    status: ToolCallStatus::Error,
                    output_preview: None,
                    image_data: None,
                },
            ],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        });
        let buf = render_app(&app, 50, 15);
        let content = buffer_to_string(&buf);
        assert!(content.contains("bash"));
        assert!(content.contains("read"));
    }

    #[test]
    fn thinking_shows_collapsed() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "the answer is 42".into(),
            tool_calls: vec![],
            thinking: Some(
                "first i need to consider the question deeply and think about it".into(),
            ),
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        });
        let buf = render_app(&app, 60, 10);
        let content = buffer_to_string(&buf);
        // should show the thinking emoji indicator
        assert!(content.contains("💭"));
    }

    #[test]
    fn image_reserves_space_and_produces_render_area() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "here is the image".into(),
            tool_calls: vec![crate::app::DisplayToolCall {
                name: "read".into(),
                summary: "photo.png".into(),
                status: ToolCallStatus::Done,
                output_preview: None,
                image_data: Some(vec![0u8; 100]), // dummy bytes
            }],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        });
        let buf = render_app(&app, 60, 30);
        let content = buffer_to_string(&buf);
        // should show the image label
        assert!(content.contains("📷"));
        assert!(content.contains("image"));
        // should have produced a render area
        let areas = app.image_render_areas.borrow();
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].msg_idx, 0);
        assert_eq!(areas[0].tc_idx, 0);
        // area should have reasonable dimensions
        assert!(areas[0].area.height > 0);
        assert!(areas[0].area.width > 0);
    }

    /// helper: convert buffer to string for assertions
    fn buffer_to_string(buf: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                s.push_str(cell.symbol());
            }
            s.push('\n');
        }
        s
    }
}
