//! message list widget - renders conversation history

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use throbber_widgets_tui::{BRAILLE_SIX, Throbber, WhichUse};

use crate::app::{App, DisplayMessage, MessageRole, ToolCallStatus};

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

        let in_scroll_mode = self.app.mode == crate::app::AppMode::Scroll;
        for (i, msg) in self.app.messages.iter().enumerate() {
            let selected = in_scroll_mode && self.app.selected_message == Some(i);
            render_message(msg, &mut lines, &self.app.tool_output_live, selected);
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
        if self.app.active_tool.is_none() && !self.app.streaming_tool_args.is_empty() {
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

        // active tool indicator (executing)
        if let Some(ref tool) = self.app.active_tool {
            let throbber = Throbber::default()
                .throbber_set(BRAILLE_SIX)
                .use_type(WhichUse::Spin);
            let spinner_span = throbber.to_symbol_span(&self.app.throbber_state);
            lines.push(Line::from(vec![
                Span::raw("  "),
                spinner_span.style(Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(tool.as_str(), Style::default().fg(Color::Cyan)),
            ]));
        }

        let text = Text::from(lines);

        // scroll: account for line wrapping so resize keeps the bottom visible
        let total_lines = wrapped_line_count(&text, area.width);
        let visible = area.height;
        let max_scroll = total_lines.saturating_sub(visible);
        let scroll = max_scroll.saturating_sub(self.app.scroll_offset);

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
            if lw <= w { 1u16 } else { ((lw + w - 1) / w) as u16 }
        })
        .sum()
}

fn truncate_model_id(id: &str) -> &str {
    if id.len() > 20 { &id[..20] } else { id }
}

fn render_message(
    msg: &DisplayMessage,
    lines: &mut Vec<Line<'_>>,
    live_tool_output: &Option<String>,
    selected: bool,
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
                .map(truncate_model_id)
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
    for tc in &msg.tool_calls {
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
        if tc.status == ToolCallStatus::Running {
            if let Some(live) = live_tool_output {
                lines.push(Line::styled(
                    format!("    {live}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
        // image indicator
        if tc.image_data.is_some() {
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled("📷 ", Style::default()),
                Span::styled(
                    "[image attached]",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
        // tool output preview (dim, indented)
        if let Some(ref output) = tc.output_preview {
            for line in output.lines() {
                lines.push(Line::styled(
                    format!("    {line}"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ));
            }
        }
    }

    // usage line
    if let Some(ref usage) = msg.usage {
        let cost_str = msg.cost.map(|c| format!(" | ${c:.4}")).unwrap_or_default();
        lines.push(Line::styled(
            format!(
                "  in:{} out:{} cache:{}{}",
                usage.input_tokens, usage.output_tokens, usage.cache_read_tokens, cost_str
            ),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
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
        let app = App::new("test".into());
        let buf = render_app(&app, 40, 10);
        // should be mostly empty
        let content = buffer_to_string(&buf);
        assert!(content.trim().is_empty());
    }

    #[test]
    fn user_message_renders() {
        let mut app = App::new("test".into());
        app.push_user_message("hello world".into());
        let buf = render_app(&app, 40, 10);
        let content = buffer_to_string(&buf);
        assert!(content.contains("you"));
        assert!(content.contains("hello world"));
    }

    #[test]
    fn assistant_message_renders() {
        let mut app = App::new("test".into());
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
        let mut app = App::new("test".into());
        app.start_streaming();
        app.push_text_delta("partial");
        // don't finish - still streaming
        let buf = render_app(&app, 40, 10);
        let content = buffer_to_string(&buf);
        assert!(content.contains("partial"));
    }

    #[test]
    fn tool_calls_render() {
        let mut app = App::new("test".into());
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
        let mut app = App::new("test".into());
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
