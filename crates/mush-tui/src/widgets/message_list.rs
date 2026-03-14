//! message list widget - renders conversation history

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use throbber_widgets_tui::{BRAILLE_SIX, Throbber, WhichUse};

use mush_ai::types::TokenCount;

use crate::app::{
    App, DisplayMessage, DisplayToolCall, ImageRenderArea, MessageRole, ToolCallStatus,
};

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

        // bottom-anchor: when content is shorter than the viewport,
        // pad with empty lines so messages sit near the input box
        let text = Text::from(lines);
        let paragraph_tmp = Paragraph::new(text.clone()).wrap(Wrap { trim: false });
        let content_lines = paragraph_tmp.line_count(area.width).min(u16::MAX as usize) as u16;
        let visible = area.height;

        let text = if content_lines < visible {
            let pad = (visible - content_lines) as usize;
            let mut padded = vec![Line::raw(""); pad];
            padded.extend(text.lines);
            Text::from(padded)
        } else {
            text
        };

        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        let total_lines = paragraph.line_count(area.width).min(u16::MAX as usize) as u16;
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
    // user and assistant messages have no label line.
    // user messages are distinguished by a subtle background.
    // only system messages get a text label.
    let is_user = matches!(msg.role, MessageRole::User);

    if matches!(msg.role, MessageRole::System) {
        let mut label_spans = Vec::new();
        if sel.selected {
            label_spans.push(Span::styled("▌ ", Style::default().fg(Color::Cyan)));
        }
        label_spans.push(Span::styled(
            "system",
            Style::default().fg(Color::Yellow),
        ));
        lines.push(Line::from(label_spans));
    } else if sel.selected {
        let mut hint_spans = vec![Span::styled("▌", Style::default().fg(Color::Cyan))];
        if sel.is_cursor {
            let hint = if sel.has_visual {
                " (y to copy range)"
            } else {
                " (v to select, y to copy)"
            };
            hint_spans.push(Span::styled(hint, Style::default().fg(Color::DarkGray)));
        }
        lines.push(Line::from(hint_spans));
    }

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

    // user messages get a subtle background to distinguish them
    let user_bg = Style::default().bg(Color::Rgb(35, 38, 45));

    // main content (markdown rendered)
    if msg.queued {
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        for line in msg.content.lines() {
            lines.push(Line::styled(format!("  {line}"), dim));
        }
    } else if is_user {
        for line in msg.content.lines() {
            lines.push(Line::from(vec![Span::styled(
                format!("  {line}"),
                user_bg,
            )]));
        }
    } else {
        let md_text = render_markdown(&msg.content);
        for line in md_text.lines {
            let mut spans: Vec<Span<'_>> = vec![Span::raw("  ")];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    }

    // tool calls: group by batch, render as bordered boxes
    // skip running tools (they're shown in the live tool panels)
    let completed: Vec<(usize, &DisplayToolCall)> = msg
        .tool_calls
        .iter()
        .enumerate()
        .filter(|(_, tc)| tc.status != ToolCallStatus::Running)
        .collect();

    // group consecutive tools with the same batch
    let mut i = 0;
    while i < completed.len() {
        let batch = completed[i].1.batch;
        let group_start = i;
        while i < completed.len() && completed[i].1.batch == batch {
            i += 1;
        }
        let group = &completed[group_start..i];

        // collect image placeholders before rendering the group
        for &(tc_idx, tc) in group {
            if tc.image_data.is_some() {
                image_placeholders.push(ImagePlaceholder {
                    msg_idx,
                    tc_idx,
                    line_idx: lines.len(),
                });
            }
        }

        render_tool_box_group(
            &group.iter().map(|(_, tc)| *tc).collect::<Vec<_>>(),
            width,
            lines,
        );

        // after the boxes, render any image placeholders
        for &(_, tc) in group {
            if tc.image_data.is_some() {
                lines.push(Line::from(vec![
                    Span::styled("    📷 ", Style::default()),
                    Span::styled(
                        "image",
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]));
                for _ in 1..IMAGE_HEIGHT {
                    lines.push(Line::raw(""));
                }
            }
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

// -- bordered tool boxes for completed tool calls --

/// indent for tool boxes (matches message content indent)
const BOX_INDENT: usize = 2;

/// minimum width per panel for side-by-side tool boxes
const MIN_TOOL_BOX_WIDTH: u16 = 30;

/// render a group of completed tool calls (same batch) as bordered boxes
fn render_tool_box_group(tools: &[&DisplayToolCall], total_width: u16, lines: &mut Vec<Line<'_>>) {
    let usable = total_width.saturating_sub(BOX_INDENT as u16);
    if usable < 8 || tools.is_empty() {
        return;
    }

    let n = tools.len();
    let side_by_side = n > 1 && usable / n as u16 >= MIN_TOOL_BOX_WIDTH;

    if side_by_side {
        render_side_by_side_boxes(tools, usable as usize, lines);
    } else {
        for tool in tools {
            render_single_tool_box(tool, usable as usize, lines);
        }
    }
}

/// render one completed tool as a bordered box
fn render_single_tool_box(tc: &DisplayToolCall, width: usize, lines: &mut Vec<Line<'_>>) {
    let (icon, colour) = tool_icon_colour(tc);
    let border = Style::default().fg(colour);
    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);

    // title: " ✓ name "
    let title_text = format!(" {icon} {} ", tc.name);
    let title_chars = title_text.chars().count();
    // ┌─ + title + ─...─ + ┐  (width total)
    let fill = width.saturating_sub(title_chars + 3); // 3 = ┌─ + ┐

    // top border
    let indent = Span::raw(" ".repeat(BOX_INDENT));
    lines.push(Line::from(vec![
        indent.clone(),
        Span::styled("┌─", border),
        Span::styled(format!(" {icon} "), Style::default().fg(colour)),
        Span::styled(
            tc.name.clone(),
            Style::default().fg(colour).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", border),
        Span::styled("─".repeat(fill), border),
        Span::styled("┐", border),
    ]));

    // content: summary line
    let inner = width.saturating_sub(4); // │ + space + space + │
    push_box_content_line(&tc.summary, inner, border, dim, &indent, lines);

    // content: output preview with diff colouring
    if let Some(ref output) = tc.output_preview {
        for text_line in output.lines() {
            push_box_content_line(
                text_line,
                inner,
                border,
                diff_line_style(text_line, dim),
                &indent,
                lines,
            );
        }
    }

    // bottom border
    lines.push(Line::from(vec![
        indent,
        Span::styled("└", border),
        Span::styled("─".repeat(width.saturating_sub(2)), border),
        Span::styled("┘", border),
    ]));
}

/// render parallel tools side-by-side in a shared bordered box
fn render_side_by_side_boxes(tools: &[&DisplayToolCall], width: usize, lines: &mut Vec<Line<'_>>) {
    let n = tools.len();
    // each panel width (including its borders): divide evenly
    // total = panel_w * n + (n-1) separators... but we share borders
    // shared layout: ┌─ a ─┬─ b ─┐ = width total
    // each panel inner = (width - n - 1) / n
    let inner_total = width.saturating_sub(n + 1); // n+1 border chars (│ or ┬/┴)
    let panel_inner = inner_total / n;
    let remainder = inner_total % n;

    let dim = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let indent = Span::raw(" ".repeat(BOX_INDENT));

    // determine border colour per panel
    let colours: Vec<Color> = tools.iter().map(|tc| tool_icon_colour(tc).1).collect();

    // -- top border --
    let mut top_spans = vec![indent.clone()];
    for (i, tc) in tools.iter().enumerate() {
        let (icon, colour) = tool_icon_colour(tc);
        let border = Style::default().fg(colour);
        let corner = if i == 0 { "┌─" } else { "┬─" };
        let title = format!(" {icon} {} ", tc.name);
        let title_chars = title.chars().count();
        let pw = panel_inner + if i < remainder { 1 } else { 0 };
        let fill = pw.saturating_sub(title_chars + 1); // 1 for the ─ after corner

        top_spans.push(Span::styled(corner, border));
        top_spans.push(Span::styled(
            format!(" {icon} "),
            Style::default().fg(colour),
        ));
        top_spans.push(Span::styled(
            tc.name.clone(),
            Style::default().fg(colour).add_modifier(Modifier::BOLD),
        ));
        top_spans.push(Span::styled(" ", border));
        top_spans.push(Span::styled("─".repeat(fill), border));
    }
    top_spans.push(Span::styled(
        "┐",
        Style::default().fg(*colours.last().unwrap_or(&Color::DarkGray)),
    ));
    lines.push(Line::from(top_spans));

    // -- content rows: max of content heights across panels --
    // pre-wrap content per panel to fit panel width
    let panel_contents: Vec<Vec<String>> = tools
        .iter()
        .enumerate()
        .map(|(i, tc)| {
            let pw = panel_inner + if i < remainder { 1 } else { 0 };
            let mut content = Vec::new();
            for wrapped in wrap_text(&tc.summary, pw) {
                content.push(wrapped);
            }
            if let Some(ref output) = tc.output_preview {
                for line in output.lines() {
                    for wrapped in wrap_text(line, pw) {
                        content.push(wrapped);
                    }
                }
            }
            content
        })
        .collect();

    let max_rows = panel_contents.iter().map(|c| c.len()).max().unwrap_or(0);

    for row in 0..max_rows {
        let mut spans = vec![indent.clone()];
        for (i, content) in panel_contents.iter().enumerate() {
            let pw = panel_inner + if i < remainder { 1 } else { 0 };
            let border = Style::default().fg(colours[i]);
            spans.push(Span::styled("│", border));

            let text = content.get(row).map(|s| s.as_str()).unwrap_or("");
            let style = diff_line_style(text, dim);
            let pad = pw.saturating_sub(text.chars().count());
            spans.push(Span::styled(format!(" {text}"), style));
            spans.push(Span::raw(" ".repeat(pad)));
        }
        spans.push(Span::styled(
            "│",
            Style::default().fg(*colours.last().unwrap_or(&Color::DarkGray)),
        ));
        lines.push(Line::from(spans));
    }

    // -- bottom border --
    let mut bot_spans = vec![indent];
    for (i, _) in tools.iter().enumerate() {
        let border = Style::default().fg(colours[i]);
        let pw = panel_inner + if i < remainder { 1 } else { 0 };
        let corner = if i == 0 { "└" } else { "┴" };
        bot_spans.push(Span::styled(corner, border));
        bot_spans.push(Span::styled("─".repeat(pw), border));
    }
    bot_spans.push(Span::styled(
        "┘",
        Style::default().fg(*colours.last().unwrap_or(&Color::DarkGray)),
    ));
    lines.push(Line::from(bot_spans));
}

/// push a styled content line inside a bordered box, wrapping if needed
fn push_box_content_line<'a>(
    text: &str,
    inner_width: usize,
    border: Style,
    style: Style,
    indent: &Span<'a>,
    lines: &mut Vec<Line<'a>>,
) {
    for wrapped in wrap_text(text, inner_width) {
        let pad = inner_width.saturating_sub(wrapped.chars().count());
        lines.push(Line::from(vec![
            indent.clone(),
            Span::styled("│ ", border),
            Span::styled(wrapped, style),
            Span::raw(" ".repeat(pad)),
            Span::styled(" │", border),
        ]));
    }
}

/// wrap text to fit within `width` chars, breaking at spaces first,
/// then character-wise for words longer than the width
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![text.to_string()];
    }
    if text.chars().count() <= width {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_len: usize = 0;

    for word in text.split(' ') {
        let word_len = word.chars().count();

        if word_len > width {
            // push current line first
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_len = 0;
            }
            // character-wrap the long word
            let mut remaining = word;
            while !remaining.is_empty() {
                let end = remaining
                    .char_indices()
                    .nth(width)
                    .map_or(remaining.len(), |(i, _)| i);
                let chunk = &remaining[..end];
                remaining = &remaining[end..];
                if remaining.is_empty() {
                    // last chunk becomes current line so next word can join
                    current = chunk.to_string();
                    current_len = chunk.chars().count();
                } else {
                    lines.push(chunk.to_string());
                }
            }
        } else if current.is_empty() {
            current = word.to_string();
            current_len = word_len;
        } else if current_len + 1 + word_len <= width {
            current.push(' ');
            current.push_str(word);
            current_len += 1 + word_len;
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
            current_len = word_len;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// get icon and colour for a completed tool call
fn tool_icon_colour(tc: &DisplayToolCall) -> (&'static str, Color) {
    match tc.status {
        ToolCallStatus::Done => ("✓", Color::Green),
        ToolCallStatus::Error => ("✗", Color::Red),
        ToolCallStatus::Running => ("⣾", Color::Cyan),
    }
}

/// style for a line of tool output (diff-aware)
fn diff_line_style(line: &str, fallback: Style) -> Style {
    if line.starts_with("+ ") {
        Style::default().fg(Color::Green)
    } else if line.starts_with("- ") {
        Style::default().fg(Color::Red)
    } else {
        fallback
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
                    batch: 1,
                },
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "src/main.rs".into(),
                    status: ToolCallStatus::Error,
                    output_preview: None,
                    image_data: None,
                    batch: 2,
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
                batch: 1,
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

    #[test]
    fn completed_tool_renders_bordered_box() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "checking".into(),
            tool_calls: vec![crate::app::DisplayToolCall {
                name: "bash".into(),
                summary: "cargo test".into(),
                status: ToolCallStatus::Done,
                output_preview: None,
                image_data: None,
                batch: 1,
            }],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        });
        let buf = render_app(&app, 50, 10);
        let content = buffer_to_string(&buf);
        // bordered box with green tick and tool name in title
        assert!(content.contains("┌"), "missing top-left corner");
        assert!(content.contains("✓"), "missing tick");
        assert!(content.contains("bash"), "missing tool name");
        assert!(content.contains("cargo test"), "missing summary");
        assert!(content.contains("└"), "missing bottom-left corner");
    }

    #[test]
    fn failed_tool_renders_red_bordered_box() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "checking".into(),
            tool_calls: vec![crate::app::DisplayToolCall {
                name: "bash".into(),
                summary: "cargo check".into(),
                status: ToolCallStatus::Error,
                output_preview: Some("error[E0063]: missing field".into()),
                image_data: None,
                batch: 1,
            }],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        });
        let buf = render_app(&app, 60, 12);
        let content = buffer_to_string(&buf);
        assert!(content.contains("✗"), "missing cross");
        assert!(content.contains("bash"), "missing tool name");
        assert!(content.contains("error"), "missing error output");
    }

    #[test]
    fn parallel_tools_render_side_by_side() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "reading".into(),
            tool_calls: vec![
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "a.rs".into(),
                    status: ToolCallStatus::Done,
                    output_preview: None,
                    image_data: None,
                    batch: 1,
                },
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "b.rs".into(),
                    status: ToolCallStatus::Done,
                    output_preview: None,
                    image_data: None,
                    batch: 1, // same batch = parallel
                },
            ],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        });
        // 80 wide: each panel gets ~39 cols, > MIN_TOOL_BOX_WIDTH (30)
        let buf = render_app(&app, 80, 10);
        let content = buffer_to_string(&buf);
        // side-by-side uses ┬ as a junction between panels
        assert!(
            content.contains("┬"),
            "missing top junction (not side-by-side)"
        );
        assert!(content.contains("a.rs"), "missing first tool summary");
        assert!(content.contains("b.rs"), "missing second tool summary");
    }

    #[test]
    fn parallel_tools_stack_when_narrow() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "reading".into(),
            tool_calls: vec![
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "a.rs".into(),
                    status: ToolCallStatus::Done,
                    output_preview: None,
                    image_data: None,
                    batch: 1,
                },
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "b.rs".into(),
                    status: ToolCallStatus::Done,
                    output_preview: None,
                    image_data: None,
                    batch: 1,
                },
            ],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        });
        // 40 wide: each panel would be ~19 cols, < MIN_TOOL_BOX_WIDTH (30)
        let buf = render_app(&app, 40, 12);
        let content = buffer_to_string(&buf);
        // stacked: no junction, two separate boxes
        assert!(!content.contains("┬"), "should not be side-by-side");
        assert!(content.contains("a.rs"), "missing first tool");
        assert!(content.contains("b.rs"), "missing second tool");
    }

    #[test]
    fn error_tool_box_has_red_border() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "reading".into(),
            tool_calls: vec![crate::app::DisplayToolCall {
                name: "read".into(),
                summary: "missing.rs".into(),
                status: ToolCallStatus::Error,
                output_preview: Some("file not found".into()),
                image_data: None,
                batch: 1,
            }],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        });
        let buf = render_app(&app, 50, 10);
        // check that the top-left corner cell has red foreground
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                if cell.symbol() == "┌" {
                    assert_eq!(
                        cell.fg,
                        Color::Red,
                        "border should be red for error tool at ({x}, {y})"
                    );
                    return;
                }
            }
        }
        panic!("no ┌ found in rendered output");
    }

    #[test]
    fn wrap_text_short_line_unchanged() {
        assert_eq!(wrap_text("hello", 20), vec!["hello"]);
    }

    #[test]
    fn wrap_text_breaks_at_word_boundary() {
        assert_eq!(wrap_text("hello world foo", 11), vec!["hello world", "foo"]);
    }

    #[test]
    fn wrap_text_long_word_char_wraps() {
        assert_eq!(
            wrap_text("/very/long/path/name", 10),
            vec!["/very/long", "/path/name"]
        );
    }

    #[test]
    fn wrap_text_empty() {
        assert_eq!(wrap_text("", 10), vec![""]);
    }

    #[test]
    fn tool_box_wraps_long_summary() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let long_path = "/home/rowan/dev/mush/crates/mush-tui/src/widgets/message_list.rs";
        app.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "reading".into(),
            tool_calls: vec![crate::app::DisplayToolCall {
                name: "read".into(),
                summary: long_path.into(),
                status: ToolCallStatus::Done,
                output_preview: None,
                image_data: None,
                batch: 1,
            }],
            thinking: None,
            thinking_expanded: false,
            usage: None,
            cost: None,
            model_id: None,
            queued: false,
        });
        // narrow box: path won't fit on one line
        let buf = render_app(&app, 40, 12);
        let content = buffer_to_string(&buf);
        // full path should be visible (wrapped, not truncated)
        assert!(
            content.contains("message_list.rs"),
            "path end should be visible after wrapping"
        );
        // no ellipsis
        assert!(!content.contains("…"), "should wrap, not truncate");
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
