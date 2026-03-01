//! session picker overlay widget

use crate::app::{SessionPickerState, filtered_sessions};
use mush_ai::types::Timestamp;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};

/// render the session picker as a centred overlay
pub fn render(frame: &mut Frame, picker: &SessionPickerState) {
    let area = frame.area();

    // centre the picker, 80% width, 60% height
    let width = (area.width * 80 / 100).clamp(40, 80);
    let height = (area.height * 60 / 100).clamp(10, 30);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    // clear the area behind the popup
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" sessions ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // filter line
    let filter_line = Line::from(vec![
        Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            if picker.filter.is_empty() {
                "…"
            } else {
                &picker.filter
            },
            Style::default().fg(Color::White),
        ),
    ]);

    if inner.height < 3 {
        return;
    }

    frame.render_widget(filter_line, Rect::new(inner.x, inner.y, inner.width, 1));

    // session list
    let sessions = filtered_sessions(picker);
    let list_area = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);

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
        let age = format_age(meta.updated_at);
        let msgs = format!("{}msg", meta.message_count);

        let prefix = if is_selected { "▸ " } else { "  " };
        let style = if is_selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let dim = Style::default().fg(Color::DarkGray);

        // truncate title to fit
        let meta_width = age.len() + msgs.len() + 4; // spacing
        let max_title = (list_area.width as usize).saturating_sub(prefix.len() + meta_width);
        let title_display = if title.len() > max_title {
            format!("{}…", &title[..max_title.saturating_sub(1)])
        } else {
            title.to_string()
        };

        let padding = list_area
            .width
            .saturating_sub((prefix.len() + title_display.len() + meta_width) as u16)
            .max(1) as usize;

        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(title_display, style),
            Span::raw(" ".repeat(padding)),
            Span::styled(msgs, dim),
            Span::styled("  ", dim),
            Span::styled(age, dim),
        ]));
    }

    if sessions.is_empty() {
        lines.push(Line::styled(
            "  no sessions found",
            Style::default().fg(Color::DarkGray),
        ));
    }

    let text = ratatui::text::Text::from(lines);
    frame.render_widget(text, list_area);
}

/// format a timestamp as a human-readable relative age
fn format_age(ts: Timestamp) -> String {
    let now = Timestamp::now().as_ms();
    let then = ts.as_ms();
    if now <= then {
        return "now".into();
    }
    let secs = (now - then) / 1000;
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    format!("{days}d ago")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_age_seconds() {
        let now = Timestamp::now().as_ms();
        let ts = Timestamp(now - 30_000);
        assert_eq!(format_age(ts), "30s ago");
    }

    #[test]
    fn format_age_minutes() {
        let now = Timestamp::now().as_ms();
        let ts = Timestamp(now - 300_000);
        assert_eq!(format_age(ts), "5m ago");
    }

    #[test]
    fn format_age_hours() {
        let now = Timestamp::now().as_ms();
        let ts = Timestamp(now - 7_200_000);
        assert_eq!(format_age(ts), "2h ago");
    }

    #[test]
    fn format_age_days() {
        let now = Timestamp::now().as_ms();
        let ts = Timestamp(now - 172_800_000);
        assert_eq!(format_age(ts), "2d ago");
    }
}
