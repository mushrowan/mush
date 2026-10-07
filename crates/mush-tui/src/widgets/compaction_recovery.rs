use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app_state::CompactionRecoveryState;
use crate::theme::Theme;
use crate::widgets::popup_layout::{PopupSize, key_hints, render_popup_frame};

pub fn render(frame: &mut Frame, state: &CompactionRecoveryState, theme: &Theme) {
    let inner = render_popup_frame(
        frame,
        PopupSize {
            width_pct: 80,
            height_pct: 55,
            width_min: 50,
            width_max: 100,
            height_min: 11,
            height_max: 18,
        },
        " compaction failed ",
        theme,
    );

    if inner.height < 5 || inner.width < 20 {
        return;
    }

    let reason_height = inner.height.saturating_sub(5).min(4);
    let reason = Paragraph::new(state.reason.as_str())
        .style(theme.menu_description)
        .wrap(Wrap { trim: true });
    frame.render_widget(
        reason,
        Rect::new(inner.x, inner.y, inner.width, reason_height),
    );

    let hint_y = inner.y + reason_height;
    let hint = key_hints(
        &[
            ("↑↓/jk", " choose  "),
            ("enter", " select  "),
            ("esc", " cancel"),
        ],
        theme,
    );
    frame.render_widget(hint, Rect::new(inner.x, hint_y, inner.width, 1));

    let options_y = hint_y + 1;
    let options_height = inner.bottom().saturating_sub(options_y);
    for (i, option) in state
        .options
        .iter()
        .enumerate()
        .take(options_height as usize)
    {
        let selected = i == state.selected;
        let marker = if selected { "▶ " } else { "  " };
        let label_style = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let line = Line::from(vec![
            Span::styled(marker, theme.selection_marker),
            Span::styled(option.label.as_str(), label_style),
            Span::styled(" - ", theme.dim),
            Span::styled(option.description.as_str(), theme.menu_description),
        ]);
        frame.render_widget(
            line,
            Rect::new(inner.x, options_y + i as u16, inner.width, 1),
        );
    }
}
