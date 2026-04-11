//! session picker overlay widget

use crate::app::{SessionPickerState, SessionScope, filtered_sessions};
use crate::path_utils::shorten_path;
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};

/// render the session picker as a centred overlay
pub fn render(frame: &mut Frame, picker: &SessionPickerState, theme: &Theme) {
    let area = frame.area();

    // centre the picker, 80% width, 60% height
    let width = (area.width * 80 / 100).clamp(40, 100);
    let height = (area.height * 60 / 100).clamp(10, 30);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    // clear the area behind the popup
    frame.render_widget(Clear, popup);

    // title shows current scope
    let title = match picker.scope {
        SessionScope::ThisDir => " sessions (this dir) ",
        SessionScope::AllDirs => " sessions (all) ",
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(theme.search_border)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height < 4 {
        return;
    }

    // filter line
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

    // hint line
    let scope_label = match picker.scope {
        SessionScope::ThisDir => "all dirs",
        SessionScope::AllDirs => "this dir",
    };
    let hint = Line::from(vec![
        Span::styled("tab", theme.selection_marker),
        Span::styled(format!(" {scope_label}  "), theme.dim),
        Span::styled("↑↓", theme.selection_marker),
        Span::styled(" navigate  ", theme.dim),
        Span::styled("enter", theme.selection_marker),
        Span::styled(" resume  ", theme.dim),
        Span::styled("esc", theme.selection_marker),
        Span::styled(" close", theme.dim),
    ]);
    frame.render_widget(hint, Rect::new(inner.x, inner.y + 1, inner.width, 1));

    // session list
    let sessions = filtered_sessions(picker);
    let list_area = Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 2);

    let show_cwd = picker.scope == SessionScope::AllDirs;

    // scroll the view if selected item is below visible area
    let visible = list_area.height as usize;
    let scroll = if picker.selected >= visible {
        picker.selected - visible + 1
    } else {
        0
    };

    let mut lines: Vec<Line<'_>> = Vec::new();
    for (i, meta) in sessions.iter().enumerate().skip(scroll).take(visible) {
        let is_selected = i == picker.selected;
        let title = meta.title.as_deref().unwrap_or("(untitled)");
        let age = meta.updated_at.age_display();
        let msgs = format!("{}msg", meta.message_count);

        let prefix = if is_selected { "▸ " } else { "  " };
        let style = if is_selected {
            theme.picker_selected.add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        // build right-side metadata
        let cwd_display = if show_cwd {
            format!("{}  ", shorten_path(&meta.cwd))
        } else {
            String::new()
        };
        let meta_right = format!("{cwd_display}{msgs}  {age}");
        let meta_width = meta_right.len() + 1; // +1 for spacing

        // truncate title to fit
        let max_title = (list_area.width as usize).saturating_sub(prefix.len() + meta_width);
        let title_display = if title.chars().count() > max_title {
            let t: String = title
                .chars()
                .take(max_title.saturating_sub(1).max(1))
                .collect();
            format!("{t}…")
        } else {
            title.to_string()
        };

        let padding = (list_area.width as usize)
            .saturating_sub(prefix.len() + title_display.len() + meta_right.len())
            .max(1);

        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(title_display, style),
            Span::raw(" ".repeat(padding)),
            Span::styled(meta_right, theme.dim),
        ]));
    }

    if sessions.is_empty() {
        let msg = match picker.scope {
            SessionScope::ThisDir => "  no sessions in this directory — press tab for all",
            SessionScope::AllDirs => "  no sessions found",
        };
        lines.push(Line::styled(msg, theme.dim));
    }

    let text = ratatui::text::Text::from(lines);
    frame.render_widget(text, list_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_non_home_path() {
        assert_eq!(shorten_path("/opt/project"), "/opt/project");
    }
}
