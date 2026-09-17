use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::app::{App, AppMode};

#[must_use]
pub fn question(app: &App) -> Option<String> {
    if app.interaction.mode != AppMode::ToolConfirm {
        return None;
    }
    if app.interaction.kernel_busy == crate::app_state::KernelBusy::Asked {
        return Some(
            "interrupted python cell still running. y wait, n kill and restart, esc cancel".into(),
        );
    }
    Some(match app.interaction.confirm_prompt.as_deref() {
        Some(prompt) => format!("run {prompt}? y/n"),
        None => "confirm tool? y/n".into(),
    })
}

#[must_use]
pub fn height(app: &App) -> u16 {
    u16::from(question(app).is_some())
}

pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    let Some(text) = question(app) else {
        return;
    };
    if area.height == 0 {
        return;
    }
    Paragraph::new(Line::from(vec![
        Span::styled(
            " ? ",
            app.theme
                .confirm
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::styled(text, app.theme.confirm),
    ]))
    .render(area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::TokenCount;

    #[test]
    fn the_row_exists_only_while_a_question_is_open() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        assert_eq!(height(&app), 0);

        app.interaction.mode = AppMode::ToolConfirm;
        app.interaction.confirm_prompt = Some("bash rm -rf build".into());
        assert_eq!(height(&app), 1);
        assert_eq!(
            question(&app).as_deref(),
            Some("run bash rm -rf build? y/n")
        );

        app.interaction.kernel_busy = crate::app_state::KernelBusy::Asked;
        assert!(
            question(&app)
                .unwrap()
                .starts_with("interrupted python cell")
        );
    }

    #[test]
    fn the_question_renders_above_the_composer() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.interaction.mode = AppMode::ToolConfirm;
        app.interaction.confirm_prompt = Some("read a.txt".into());
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);

        render(&app, area, &mut buf);

        let text = crate::test_support::buffer_to_string(&buf);
        assert!(text.contains("? run read a.txt? y/n"), "{text}");
    }
}
