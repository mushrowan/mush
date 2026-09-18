use crate::message_picker::{MessagePickerState, filtered_items};
use crate::theme::Theme;
use crate::widgets::popup_layout::{PopupSize, centred_popup};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};

pub fn render(frame: &mut Frame, picker: &MessagePickerState, theme: &Theme) {
    let popup = centred_popup(
        frame.area(),
        PopupSize {
            width_pct: 80,
            height_pct: 60,
            width_min: 40,
            width_max: 100,
            height_min: 10,
            height_max: 30,
        },
    );

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(picker.purpose.title())
        .borders(Borders::ALL)
        .border_style(theme.search_border)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height < 4 {
        return;
    }

    let filter_line = Line::from(vec![
        Span::styled("filter: ", theme.dim),
        Span::styled(
            if picker.filter.is_empty() {
                "…"
            } else {
                &picker.filter
            },
            Style::default(),
        ),
    ]);
    frame.render_widget(filter_line, Rect::new(inner.x, inner.y, inner.width, 1));

    let hint = Line::from(vec![
        Span::styled("↑↓", theme.selection_marker),
        Span::styled(" navigate  ", theme.dim),
        Span::styled("enter", theme.selection_marker),
        Span::styled(format!(" {} before it  ", picker.purpose.verb()), theme.dim),
        Span::styled("esc", theme.selection_marker),
        Span::styled(" close", theme.dim),
    ]);
    frame.render_widget(hint, Rect::new(inner.x, inner.y + 1, inner.width, 1));

    let items = filtered_items(picker);
    let list_area = Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 2);
    let visible = list_area.height as usize;
    let scroll = picker.selected.saturating_sub(visible.saturating_sub(1));
    let number_width = picker
        .items
        .last()
        .map_or(1, |item| item.index.to_string().len());

    let mut lines: Vec<Line<'_>> = Vec::new();
    for (i, item) in items.iter().enumerate().skip(scroll).take(visible) {
        let is_selected = i == picker.selected;
        let prefix = if is_selected { "▸ " } else { "  " };
        let number = format!("{:>number_width$}  ", item.index);
        let style = if is_selected {
            theme.picker_selected.add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let max_text = (list_area.width as usize).saturating_sub(prefix.len() + number.len());
        let text = if item.text.chars().count() > max_text {
            let t: String = item
                .text
                .chars()
                .take(max_text.saturating_sub(1).max(1))
                .collect();
            format!("{t}…")
        } else {
            item.text.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(number, theme.dim),
            Span::styled(text, style),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::styled("no matching messages", theme.dim));
    }
    frame.render_widget(ratatui::text::Text::from(lines), list_area);
}
