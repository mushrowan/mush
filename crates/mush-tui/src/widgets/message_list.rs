//! message list widget - renders conversation history

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

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

        for msg in &self.app.messages {
            render_message(msg, &mut lines);
            lines.push(Line::raw(""));
        }

        // streaming content
        if self.app.is_streaming {
            if !self.app.streaming_thinking.is_empty() {
                lines.push(Line::from(vec![Span::styled(
                    "  thinking ",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                )]));
                for line in self.app.streaming_thinking.lines() {
                    lines.push(Line::styled(
                        format!("  {line}"),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    ));
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
                lines.push(Line::styled("  ...", Style::default().fg(Color::DarkGray)));
            }
        }

        // active tool indicator
        if let Some(ref tool) = self.app.active_tool {
            lines.push(Line::from(vec![
                Span::styled("  ▶ ", Style::default().fg(Color::Cyan)),
                Span::styled(tool.as_str(), Style::default().fg(Color::Cyan)),
            ]));
        }

        let text = Text::from(lines);

        // scroll: show from bottom, offset by scroll_offset
        let total_lines = text.lines.len() as u16;
        let visible = area.height;
        let max_scroll = total_lines.saturating_sub(visible);
        let scroll = max_scroll.saturating_sub(self.app.scroll_offset);

        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .render(area, buf);
    }
}

fn render_message(msg: &DisplayMessage, lines: &mut Vec<Line<'_>>) {
    let (label, label_style) = match msg.role {
        MessageRole::User => (
            "you",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        MessageRole::Assistant => (
            "mush",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        MessageRole::System => ("system", Style::default().fg(Color::Yellow)),
    };

    lines.push(Line::from(vec![Span::styled(label, label_style)]));

    // thinking block (collapsed)
    if let Some(ref thinking) = msg.thinking {
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
        ]));
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
        assert!(content.contains("mush"));
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
                },
                crate::app::DisplayToolCall {
                    name: "read".into(),
                    summary: "src/main.rs".into(),
                    status: ToolCallStatus::Error,
                    output_preview: None,
                },
            ],
            thinking: None,
            usage: None,
            cost: None,
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
            usage: None,
            cost: None,
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
