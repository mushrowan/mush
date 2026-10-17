use crate::login_picker::{EntryPrompt, LoginPickerState};
use crate::theme::Theme;
use crate::widgets::popup_layout::key_hints;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};

const POPUP_HEIGHT: u16 = 7;
const POPUP_WIDTH: u16 = 60;

pub fn render(frame: &mut Frame, picker: &LoginPickerState, theme: &Theme) {
    let Some(prompt) = picker.entry.as_ref() else {
        return;
    };
    let validating = picker
        .validating
        .as_deref()
        .is_some_and(|id| id == prompt.entry_id);

    let area = frame.area();
    let width = POPUP_WIDTH.min(area.width);
    let height = POPUP_HEIGHT.min(area.height);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);
    let title = format!(" paste {} api key ", prompt.provider_name.to_lowercase());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(theme.input_border)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height < 4 {
        return;
    }

    let mask: String = "•".repeat(crate::text::grapheme_count(&prompt.buffer));
    let input = Line::from(vec![
        Span::styled("» ", theme.dim),
        Span::styled(mask, Style::default()),
    ]);
    frame.render_widget(input, Rect::new(inner.x, inner.y, inner.width, 1));

    let status_y = inner.y + 2;
    if status_y < inner.y + inner.height {
        let status = status_line(prompt, validating, theme);
        frame.render_widget(status, Rect::new(inner.x, status_y, inner.width, 1));
    }

    let hint_y = inner.y + inner.height.saturating_sub(1);
    if hint_y > status_y {
        let hint = if validating {
            Line::from(Span::styled("esc cancel", theme.dim))
        } else {
            key_hints(&[("enter", " save · "), ("esc", " cancel")], theme)
        };
        frame.render_widget(hint, Rect::new(inner.x, hint_y, inner.width, 1));
    }
}

fn status_line<'a>(prompt: &'a EntryPrompt, validating: bool, theme: &Theme) -> Line<'a> {
    if validating {
        return Line::from(Span::styled("validating…", theme.dim));
    }
    if let Some(err) = prompt.error_message.as_deref() {
        return Line::from(Span::styled(err, theme.tool_error));
    }
    Line::from(Span::raw(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::login_picker::LoginEntry;
    use crate::test_support::buffer_to_string;
    use crate::test_support::draw_to_buffer;
    use mush_ai::login::LoginMethod;

    use ratatui::buffer::Buffer;

    fn api_key_entry(id: &str, name: &str) -> LoginEntry {
        LoginEntry {
            id: id.into(),
            name: name.into(),
            method: LoginMethod::ApiKey {
                storage_key: id.into(),
                env_var: format!("{}_API_KEY", id.to_uppercase().replace('-', "_")),
                config_key: id.into(),
            },
            source: None,
            account_id: None,
        }
    }

    fn render_to_buffer(picker: &LoginPickerState, width: u16, height: u16) -> Buffer {
        let theme = Theme::default();
        draw_to_buffer(width, height, |frame| render(frame, picker, &theme))
    }

    #[test]
    fn no_render_when_no_entry_prompt_is_armed() {
        let picker = LoginPickerState::new(vec![api_key_entry("openrouter", "OpenRouter")]);
        let buf = render_to_buffer(&picker, 80, 20);
        let content = buffer_to_string(&buf);
        assert!(
            !content.contains('┌') && !content.contains('└'),
            "no popover border should appear, got: {content}"
        );
    }

    #[test]
    fn renders_lowercase_title_with_provider_name() {
        let mut picker = LoginPickerState::new(vec![api_key_entry("openrouter", "OpenRouter")]);
        picker.arm_entry();
        let buf = render_to_buffer(&picker, 80, 20);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("paste openrouter api key"),
            "expected lowercase title, got: {content}"
        );
        assert!(
            !content.contains("Paste OpenRouter"),
            "title-case must not leak through, got: {content}"
        );
    }

    #[test]
    fn renders_masked_buffer_and_does_not_leak_secret() {
        let mut picker = LoginPickerState::new(vec![api_key_entry("openrouter", "OpenRouter")]);
        picker.arm_entry();
        if let Some(prompt) = picker.entry.as_mut() {
            prompt.buffer.push_str("sk-or-v1-secret");
        }
        let buf = render_to_buffer(&picker, 80, 20);
        let content = buffer_to_string(&buf);
        assert!(content.contains('•'), "masked bullets missing");
        assert!(
            !content.contains("sk-or-v1-secret"),
            "raw secret must not appear in render output, got: {content}"
        );
    }

    #[test]
    fn renders_validating_status_during_in_flight_request() {
        let mut picker = LoginPickerState::new(vec![api_key_entry("openrouter", "OpenRouter")]);
        picker.arm_entry();
        picker.begin_validation("openrouter", "OpenRouter");
        let buf = render_to_buffer(&picker, 80, 20);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("validating"),
            "validating status missing, got: {content}"
        );
    }

    #[test]
    fn renders_error_message_under_input_after_failed_validation() {
        let mut picker = LoginPickerState::new(vec![api_key_entry("openrouter", "OpenRouter")]);
        picker.arm_entry();
        picker.begin_validation("openrouter", "OpenRouter");
        picker.finish_validation_failure("openrouter", "OpenRouter", "key was rejected");
        let buf = render_to_buffer(&picker, 80, 20);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("key was rejected"),
            "rejection should appear inside the popover, got: {content}"
        );
    }

    #[test]
    fn renders_save_and_cancel_hints() {
        let mut picker = LoginPickerState::new(vec![api_key_entry("openrouter", "OpenRouter")]);
        picker.arm_entry();
        let buf = render_to_buffer(&picker, 80, 20);
        let content = buffer_to_string(&buf);
        for needle in ["enter", "save", "esc", "cancel"] {
            assert!(content.contains(needle), "hint missing {needle}");
        }
    }
}
