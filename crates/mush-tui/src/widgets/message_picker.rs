use crate::message_picker::{
    MessagePickerKind, MessagePickerPurpose, MessagePickerState, filtered_items,
};
use crate::theme::Theme;
use crate::widgets::popup_layout::{
    PICKER_SIZE, filter_line, key_hints, render_popup_frame, truncate_chars,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

pub fn render(frame: &mut Frame, picker: &MessagePickerState, theme: &Theme) {
    let inner = render_popup_frame(frame, PICKER_SIZE, picker.purpose.title(), theme);

    if inner.height < 4 {
        return;
    }

    let filter_line = filter_line(&picker.filter, theme);
    frame.render_widget(filter_line, Rect::new(inner.x, inner.y, inner.width, 1));

    let verb = format!(" {} before it  ", picker.purpose.verb());
    let hint = key_hints(
        &[("↑↓", " navigate  "), ("enter", &verb), ("esc", " close")],
        theme,
    );
    frame.render_widget(hint, Rect::new(inner.x, inner.y + 1, inner.width, 1));

    let items = filtered_items(picker);
    let list_area = Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 2);
    let visible = list_area.height as usize;
    let scroll = picker.selected.saturating_sub(visible.saturating_sub(1));
    let number_width = picker
        .items
        .last()
        .map_or(1, |item| item.index.to_string().len());

    let tree = picker.purpose == MessagePickerPurpose::Tree;
    let mut lines: Vec<Line<'_>> = Vec::new();
    for (i, item) in items.iter().enumerate().skip(scroll).take(visible) {
        let is_selected = i == picker.selected;
        let prefix = if is_selected { "▸ " } else { "  " };
        let number = format!("{:>number_width$}  ", item.index);
        let indent = if tree {
            "  ".repeat(item.depth)
        } else {
            String::new()
        };
        let tag = match (tree, item.kind) {
            (true, MessagePickerKind::BranchSummary) => "summary ",
            (true, MessagePickerKind::Compaction) => "compaction ",
            _ => "",
        };
        let fork = if tree && item.fork { " ⑂" } else { "" };
        let style = if is_selected {
            theme.picker_selected.add_modifier(Modifier::BOLD)
        } else if tree && !item.on_branch {
            theme.dim
        } else {
            Style::default()
        };
        let max_text = (list_area.width as usize).saturating_sub(
            prefix.len() + number.len() + indent.len() + tag.len() + fork.chars().count(),
        );
        let text = truncate_chars(&item.text, max_text);
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(number, theme.dim),
            Span::raw(indent),
            Span::styled(tag, theme.dim),
            Span::styled(text, style),
            Span::styled(fork, theme.dim),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::styled("no matching messages", theme.dim));
    }
    frame.render_widget(ratatui::text::Text::from(lines), list_area);
}
