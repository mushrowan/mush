use crate::path_utils::shorten_path;
use crate::session_picker::{SessionPickerState, SessionScope, filtered_sessions};
use crate::theme::Theme;
use crate::widgets::popup_layout::{PICKER_SIZE, filter_line, render_popup_frame, truncate_chars};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

pub fn render(frame: &mut Frame, picker: &SessionPickerState, theme: &Theme) {
    let title = match (picker.scope, picker.loading) {
        (SessionScope::ThisDir, false) => " sessions (this dir) ",
        (SessionScope::AllDirs, false) => " sessions (all) ",
        (SessionScope::ThisDir, true) => " sessions (this dir) · loading… ",
        (SessionScope::AllDirs, true) => " sessions (all) · loading… ",
    };

    let inner = render_popup_frame(frame, PICKER_SIZE, title, theme);

    if inner.height < 4 {
        return;
    }

    let filter_line = filter_line(&picker.filter, theme);
    frame.render_widget(filter_line, Rect::new(inner.x, inner.y, inner.width, 1));

    let scope_label = match picker.scope {
        SessionScope::ThisDir => "all dirs",
        SessionScope::AllDirs => "this dir",
    };
    let hint = if let Some(meta) = &picker.confirm_delete {
        Line::from(vec![
            Span::styled(
                format!("delete {}? ", meta.title.as_deref().unwrap_or("(untitled)")),
                theme.tool_error,
            ),
            Span::styled("y", theme.selection_marker),
            Span::styled(" yes  ", theme.dim),
            Span::styled("any other key", theme.selection_marker),
            Span::styled(" keep", theme.dim),
        ])
    } else if let Some(toast) = &picker.toast {
        Line::styled(toast.as_str(), theme.dim)
    } else {
        Line::from(vec![
            Span::styled("tab", theme.selection_marker),
            Span::styled(format!(" {scope_label}  "), theme.dim),
            Span::styled("↑↓", theme.selection_marker),
            Span::styled(" navigate  ", theme.dim),
            Span::styled("enter", theme.selection_marker),
            Span::styled(" resume  ", theme.dim),
            Span::styled("ctrl+d", theme.selection_marker),
            Span::styled(" delete  ", theme.dim),
            Span::styled("esc", theme.selection_marker),
            Span::styled(" close", theme.dim),
        ])
    };
    frame.render_widget(hint, Rect::new(inner.x, inner.y + 1, inner.width, 1));

    let sessions = filtered_sessions(picker);
    let list_area = Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 2);

    let show_cwd = picker.scope == SessionScope::AllDirs;

    let visible = list_area.height as usize;
    let scroll = (picker.selected + 1).saturating_sub(visible);

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

        let cwd_display = if show_cwd {
            format!("{}  ", shorten_path(&meta.cwd))
        } else {
            String::new()
        };
        let live = picker.live.get(&meta.id).copied();
        let live_label = live
            .map(|live| format!("{}  ", live.label()))
            .unwrap_or_default();
        let meta_right = format!("{cwd_display}{msgs}  {age}");
        let meta_width = live_label.len() + meta_right.len() + 1;

        let max_title = (list_area.width as usize).saturating_sub(prefix.len() + meta_width);
        let title_display = truncate_chars(title, max_title);

        let padding = (list_area.width as usize)
            .saturating_sub(
                prefix.len() + title_display.len() + live_label.len() + meta_right.len(),
            )
            .max(1);
        let live_style = match live {
            Some(
                crate::session_picker::Liveness::Stale | crate::session_picker::Liveness::Failed,
            ) => theme.tool_error,
            Some(crate::session_picker::Liveness::Working) => theme.selection_marker,
            Some(
                crate::session_picker::Liveness::Idle | crate::session_picker::Liveness::Paused,
            )
            | None => theme.dim,
        };

        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(title_display, style),
            Span::raw(" ".repeat(padding)),
            Span::styled(live_label, live_style),
            Span::styled(meta_right, theme.dim),
        ]));
    }

    if sessions.is_empty() {
        let msg = match picker.scope {
            SessionScope::ThisDir => "  no sessions in this directory · press tab for all",
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

    fn meta(id: &str, title: &str) -> mush_session::SessionMeta {
        mush_session::SessionMeta {
            id: mush_ai::types::SessionId::from(id.to_string()),
            title: Some(title.into()),
            model_id: "m".into(),
            created_at: mush_ai::types::Timestamp::now(),
            updated_at: mush_ai::types::Timestamp::now(),
            message_count: 3,
            cwd: "/tmp".into(),
            parent: None,
            subagent_model: None,
            accounts: Default::default(),
            paused: None,
            rlm_max_depth: None,
            lifecycle: mush_session::Lifecycle::Live,
        }
    }

    #[test]
    fn a_session_with_a_live_worker_shows_its_liveness() {
        let mut picker = SessionPickerState::new(
            vec![meta("hung", "hung agent"), meta("saved", "saved agent")],
            "/tmp".into(),
        );
        let (tx, rx) = std::sync::mpsc::channel();
        picker.watch_roster(rx);
        tx.send(vec![(
            mush_ai::types::SessionId::from("hung".to_string()),
            crate::session_picker::Liveness::Stale,
        )])
        .unwrap();
        assert!(picker.drain_incoming());

        let buffer = crate::test_support::draw_to_buffer(90, 20, |frame| {
            render(frame, &picker, &Theme::default());
        });
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let hung = rows.iter().find(|row| row.contains("hung agent")).unwrap();
        let saved = rows.iter().find(|row| row.contains("saved agent")).unwrap();
        assert!(hung.contains("stale"), "{hung}");
        assert!(!saved.contains("stale"), "{saved}");
    }

    #[test]
    fn shorten_non_home_path() {
        assert_eq!(shorten_path("/opt/project"), "/opt/project");
    }
}
