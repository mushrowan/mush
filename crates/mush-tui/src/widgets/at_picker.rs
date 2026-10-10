use crate::app_state::AtPickerState;
use crate::theme::Theme;
use crate::widgets::popup_layout::render_menu;
use ratatui::Frame;
use ratatui::layout::Rect;

pub fn render(frame: &mut Frame, picker: &AtPickerState, input_area: Rect, theme: &Theme) {
    let rows: Vec<(&str, &str)> = picker
        .matches
        .iter()
        .map(|template| {
            (
                template.name.as_str(),
                template.description.as_deref().unwrap_or(""),
            )
        })
        .collect();
    render_menu(frame, '@', &rows, picker.selected, input_area, theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::buffer_to_string;
    use mush_ext::{PromptTemplate, TemplateSource};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn template(name: &str) -> PromptTemplate {
        PromptTemplate {
            name: name.into(),
            description: Some(format!("description: {name}")),
            content: format!("body: {name}"),
            source: TemplateSource::User,
            path: std::path::PathBuf::from(format!("/tmp/{name}.md")),
        }
    }

    fn render_picker(picker: &AtPickerState, width: u16, height: u16) -> Buffer {
        let theme = Theme::default();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, height.saturating_sub(1), width, 1);
                render(frame, picker, area, &theme);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn first_symbol_x(buf: &Buffer, symbol: &str) -> Option<u16> {
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].symbol() == symbol {
                    return Some(x);
                }
            }
        }
        None
    }

    #[test]
    fn picker_shows_selected_marker_on_active_row() {
        let picker = AtPickerState {
            matches: vec![template("review"), template("review-pr")],
            selected: 1,
            trigger_start: 0,
            trigger_end: 4,
        };
        let buf = render_picker(&picker, 60, 8);
        let content = buffer_to_string(&buf);
        let lines: Vec<&str> = content.lines().collect();
        let marked = lines
            .iter()
            .find(|l| l.contains("@review-pr"))
            .copied()
            .unwrap_or("");
        let unmarked = lines
            .iter()
            .find(|l| l.contains("@review ") || l.ends_with("@review"))
            .copied()
            .unwrap_or("");
        assert!(
            marked.contains('▸'),
            "selected row should show ▸ marker, got {marked:?}"
        );
        assert!(
            !unmarked.contains('▸'),
            "non-selected rows must not show ▸, got {unmarked:?}"
        );
    }

    #[test]
    fn picker_aligns_description_after_wide_template_name() {
        let picker = AtPickerState {
            matches: vec![template("🍓")],
            selected: 0,
            trigger_start: 0,
            trigger_end: 2,
        };
        let buf = render_picker(&picker, 60, 5);

        assert_eq!(
            first_symbol_x(&buf, "d"),
            Some(18),
            "description should start 16 cells after the inner row starts"
        );
    }
}
