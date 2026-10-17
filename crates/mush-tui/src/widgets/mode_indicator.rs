use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::app::{App, AppMode};
use crate::screen::SelectionKind;

fn mode(app: &App) -> Option<(&'static str, Style)> {
    let theme = &app.theme;
    match app.interaction.mode {
        AppMode::Normal => Some(("NORMAL", theme.mode_normal)),
        AppMode::Visual => {
            let kind = if app.interaction.focus.is_log() {
                app.viewport.selection_kind()
            } else {
                app.input.selection_kind
            };
            let label = match kind {
                SelectionKind::Char => "VISUAL",
                SelectionKind::Line => "V-LINE",
                SelectionKind::Block => "V-BLOCK",
            };
            Some((label, theme.mode_visual))
        }
        _ => None,
    }
}

#[must_use]
pub fn mode_label(app: &App) -> Option<Line<'static>> {
    let (label, style) = mode(app)?;
    let border = mode_border(app).unwrap_or_default();
    let mut spans = vec![
        Span::styled("\u{2500}", border),
        Span::styled(format!(" {label} "), style),
    ];
    if app.interaction.focus.is_log() {
        spans.push(Span::styled(" log", app.theme.dim));
    }
    if let Some(typed) = app.interaction.normal.keys.typed() {
        spans.push(Span::styled(format!(" {typed}"), border));
    }
    if spans.len() > 2 {
        spans.push(Span::raw(" "));
    }
    Some(Line::from(spans))
}

#[must_use]
pub fn mode_border(app: &App) -> Option<Style> {
    let (_, style) = mode(app)?;
    Some(Style::default().fg(style.bg?))
}
