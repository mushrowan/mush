//! login picker overlay widget
//!
//! mirrors the session picker shape: a centred floating window with
//! filter / hint / list. each row shows a logged-in / logged-out badge
//! and the optional account id

use crate::login_picker::{LoginPickerState, filtered_entries};
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};

/// render the login picker as a centred overlay
pub fn render(frame: &mut Frame, picker: &LoginPickerState, theme: &Theme) {
    let area = frame.area();

    // 80% width, 60% height. clamp so the popup stays sensible on tiny
    // and giant terminals alike
    let width = (area.width * 80 / 100).clamp(40, 100);
    let height = (area.height * 60 / 100).clamp(10, 30);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" login ")
        .borders(Borders::ALL)
        .border_style(theme.search_border)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height < 5 {
        return;
    }

    // filter line, replaced by an api-key entry prompt while one is
    // armed so the user types into a dedicated row instead of the
    // (now-stale) filter
    let prompt_line = if let Some(prompt) = picker.entry.as_ref() {
        let mask: String = "•".repeat(prompt.buffer.chars().count());
        let label = format!("paste key for {} » ", prompt.provider_name);
        Line::from(vec![
            Span::styled(label, theme.dim),
            Span::styled(mask, Style::default()),
        ])
    } else {
        Line::from(vec![
            Span::styled("filter: ", theme.dim),
            Span::styled(
                if picker.filter.is_empty() {
                    "…"
                } else {
                    &picker.filter
                },
                Style::default(),
            ),
        ])
    };
    frame.render_widget(prompt_line, Rect::new(inner.x, inner.y, inner.width, 1));

    // hint line. content shifts when an entry prompt is active because
    // the keybinds the user needs there are different
    let hint = if picker.entry.is_some() {
        Line::from(vec![
            Span::styled("type", theme.selection_marker),
            Span::styled(" key · ", theme.dim),
            Span::styled("enter", theme.selection_marker),
            Span::styled(" save · ", theme.dim),
            Span::styled("esc", theme.selection_marker),
            Span::styled(" cancel", theme.dim),
        ])
    } else {
        Line::from(vec![
            Span::styled("ctrl+j/k", theme.selection_marker),
            Span::styled(" nav · ", theme.dim),
            Span::styled("enter", theme.selection_marker),
            Span::styled(" login/logout · ", theme.dim),
            Span::styled("esc", theme.selection_marker),
            Span::styled(" close", theme.dim),
        ])
    };
    frame.render_widget(hint, Rect::new(inner.x, inner.y + 1, inner.width, 1));

    let toast_rows = u16::from(picker.toast.is_some());
    let list_height = inner.height.saturating_sub(2 + toast_rows);
    let list_area = Rect::new(inner.x, inner.y + 2, inner.width, list_height);
    render_list(frame, picker, list_area, theme);

    if let Some(toast) = picker.toast.as_deref() {
        let toast_y = inner.y + inner.height.saturating_sub(1);
        frame.render_widget(
            Line::from(Span::styled(toast, theme.menu_description)),
            Rect::new(inner.x, toast_y, inner.width, 1),
        );
    }
}

fn render_list(frame: &mut Frame, picker: &LoginPickerState, list_area: Rect, theme: &Theme) {
    if list_area.height == 0 {
        return;
    }
    let visible = list_area.height as usize;
    let entries = filtered_entries(picker);

    if entries.is_empty() {
        frame.render_widget(Line::styled("  no providers match", theme.dim), list_area);
        return;
    }

    let scroll = if picker.selected >= visible {
        picker.selected - visible + 1
    } else {
        0
    };

    let mut lines: Vec<Line<'_>> = Vec::new();
    for (i, entry) in entries.iter().enumerate().skip(scroll).take(visible) {
        let is_selected = i == picker.selected;
        let row_style = if is_selected {
            theme.picker_selected.add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let prefix = if is_selected { "▸ " } else { "  " };
        let logged_in = entry.logged_in();
        // source-aware badge: shows where the credential lives so
        // users see at a glance whether mush owns the secret or it's
        // coming from env / config / oauth
        let badge_text = match entry.source.as_ref() {
            Some(source) => format!("{} ", source.badge()),
            None => "[logged out] ".into(),
        };
        let badge_style = if logged_in {
            theme.context_ok
        } else {
            theme.dim
        };

        // right-side metadata: account id when present, then row id
        // (so the human name dominates the row and the slug sits with
        // diagnostic info)
        let id_suffix = format!("  [{}]", entry.id);
        let account_suffix = entry
            .account_id
            .as_deref()
            .map(|acc| format!("  · {acc}"))
            .unwrap_or_default();
        let right = format!("{id_suffix}{account_suffix}");
        let right_width = right.chars().count();

        let name_len = entry.name.chars().count();
        let badge_len = badge_text.chars().count();
        let left_width = prefix.len() + badge_len + name_len;
        let pad = (list_area.width as usize)
            .saturating_sub(left_width + right_width)
            .max(1);

        lines.push(Line::from(vec![
            Span::styled(prefix, row_style),
            Span::styled(badge_text, badge_style),
            Span::styled(entry.name.clone(), row_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(right, theme.menu_description),
        ]));
    }

    let text = ratatui::text::Text::from(lines);
    frame.render_widget(text, list_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::login_picker::LoginEntry;
    use mush_ai::login::{LoginMethod, LoginSource};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn oauth_entry(id: &str, name: &str, source: Option<LoginSource>) -> LoginEntry {
        LoginEntry {
            id: id.into(),
            name: name.into(),
            method: LoginMethod::OAuth {
                oauth_provider_id: id.into(),
            },
            source,
            account_id: None,
        }
    }

    fn api_key_entry(id: &str, name: &str, source: Option<LoginSource>) -> LoginEntry {
        LoginEntry {
            id: id.into(),
            name: name.into(),
            method: LoginMethod::ApiKey {
                storage_key: id.into(),
                env_var: format!("{}_API_KEY", id.to_uppercase().replace('-', "_")),
                config_key: id.into(),
            },
            source,
            account_id: None,
        }
    }

    fn render_to_buffer(picker: &LoginPickerState, width: u16, height: u16) -> Buffer {
        let theme = Theme::default();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, picker, &theme);
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
    fn render_shows_source_badge_per_row() {
        // every row carries a source badge that names where the
        // credential is coming from. a logged-out row says so, a
        // logged-in oauth row tags `[oauth]`, an env-sourced row
        // tags `[env]` and so on
        let picker = LoginPickerState::new(vec![
            oauth_entry("anthropic-pro-max", "Anthropic", Some(LoginSource::OAuth)),
            api_key_entry(
                "openrouter",
                "OpenRouter",
                Some(LoginSource::Env("OPENROUTER_API_KEY".into())),
            ),
            api_key_entry("openai-api", "OpenAI API", None),
        ]);
        let buf = render_to_buffer(&picker, 100, 18);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("[oauth]"),
            "oauth badge missing, got: {content}"
        );
        assert!(
            content.contains("[env]"),
            "env badge missing, got: {content}"
        );
        assert!(
            content.contains("[logged out]"),
            "logged-out badge missing, got: {content}"
        );
        assert!(
            content.contains("OpenRouter"),
            "human-readable provider name should render"
        );
    }

    #[test]
    fn render_shows_keybind_hint_line() {
        // the hint row must teach the keybinds the user needs without
        // pulling up a help screen
        let picker =
            LoginPickerState::new(vec![oauth_entry("anthropic-pro-max", "Anthropic", None)]);
        let buf = render_to_buffer(&picker, 80, 16);
        let content = buffer_to_string(&buf);
        for needle in ["ctrl+j/k", "enter", "esc"] {
            assert!(
                content.contains(needle),
                "hint must mention {needle}, got: {content}"
            );
        }
    }

    #[test]
    fn render_shows_toast_when_present() {
        let mut picker = LoginPickerState::new(vec![oauth_entry(
            "anthropic-pro-max",
            "Anthropic",
            Some(LoginSource::OAuth),
        )]);
        picker.arm_logout();
        let buf = render_to_buffer(&picker, 80, 16);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("y/n"),
            "armed logout toast should surface y/n hint, got: {content}"
        );
    }

    #[test]
    fn render_shows_account_id_when_present() {
        let entry = LoginEntry {
            id: "anthropic-pro-max".into(),
            name: "Anthropic".into(),
            method: LoginMethod::OAuth {
                oauth_provider_id: "anthropic".into(),
            },
            source: Some(LoginSource::OAuth),
            account_id: Some("acc-42".into()),
        };
        let mut picker = LoginPickerState::new(vec![entry]);
        picker.selected = 0;
        let buf = render_to_buffer(&picker, 80, 16);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("acc-42"),
            "account id should be visible on the row, got: {content}"
        );
    }

    #[test]
    fn render_shows_masked_entry_prompt_when_armed() {
        // when the picker is collecting an api key, the filter row is
        // replaced by a prompt and the typed characters render as •
        // so onlookers can't read the secret
        let mut picker =
            LoginPickerState::new(vec![api_key_entry("openrouter", "OpenRouter", None)]);
        picker.arm_entry();
        if let Some(prompt) = picker.entry.as_mut() {
            prompt.buffer.push_str("sk-or-v1-secret");
        }
        let buf = render_to_buffer(&picker, 100, 18);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("paste key for OpenRouter"),
            "prompt label should name the row, got: {content}"
        );
        assert!(
            content.contains("•"),
            "typed characters must render as bullets, got: {content}"
        );
        assert!(
            !content.contains("sk-or-v1-secret"),
            "raw secret must not leak into the buffer, got: {content}"
        );
        // hint shifts to entry-mode keybinds
        assert!(
            content.contains("save"),
            "entry hint should mention save, got: {content}"
        );
    }
}
