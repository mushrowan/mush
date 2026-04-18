//! side-by-side tool execution panels

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use throbber_widgets_tui::{BRAILLE_SIX, Throbber, ThrobberState, WhichUse};

use crate::app::{ActiveToolState, ToolCallStatus};
use crate::theme::Theme;

/// minimum width per tool panel before stacking vertically
const MIN_PANEL_WIDTH: u16 = 30;

/// renders active tools as side-by-side panels
pub struct ToolPanels<'a> {
    tools: &'a [ActiveToolState],
    throbber_state: &'a ThrobberState,
    theme: &'a Theme,
}

impl<'a> ToolPanels<'a> {
    pub fn new(
        tools: &'a [ActiveToolState],
        throbber_state: &'a ThrobberState,
        theme: &'a Theme,
    ) -> Self {
        Self {
            tools,
            throbber_state,
            theme,
        }
    }
}

impl Widget for ToolPanels<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if self.tools.is_empty() || area.width < 4 || area.height < 3 {
            return;
        }

        let n = self.tools.len();
        // lay out as a grid: cols capped by area.width / MIN_PANEL_WIDTH,
        // rows = ceil(n / cols). this gives 2x2 for 4 tools on mid-width
        // terminals instead of a strict horizontal or vertical strip
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
                render_panel(tool, self.throbber_state, self.theme, cells[i], buf);
            }
        }
    }
}

fn render_panel(
    tool: &ActiveToolState,
    throbber_state: &ThrobberState,
    theme: &Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    let (icon_span, border_colour) = match tool.status {
        ToolCallStatus::Running => {
            let throbber = Throbber::default()
                .throbber_set(BRAILLE_SIX)
                .use_type(WhichUse::Spin);
            let spinner = throbber.to_symbol_span(throbber_state);
            (
                spinner.style(theme.tool_running),
                theme
                    .input_border
                    .fg
                    .unwrap_or(ratatui::style::Color::DarkGray),
            )
        }
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

    // summary line: truncate to panel width since ratatui won't wrap for us.
    // summarise_tool_args returns the full command; we cap it here
    let summary =
        mush_agent::display::truncate_with_ellipsis(tool.summary.as_str(), inner.width as usize);
    lines.push(Line::styled(summary, theme.dim));

    // show output: live output for running, final output for done
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

/// compute the height needed for the tool panels area
pub fn tool_panels_height(tools: &[ActiveToolState], area_width: u16) -> u16 {
    if tools.is_empty() {
        return 0;
    }
    let n = tools.len();
    let has_output = tools
        .iter()
        .any(|t| t.output.is_some() || t.live_output.is_some());
    // grid layout: cols capped by area_width / MIN_PANEL_WIDTH, rows = ceil(n/cols)
    let max_cols = (area_width / MIN_PANEL_WIDTH).max(1) as usize;
    let cols = n.min(max_cols);
    let rows = n.div_ceil(cols);
    let per_row = if has_output { 8 } else { 5 };
    ((rows as u16) * per_row).min(12)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render_panels(tools: &[ActiveToolState], width: u16, height: u16) -> Buffer {
        let state = ThrobberState::default();
        let theme = Theme::default();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(ToolPanels::new(tools, &state, &theme), area);
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
        // 80 wide: 40 each, > MIN_PANEL_WIDTH
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
        // 40 wide: 20 each, < MIN_PANEL_WIDTH, should stack
        let buf = render_panels(&tools, 40, 10);
        let content = buffer_to_string(&buf);
        assert!(content.contains("Read"));
        assert!(content.contains("Grep"));
    }

    #[test]
    fn four_tools_render_as_2x2_grid_on_mid_width() {
        // 80 wide: 80/30 = 2 cols, 4 tools → 2 rows of 2. both grid rows
        // should show their respective tool names on the top border
        let make = |n: &str| ActiveToolState {
            tool_call_id: n.into(),
            name: n.into(),
            summary: format!("{n}.rs"),
            live_output: None,
            status: ToolCallStatus::Running,
            output: None,
        };
        let tools = vec![make("Aa"), make("Bb"), make("Cc"), make("Dd")];
        // need enough height for 2 rows of 5 = 10 rows
        let buf = render_panels(&tools, 80, 10);
        let content = buffer_to_string(&buf);
        for name in &["Aa", "Bb", "Cc", "Dd"] {
            assert!(content.contains(name), "missing {name}");
        }
        // first and second grid rows each have their own top border (with ┌
        // corners). count how many rows contain "┌" markers
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
        // 3 tools on a 40-wide screen: 40/3 = 13 < 30, stacks
        assert_eq!(tool_panels_height(&tools, 40), 12);
    }
}
