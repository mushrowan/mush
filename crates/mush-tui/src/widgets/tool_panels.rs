use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use crate::app::{ActiveToolState, ToolCallStatus};
use crate::path_utils::truncate_path;
use crate::theme::Theme;

const MIN_PANEL_WIDTH: u16 = 30;

pub struct ToolPanels<'a> {
    tools: &'a [ActiveToolState],
    theme: &'a Theme,
}

impl<'a> ToolPanels<'a> {
    pub fn new(tools: &'a [ActiveToolState], theme: &'a Theme) -> Self {
        Self { tools, theme }
    }
}

impl Widget for ToolPanels<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if self.tools.is_empty() || area.width < 4 || area.height < 3 {
            return;
        }

        let n = self.tools.len();
        let max_cols = ((area.width / MIN_PANEL_WIDTH) as usize).max(1);
        let cols = n.min(max_cols);
        let rows = n.div_ceil(cols);

        let row_constraints: Vec<Constraint> = (0..rows)
            .map(|_| Constraint::Ratio(1, rows as u32))
            .collect();
        let grid_rows = Layout::vertical(&row_constraints).split(area);
        for (row_idx, row_area) in grid_rows.iter().enumerate() {
            let start = row_idx * cols;
            let end = (start + cols).min(n);
            let row_tools = &self.tools[start..end];
            let row_cols = row_tools.len();
            let col_constraints: Vec<Constraint> = (0..row_cols)
                .map(|_| Constraint::Ratio(1, row_cols as u32))
                .collect();
            let cells = Layout::horizontal(&col_constraints).split(*row_area);
            for (i, tool) in row_tools.iter().enumerate() {
                render_panel(tool, self.theme, cells[i], buf);
            }
        }
    }
}

fn render_panel(tool: &ActiveToolState, theme: &Theme, area: Rect, buf: &mut Buffer) {
    let (icon_span, border_colour) = match tool.status {
        ToolCallStatus::Running => (
            Span::styled("…", theme.tool_running),
            theme
                .input_border
                .fg
                .unwrap_or(ratatui::style::Color::DarkGray),
        ),
        ToolCallStatus::Done => (
            Span::styled("✓", theme.tool_done),
            theme.tool_done.fg.unwrap_or(ratatui::style::Color::Green),
        ),
        ToolCallStatus::Error => (
            Span::styled("✗", theme.tool_error),
            theme.tool_error.fg.unwrap_or(ratatui::style::Color::Red),
        ),
    };

    let title = Line::from(vec![
        Span::raw(" "),
        icon_span,
        Span::raw(" "),
        Span::styled(
            tool.name.as_str(),
            Style::default()
                .fg(border_colour)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_colour));

    let inner = block.inner(area);
    block.render(area, buf);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut lines: Vec<Line<'_>> = Vec::new();

    let summary = if is_path_summary_tool(&tool.name) {
        truncate_path(tool.summary.as_str(), inner.width as usize)
    } else {
        mush_agent::display::truncate_with_ellipsis(tool.summary.as_str(), inner.width as usize)
    };
    lines.push(Line::styled(summary, theme.dim));

    let output = match tool.status {
        ToolCallStatus::Running => tool.live_output.as_deref(),
        _ => tool.output.as_deref(),
    };
    if let Some(text) = output {
        lines.push(Line::raw(""));
        for line in text.lines() {
            let style = if line.starts_with("+ ") {
                theme.diff_added
            } else if line.starts_with("- ") {
                theme.diff_removed
            } else {
                theme.dim
            };
            lines.push(Line::styled(line, style));
        }
    }

    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(inner, buf);
}

fn is_path_summary_tool(name: &str) -> bool {
    matches!(
        name.to_lowercase().replace('_', "").as_str(),
        "read" | "write" | "edit" | "ls"
    )
}

pub fn tool_panels_height(tools: &[ActiveToolState], area_width: u16) -> u16 {
    if tools.is_empty() {
        return 0;
    }
    let n = tools.len();
    let has_output = tools
        .iter()
        .any(|t| t.output.is_some() || t.live_output.is_some());
    let max_cols = (area_width / MIN_PANEL_WIDTH).max(1) as usize;
    let cols = n.min(max_cols);
    let rows = n.div_ceil(cols);
    let per_row = if has_output { 8 } else { 5 };
    ((rows as u16) * per_row).min(12)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::buffer_to_string;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render_panels(tools: &[ActiveToolState], width: u16, height: u16) -> Buffer {
        let theme = Theme::default();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(ToolPanels::new(tools, &theme), area);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn single_tool_renders() {
        let tools = vec![ActiveToolState {
            tool_call_id: "tc1".into(),
            name: "Read".into(),
            summary: "src/main.rs".into(),
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        }];
        let buf = render_panels(&tools, 60, 5);
        let content = buffer_to_string(&buf);
        assert!(content.contains("Read"));
        assert!(content.contains("src/main.rs"));
    }

    #[test]
    fn two_tools_side_by_side() {
        let tools = vec![
            ActiveToolState {
                tool_call_id: "tc1".into(),
                name: "Read".into(),
                summary: "src/main.rs".into(),
                live_output: None,
                status: ToolCallStatus::Running,
                output: None,
            },
            ActiveToolState {
                tool_call_id: "tc2".into(),
                name: "Grep".into(),
                summary: "pattern: TODO".into(),
                live_output: None,
                status: ToolCallStatus::Running,
                output: None,
            },
        ];
        let buf = render_panels(&tools, 80, 5);
        let content = buffer_to_string(&buf);
        assert!(content.contains("Read"));
        assert!(content.contains("Grep"));
    }

    #[test]
    fn narrow_screen_stacks() {
        let tools = vec![
            ActiveToolState {
                tool_call_id: "tc1".into(),
                name: "Read".into(),
                summary: "file1".into(),
                live_output: None,
                status: ToolCallStatus::Running,
                output: None,
            },
            ActiveToolState {
                tool_call_id: "tc2".into(),
                name: "Grep".into(),
                summary: "file2".into(),
                live_output: None,
                status: ToolCallStatus::Running,
                output: None,
            },
        ];
        let buf = render_panels(&tools, 40, 10);
        let content = buffer_to_string(&buf);
        assert!(content.contains("Read"));
        assert!(content.contains("Grep"));
    }

    #[test]
    fn four_tools_render_as_2x2_grid_on_mid_width() {
        let make = |n: &str| ActiveToolState {
            tool_call_id: n.into(),
            name: n.into(),
            summary: format!("{n}.rs"),
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        };
        let tools = vec![make("Aa"), make("Bb"), make("Cc"), make("Dd")];
        let buf = render_panels(&tools, 80, 10);
        let content = buffer_to_string(&buf);
        for name in &["Aa", "Bb", "Cc", "Dd"] {
            assert!(content.contains(name), "missing {name}");
        }
        let top_border_rows = content.lines().filter(|l| l.contains("┌")).count();
        assert_eq!(
            top_border_rows, 2,
            "expected two grid rows with top borders, got:\n{content}"
        );
    }

    #[test]
    fn done_tool_shows_checkmark() {
        let tools = vec![ActiveToolState {
            tool_call_id: "tc1".into(),
            name: "Edit".into(),
            summary: "src/main.rs".into(),
            live_output: None,
            status: ToolCallStatus::Done,
            output: Some("- old line\n+ new line".into()),
        }];
        let buf = render_panels(&tools, 60, 8);
        let content = buffer_to_string(&buf);
        assert!(content.contains("✓"));
        assert!(content.contains("Edit"));
    }

    #[test]
    fn tool_panels_height_empty() {
        assert_eq!(tool_panels_height(&[], 80), 0);
    }

    #[test]
    fn tool_panels_height_side_by_side() {
        let tools = vec![
            ActiveToolState {
                tool_call_id: "tc1".into(),
                name: "a".into(),
                summary: "".into(),
                live_output: None,
                status: ToolCallStatus::Running,
                output: None,
            },
            ActiveToolState {
                tool_call_id: "tc2".into(),
                name: "b".into(),
                summary: "".into(),
                live_output: None,
                status: ToolCallStatus::Running,
                output: None,
            },
        ];
        assert_eq!(tool_panels_height(&tools, 80), 5);
    }

    #[test]
    fn tool_panels_height_with_output() {
        let tools = vec![ActiveToolState {
            tool_call_id: "tc1".into(),
            name: "a".into(),
            summary: "".into(),
            live_output: None,
            status: ToolCallStatus::Done,
            output: Some("result".into()),
        }];
        assert_eq!(tool_panels_height(&tools, 80), 8);
    }

    #[test]
    fn long_read_tool_path_truncates_from_head_keeping_filename() {
        let tools = vec![ActiveToolState {
            tool_call_id: "tc1".into(),
            name: "Read".into(),
            summary: "/home/user/dev/mush/crates/mush-tui/src/widgets/message_list.rs".into(),
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        }];
        let buf = render_panels(&tools, 30, 5);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("message_list.rs"),
            "expected filename in panel, got: {content}"
        );
        assert!(
            content.contains('…'),
            "expected leading ellipsis, got: {content}"
        );
    }

    #[test]
    fn tool_panels_height_stacked() {
        let tools = vec![
            ActiveToolState {
                tool_call_id: "tc1".into(),
                name: "a".into(),
                summary: "".into(),
                live_output: None,
                status: ToolCallStatus::Running,
                output: None,
            },
            ActiveToolState {
                tool_call_id: "tc2".into(),
                name: "b".into(),
                summary: "".into(),
                live_output: None,
                status: ToolCallStatus::Running,
                output: None,
            },
            ActiveToolState {
                tool_call_id: "tc3".into(),
                name: "c".into(),
                summary: "".into(),
                live_output: None,
                status: ToolCallStatus::Running,
                output: None,
            },
        ];
        assert_eq!(tool_panels_height(&tools, 40), 12);
    }
}
