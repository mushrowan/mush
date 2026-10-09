use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::app::App;

#[must_use]
pub fn height(app: &App) -> u16 {
    u16::from(app.feature_hint().is_some())
}

pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let Some(hint) = app.feature_hint() else {
        return;
    };
    let mut spans = vec![Span::raw(" ")];
    spans.extend(crate::widgets::shimmer::shimmer_spans(
        "hint:",
        &app.theme.shimmer,
        app.shimmer_phase(),
        app.theme.dim,
    ));
    spans.push(Span::styled(format!(" {hint}"), app.theme.dim));
    Paragraph::new(Line::from(spans)).render(area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppMode;
    use mush_ai::types::TokenCount;

    #[test]
    fn an_open_question_takes_no_row_above_the_composer() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        assert_eq!(height(&app), 0);

        app.interaction.mode = AppMode::ToolConfirm;
        app.interaction.confirm_prompt = Some("bash rm -rf build".into());
        assert_eq!(height(&app), 0);
    }

    #[test]
    fn the_hint_row_shows_while_streaming() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.begin_turn();
        app.start_streaming();
        app.turn.backdate(std::time::Duration::from_secs(6));
        app.tick_animation();
        assert_eq!(height(&app), 1);
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        render(&app, area, &mut buf);
        let text = crate::test_support::buffer_to_string(&buf);
        assert!(text.starts_with(" hint: "), "{text}");
    }
}
