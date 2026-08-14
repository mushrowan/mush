use crate::slash_menu::SlashMenuState;
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};

const MAX_VISIBLE: usize = 12;

#[must_use]
pub fn height(menu: &SlashMenuState) -> u16 {
    u16::try_from(MAX_VISIBLE.min(menu.matches.len()) + 2).unwrap_or(u16::MAX)
}

pub fn render(frame: &mut Frame, menu: &SlashMenuState, input_area: Rect, theme: &Theme) {
    let height = height(menu);
    let width = input_area.width.min(80);
    let x = input_area.x;
    let y = input_area.y.saturating_sub(height);

    let popup = Rect::new(x, y, width, height);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.input_border)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let visible = inner.height as usize;
    let scroll = if menu.selected >= visible {
        menu.selected - visible + 1
    } else {
        0
    };

    let mut lines: Vec<Line<'_>> = Vec::new();
    for (i, cmd) in menu.matches.iter().enumerate().skip(scroll).take(visible) {
        let is_selected = i == menu.selected;

        let name_style = if is_selected {
            theme.picker_selected.add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let prefix = if is_selected { "▸ " } else { "  " };

        let name_text = format!("/{}", cmd.name);
        let pad = 16usize.saturating_sub(name_text.len() + prefix.len());

        lines.push(Line::from(vec![
            Span::styled(prefix, name_style),
            Span::styled(name_text, name_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(&cmd.description, theme.menu_description),
        ]));
    }

    let text = ratatui::text::Text::from(lines);
    frame.render_widget(text, inner);
}

#[cfg(test)]
mod tests {
    use crate::slash_menu::{SlashCommand, SlashMenuState};
    use crate::test_support::buffer_to_string;
    use crate::theme::Theme;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn render_menu(menu: &SlashMenuState, width: u16, height: u16) -> Buffer {
        let theme = Theme::default();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, height.saturating_sub(1), width, 1);
                super::render(frame, menu, area, &theme);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn command_row_shows_name_and_description() {
        let menu = SlashMenuState::for_commands(vec![SlashCommand {
            name: "help".into(),
            description: "show help".into(),
        }]);
        let buf = render_menu(&menu, 60, 6);
        let content = buffer_to_string(&buf);
        assert!(content.contains("/help"), "row must show /help");
        assert!(content.contains("show help"), "row must show description");
    }

    #[test]
    fn selected_row_renders_caret_marker() {
        let menu = SlashMenuState::for_commands(vec![
            SlashCommand {
                name: "help".into(),
                description: "show help".into(),
            },
            SlashCommand {
                name: "model".into(),
                description: "open model picker".into(),
            },
        ]);
        let buf = render_menu(&menu, 60, 6);
        let content = buffer_to_string(&buf);
        let lines: Vec<&str> = content.lines().collect();
        let selected_row = lines
            .iter()
            .find(|l| l.contains("/help"))
            .copied()
            .unwrap_or("");
        assert!(
            selected_row.contains('▸'),
            "selected row should show ▸, got: {selected_row:?}"
        );
    }
}
