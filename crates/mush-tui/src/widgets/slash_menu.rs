//! slash command completion menu widget
//!
//! renders a popup above the input box showing matching commands with descriptions

use crate::slash_menu::SlashMenuState;
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
    let toast_rows = usize::from(menu.toast.is_some());
    let max_visible = 12.min(item_count);
    // popup sits above the input box. +2 for borders, +1 more when a
    // toast needs its own line
    let height = (max_visible + toast_rows + 2) as u16;
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
            // two-char slot so favourited and non-favourited rows stay
            // vertically aligned
            let fav_marker = if menu.is_favourite(&model.id) {
                "★ "
            } else {
                "  "
            };

            let id_text = format!("/model {}", model.id);
            let stale_marker = if model.stale { " [stale]" } else { "" };
            let pad = 34usize.saturating_sub(
                id_text.len() + prefix.len() + fav_marker.len() + stale_marker.len(),
            );

            lines.push(Line::from(vec![
                Span::styled(prefix, id_style),
                Span::styled(fav_marker, theme.menu_description),
                Span::styled(id_text, id_style),
                Span::styled(stale_marker, theme.menu_description),
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

    // toast line at the bottom if present (takes the last visible slot)
    if let Some(toast) = menu.toast.as_deref() {
        lines.push(Line::from(Span::styled(toast, theme.menu_description)));
    }

    let text = ratatui::text::Text::from(lines);
    frame.render_widget(text, inner);
}

#[cfg(test)]
mod tests {
    use crate::slash_menu::{ModelCompletion, SlashMenuState};
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

    fn buffer_to_string(buf: &Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn model_row_shows_star_for_favourite() {
        let menu = SlashMenuState::for_models_with_favourites(
            vec![
                ModelCompletion {
                    id: "claude-opus".into(),
                    name: "Claude Opus".into(),
                    provider: "anthropic".into(),
                    stale: false,
                },
                ModelCompletion {
                    id: "gpt-5".into(),
                    name: "GPT 5".into(),
                    provider: "anthropic".into(),
                    stale: false,
                },
            ],
            vec!["claude-opus".into()],
            false,
        );
        let buf = render_menu(&menu, 60, 8);
        let content = buffer_to_string(&buf);
        // first row (selected + favourite) must show the star
        let lines: Vec<&str> = content.lines().collect();
        let fav_row = lines
            .iter()
            .find(|l| l.contains("claude-opus"))
            .unwrap_or(&"");
        let nonfav_row = lines.iter().find(|l| l.contains("gpt-5")).unwrap_or(&"");
        assert!(
            fav_row.contains("★"),
            "favourited row should render a ★ marker; got: {fav_row:?}"
        );
        assert!(
            !nonfav_row.contains("★"),
            "non-favourite row must not render ★; got: {nonfav_row:?}"
        );
    }

    #[test]
    fn locked_menu_renders_toast_when_set() {
        let mut menu = SlashMenuState::for_models_with_favourites(
            vec![ModelCompletion {
                id: "claude-opus".into(),
                name: "Claude Opus".into(),
                provider: "anthropic".into(),
                stale: false,
            }],
            Vec::new(),
            true,
        );
        menu.toast = Some("favourites locked by config.toml".into());
        let buf = render_menu(&menu, 60, 8);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("favourites locked"),
            "toast text should appear in the popup: {content:?}"
        );
    }

    #[test]
    fn stale_model_row_shows_stale_marker() {
        // a model that was discovered in a previous fetch but is no longer
        // returned by the upstream gets a `[stale]` suffix in the picker
        // so the user can spot deprecated/removed models at a glance
        let menu = SlashMenuState::for_models_with_favourites(
            vec![
                ModelCompletion {
                    id: "fresh-model".into(),
                    name: "Fresh".into(),
                    provider: "anthropic".into(),
                    stale: false,
                },
                ModelCompletion {
                    id: "removed-model".into(),
                    name: "Removed".into(),
                    provider: "anthropic".into(),
                    stale: true,
                },
            ],
            Vec::new(),
            false,
        );
        let buf = render_menu(&menu, 80, 8);
        let content = buffer_to_string(&buf);
        let lines: Vec<&str> = content.lines().collect();
        let stale_row = lines
            .iter()
            .find(|l| l.contains("removed-model"))
            .unwrap_or(&"");
        let fresh_row = lines
            .iter()
            .find(|l| l.contains("fresh-model"))
            .unwrap_or(&"");
        assert!(
            stale_row.contains("[stale]"),
            "stale row must render [stale] marker; got: {stale_row:?}"
        );
        assert!(
            !fresh_row.contains("[stale]"),
            "fresh row must not render [stale] marker; got: {fresh_row:?}"
        );
    }
}
