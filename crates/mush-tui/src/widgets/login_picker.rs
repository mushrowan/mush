use crate::login_picker::{LoginPickerState, filtered_entries};
use crate::theme::Theme;
use crate::widgets::popup_layout::{PICKER_SIZE, filter_line, key_hints, render_popup_frame};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

pub fn render(frame: &mut Frame, picker: &LoginPickerState, theme: &Theme) {
    let inner = render_popup_frame(frame, PICKER_SIZE, " login ", theme);

    if inner.height < 5 {
        return;
    }

    let prompt_line = filter_line(&picker.filter, theme);
    frame.render_widget(prompt_line, Rect::new(inner.x, inner.y, inner.width, 1));

    let hint = key_hints(
        &[
            ("ctrl+j/k", " nav · "),
            ("enter", " login/logout · "),
            ("esc", " close"),
        ],
        theme,
    );
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

    super::login_entry_popup::render(frame, picker, theme);
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

    let scroll = (picker.selected + 1).saturating_sub(visible);

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
        let badge_text = match entry.source.as_ref() {
            Some(source) => format!("{} ", source.badge()),
            None => "[logged out] ".into(),
        };
        let badge_style = if logged_in {
            theme.context_ok
        } else {
            theme.dim
        };

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
    use crate::test_support::buffer_to_string;
    use crate::test_support::draw_to_buffer;
    use mush_ai::login::{LoginMethod, LoginSource};

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
        draw_to_buffer(width, height, |frame| render(frame, picker, &theme))
    }

    #[test]
    fn render_does_not_panic_on_terminal_narrower_than_min_popup_width() {
        let picker = LoginPickerState::new(vec![oauth_entry(
            "anthropic-pro-max",
            "Anthropic",
            Some(LoginSource::OAuth),
        )]);
        for width in [10u16, 20, 30, 39, 40] {
            let _ = render_to_buffer(&picker, width, 26);
        }
    }

    #[test]
    fn render_shows_source_badge_per_row() {
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
    fn render_overlays_entry_popover_when_armed() {
        let mut picker =
            LoginPickerState::new(vec![api_key_entry("openrouter", "OpenRouter", None)]);
        picker.arm_entry();
        if let Some(prompt) = picker.entry.as_mut() {
            prompt.buffer.push_str("sk-or-v1-secret");
        }
        let buf = render_to_buffer(&picker, 100, 18);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("paste openrouter api key"),
            "popover title should name the row in lowercase, got: {content}"
        );
        assert!(
            content.contains("•"),
            "typed characters must render as bullets, got: {content}"
        );
        assert!(
            !content.contains("sk-or-v1-secret"),
            "raw secret must not leak into the buffer, got: {content}"
        );
        assert!(
            content.contains("save"),
            "popover hint should mention save, got: {content}"
        );
    }
}
