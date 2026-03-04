//! message list widget - renders conversation history

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use throbber_widgets_tui::{BRAILLE_SIX, Throbber, WhichUse};

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
        for (i, msg) in self.app.messages.iter().enumerate() {
            let selected = in_scroll_mode && self.app.selected_message == Some(i);
            render_message(
                msg,
                i,
                &mut lines,
                &self.app.tool_output_live,
                selected,
                &mut image_placeholders,
            );
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

            if !self.app.streaming_thinking.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    spinner_span.clone().style(dim),
                    Span::styled(" thinking", dim),
                ]));
                for line in self.app.streaming_thinking.lines() {
                    lines.push(Line::styled(format!("  {line}"), dim));
                }
                lines.push(Line::raw(""));
            }
            if !self.app.streaming_text.is_empty() {
                let md_text = render_markdown(&self.app.streaming_text);
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

        // active tool indicator (executing, not yet done)
        for tool in &self.app.active_tools {
            if tool.status != ToolCallStatus::Running {
                continue;
            }
            let throbber = Throbber::default()
                .throbber_set(BRAILLE_SIX)
                .use_type(WhichUse::Spin);
            let spinner_span = throbber.to_symbol_span(&self.app.throbber_state);
            lines.push(Line::from(vec![
                Span::raw("  "),
                spinner_span.style(Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(tool.name.as_str(), Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(tool.summary.as_str(), Style::default().fg(Color::DarkGray)),
            ]));

            if let Some(ref live) = tool.live_output {
                lines.push(Line::styled(
                    format!("    {live}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }

        let text = Text::from(lines);

        // scroll: account for line wrapping so resize keeps the bottom visible
        let total_lines = wrapped_line_count(&text, area.width);
        let visible = area.height;
        let max_scroll = total_lines.saturating_sub(visible);
        let scroll = max_scroll.saturating_sub(self.app.scroll_offset);

        // compute image render areas based on scroll position
        let mut render_areas = Vec::new();
        if !image_placeholders.is_empty() && area.width > 0 {
            let w = area.width as usize;
            for ph in &image_placeholders {
                // count wrapped lines before this placeholder
                let y_before: u16 = text.lines[..ph.line_idx]
                    .iter()
                    .map(|line| {
                        let lw = line.width();
                        if lw <= w { 1u16 } else { lw.div_ceil(w) as u16 }
                    })
                    .sum();
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

        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .render(area, buf);
    }
}

/// count visual lines after wrapping (each source line wraps to ceil(width/area_width))
fn wrapped_line_count(text: &Text<'_>, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    let w = width as usize;
    text.lines
        .iter()
        .map(|line| {
            let lw = line.width();
            if lw <= w { 1u16 } else { lw.div_ceil(w) as u16 }
        })
        .sum()
}

fn truncate_model_id(id: &str) -> &str {
    if id.len() > 20 { &id[..20] } else { id }
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

fn render_message(
    msg: &DisplayMessage,
    msg_idx: usize,
    lines: &mut Vec<Line<'_>>,
    live_tool_output: &Option<String>,
    selected: bool,
    image_placeholders: &mut Vec<ImagePlaceholder>,
) {
    let (label, label_style) = match msg.role {
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
    if selected {
        label_spans.push(Span::styled("▌ ", Style::default().fg(Color::Cyan)));
    }
    label_spans.push(Span::styled(label, label_style));
    if selected {
        label_spans.push(Span::styled(
            " (y to copy)",
            Style::default().fg(Color::DarkGray),
        ));
    }
    lines.push(Line::from(label_spans));

    // thinking block
    if let Some(ref thinking) = msg.thinking {
        if msg.thinking_expanded {
            let line_count = thinking.lines().count();
            lines.push(Line::from(vec![
                Span::styled("  💭 ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("thinking ({line_count} lines) [ctrl+o to collapse]"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
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
            let trimmed = if preview.len() > 60 {
                format!("{}...", &preview[..57])
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
    let md_text = render_markdown(&msg.content);
    for line in md_text.lines {
        let mut spans: Vec<Span<'_>> = vec![Span::raw("  ")];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }

    // tool calls
    for (tc_idx, tc) in msg.tool_calls.iter().enumerate() {
        let (icon, colour) = match tc.status {
            ToolCallStatus::Running => ("▶", Color::Cyan),
            ToolCallStatus::Done => ("✓", Color::Green),
            ToolCallStatus::Error => ("✗", Color::Red),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {icon} "), Style::default().fg(colour)),
            Span::styled(tc.name.clone(), Style::default().fg(colour)),
            Span::raw(" "),
            Span::styled(tc.summary.clone(), Style::default().fg(Color::DarkGray)),
        ]));
        // live output from running tool
        if tc.status == ToolCallStatus::Running
            && let Some(live) = live_tool_output
        {
            lines.push(Line::styled(
                format!("    {live}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
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
            for line in output.lines() {
                let style = if line.starts_with("+ ") {
                    Style::default().fg(Color::Green)
                } else if line.starts_with("- ") {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM)
                };
                lines.push(Line::styled(format!("    {line}"), style));
            }
        }
    }

    // usage line (compact: total tokens + cost, with cache hit % if relevant)
    if let Some(ref usage) = msg.usage {
        let total = usage.total_tokens();
        if total > 0 {
            let mut parts = vec![format!("  {total}tok")];
            if usage.cache_read_tokens > 0 {
                let total_input = usage.total_input_tokens().max(1) as f64;
                let pct = (usage.cache_read_tokens as f64 / total_input * 100.0) as u32;
                parts.push(format!("{pct}% cached"));
            }
            if let Some(c) = msg.cost {
                parts.push(format!("${c:.4}"));
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

/// truncate a string to a max length, adding ellipsis
fn truncate_line(s: &str, max: usize) -> String {
    // take first line, strip whitespace
    let line = s.lines().next().unwrap_or(s).trim();
    if line.len() <= max {
        line.to_string()
    } else {
        format!("{}…", &line[..max])
    }
}

/// render markdown text to styled ratatui Text
fn render_markdown(source: &str) -> Text<'static> {
    if source.is_empty() {
        return Text::default();
    }
    crate::markdown::render(source)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let app = App::new("test".into(), 200_000);
        let buf = render_app(&app, 40, 10);
        // should be mostly empty
        let content = buffer_to_string(&buf);
        assert!(content.trim().is_empty());
    }

    #[test]
    fn user_message_renders() {
        let mut app = App::new("test".into(), 200_000);
        app.push_user_message("hello world");
        let buf = render_app(&app, 40, 10);
        let content = buffer_to_string(&buf);
        assert!(content.contains("you"));
        assert!(content.contains("hello world"));
    }

    #[test]
    fn assistant_message_renders() {
        let mut app = App::new("test".into(), 200_000);
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
        let mut app = App::new("test".into(), 200_000);
        app.start_streaming();
        app.push_text_delta("partial");
        // don't finish - still streaming
        let buf = render_app(&app, 40, 10);
        let content = buffer_to_string(&buf);
        assert!(content.contains("partial"));
    }

    #[test]
    fn tool_calls_render() {
        let mut app = App::new("test".into(), 200_000);
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
        });
        let buf = render_app(&app, 50, 15);
        let content = buffer_to_string(&buf);
        assert!(content.contains("bash"));
        assert!(content.contains("read"));
    }

    #[test]
    fn thinking_shows_collapsed() {
        let mut app = App::new("test".into(), 200_000);
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
        });
        let buf = render_app(&app, 60, 10);
        let content = buffer_to_string(&buf);
        // should show the thinking emoji indicator
        assert!(content.contains("💭"));
    }

    #[test]
    fn image_reserves_space_and_produces_render_area() {
        let mut app = App::new("test".into(), 200_000);
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
