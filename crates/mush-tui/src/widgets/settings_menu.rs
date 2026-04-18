//! /settings floating menu widget
//!
//! renders a centred overlay listing the runtime settings. navigation is
//! j/k or arrow keys; space/enter toggles the selected item (or cycles
//! through scope values). esc closes the menu.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Padding, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use crate::settings::{MenuItemKind, ScopedSettings, SettingsMenuState};
use crate::theme::Theme;

/// render the settings overlay centred on the frame
pub fn render(
    frame: &mut Frame,
    menu: &SettingsMenuState,
    settings: &ScopedSettings,
    theme: &Theme,
) {
    let area = frame.area();

    // centre the overlay, 80% width, 70% height (capped)
    let width = (area.width * 80 / 100).clamp(50, 90);
    let height = (area.height * 70 / 100).clamp(14, 20);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" settings ")
        .borders(Borders::ALL)
        .border_style(theme.search_border)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height < 4 || inner.width < 20 {
        return;
    }

    // hint line at the top
    let hint = Line::from(vec![
        Span::styled("↑↓/jk", theme.selection_marker),
        Span::styled(" nav  ", theme.dim),
        Span::styled("space/enter", theme.selection_marker),
        Span::styled(" toggle  ", theme.dim),
        Span::styled("esc", theme.selection_marker),
        Span::styled(" close", theme.dim),
    ]);
    frame.render_widget(hint, Rect::new(inner.x, inner.y, inner.width, 1));

    // divider
    if inner.height > 2 {
        let divider = Line::from(Span::styled("─".repeat(inner.width as usize), theme.dim));
        frame.render_widget(divider, Rect::new(inner.x, inner.y + 1, inner.width, 1));
    }

    // items area + scrollbar column
    let items_y = inner.y + 2;
    let items_height = inner.height.saturating_sub(3); // hint + divider + bottom desc
    let items_area = Rect::new(
        inner.x,
        items_y,
        inner.width.saturating_sub(1),
        items_height,
    );
    let scrollbar_area = Rect::new(inner.right().saturating_sub(1), items_y, 1, items_height);

    let items = MenuItemKind::all();
    let visible = items_area.height as usize;
    let scroll_offset = clamp_scroll(menu.selected, menu.scroll_offset, visible, items.len());

    // compute the longest label for alignment
    let label_width = items.iter().map(|k| k.label().len()).max().unwrap_or(0);

    for (i, item) in items.iter().enumerate().skip(scroll_offset).take(visible) {
        let y = items_area.y + (i - scroll_offset) as u16;
        let selected = i == menu.selected;
        let value = item.value(settings);
        let is_on = value == "on";
        let is_off = value == "off";

        let marker = if selected { "▶ " } else { "  " };
        let marker_style = if selected {
            theme.selection_marker
        } else {
            theme.dim
        };

        let label_style = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let value_style = if is_on {
            theme.context_ok
        } else if is_off {
            theme.dim
        } else {
            // enum like scope
            Style::default().add_modifier(Modifier::BOLD)
        };

        let label_padded = format!("{:<width$}", item.label(), width = label_width);
        let line = Line::from(vec![
            Span::styled(marker, marker_style),
            Span::styled(label_padded, label_style),
            Span::raw("  "),
            Span::styled(value, value_style),
        ]);
        frame.render_widget(line, Rect::new(items_area.x, y, items_area.width, 1));
    }

    // scrollbar (only render when content overflows)
    if items.len() > visible {
        let mut sb_state =
            ScrollbarState::new(items.len().saturating_sub(visible)).position(scroll_offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .style(theme.dim);
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut sb_state);
    }

    // description footer for the selected item
    if inner.height >= 4 {
        let desc = menu.current().description();
        let desc_line = Line::from(Span::styled(desc, theme.dim));
        frame.render_widget(
            desc_line,
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    }
}

/// adjust scroll so the selected item stays visible
fn clamp_scroll(selected: usize, current_offset: usize, visible: usize, total: usize) -> usize {
    if visible == 0 || total == 0 {
        return 0;
    }
    if selected < current_offset {
        selected
    } else if selected >= current_offset + visible {
        selected + 1 - visible
    } else {
        current_offset.min(total.saturating_sub(visible))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_scroll_keeps_selection_visible() {
        // selection scrolls up: new offset tracks selection
        assert_eq!(clamp_scroll(0, 3, 5, 10), 0);
        // selection within visible window: offset unchanged
        assert_eq!(clamp_scroll(4, 2, 5, 10), 2);
        // selection scrolls down off the bottom: offset advances
        assert_eq!(clamp_scroll(8, 2, 5, 10), 4);
    }
}
