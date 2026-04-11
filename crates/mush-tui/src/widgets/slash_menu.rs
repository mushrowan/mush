//! slash command completion menu widget
//!
//! renders a popup above the input box showing matching commands with descriptions

use crate::app::SlashMenuState;
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};

/// render the slash menu as a popup above the input area
pub fn render(frame: &mut Frame, menu: &SlashMenuState, input_area: Rect, theme: &Theme) {
    let item_count = if menu.model_mode {
        menu.model_matches.len()
    } else {
        menu.matches.len()
    };
    let max_visible = 12.min(item_count);
    // popup sits above the input box
    let height = (max_visible + 2) as u16; // +2 for borders
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

    // scroll so selected item is visible
    let visible = inner.height as usize;
    let scroll = if menu.selected >= visible {
        menu.selected - visible + 1
    } else {
        0
    };

    let mut lines: Vec<Line<'_>> = Vec::new();
    if menu.model_mode {
        for (i, model) in menu
            .model_matches
            .iter()
            .enumerate()
            .skip(scroll)
            .take(visible)
        {
            let is_selected = i == menu.selected;

            let id_style = if is_selected {
                theme.picker_selected.add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let prefix = if is_selected { "▸ " } else { "  " };

            let id_text = format!("/model {}", model.id);
            let pad = 34usize.saturating_sub(id_text.len() + prefix.len());

            lines.push(Line::from(vec![
                Span::styled(prefix, id_style),
                Span::styled(id_text, id_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(&model.name, theme.menu_description),
            ]));
        }
    } else {
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
    }

    let text = ratatui::text::Text::from(lines);
    frame.render_widget(text, inner);
}
