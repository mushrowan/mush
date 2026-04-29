//! `@`-template picker popup
//!
//! renders a list of prompt-template candidates above the input area
//! when the user pressed tab on a partial-match `@<word>`. mirrors the
//! slash-command menu's shape so the visual language is consistent
//! (selected row prefixed with `▸`, description in dim style).

use crate::app_state::AtPickerState;
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};

/// render the picker as a popup above the input area
pub fn render(frame: &mut Frame, picker: &AtPickerState, input_area: Rect, theme: &Theme) {
    let item_count = picker.matches.len();
    let max_visible = 12.min(item_count);
    // popup sits above the input box. +2 for borders
    let height = (max_visible + 2) as u16;
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

    // scroll so the selected item stays visible
    let visible = inner.height as usize;
    let scroll = if picker.selected >= visible {
        picker.selected - visible + 1
    } else {
        0
    };

    let mut lines: Vec<Line<'_>> = Vec::new();
    for (i, template) in picker.matches.iter().enumerate().skip(scroll).take(visible) {
        let is_selected = i == picker.selected;
        let name_style = if is_selected {
            theme.picker_selected.add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let prefix = if is_selected { "▸ " } else { "  " };
        let name_text = format!("@{}", template.name);
        let pad = 16usize.saturating_sub(name_text.len() + prefix.len());

        lines.push(Line::from(vec![
            Span::styled(prefix, name_style),
            Span::styled(name_text, name_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(
                template.description.as_deref().unwrap_or(""),
                theme.menu_description,
            ),
        ]));
    }

    let text = ratatui::text::Text::from(lines);
    frame.render_widget(text, inner);
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
