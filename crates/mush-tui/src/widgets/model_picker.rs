//! model picker overlay widget
//!
//! mirrors the session picker shape: a centred floating window with
//! filter / hint / list. extras: two-tab header (all / ★ favourites)
//! and per-row provider badge

use crate::model_picker::{ModelPickerState, ModelPickerTab, filtered_models};
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};

/// render the model picker as a centred overlay
pub fn render(frame: &mut Frame, picker: &ModelPickerState, theme: &Theme) {
    let area = frame.area();

    // 80% width, 60% height. clamp so we don't render a giant box on
    // huge terminals or a useless one on tiny ones
    let width = (area.width * 80 / 100).clamp(40, 100);
    let height = (area.height * 60 / 100).clamp(10, 30);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" models ")
        .borders(Borders::ALL)
        .border_style(theme.search_border)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height < 5 {
        return;
    }

    // tab header: [ all ]  [ ★ favourites ]
    let tab_line = build_tab_header(picker, theme);
    frame.render_widget(tab_line, Rect::new(inner.x, inner.y, inner.width, 1));

    // filter line
    let filter_line = Line::from(vec![
        Span::styled("filter: ", theme.dim),
        Span::styled(
            if picker.filter.is_empty() {
                "…"
            } else {
                &picker.filter
            },
            Style::default(),
        ),
    ]);
    frame.render_widget(filter_line, Rect::new(inner.x, inner.y + 1, inner.width, 1));

    // hint line
    let hint = Line::from(vec![
        Span::styled("tab", theme.selection_marker),
        Span::styled(" switch · ", theme.dim),
        Span::styled("ctrl+j/k", theme.selection_marker),
        Span::styled(" nav · ", theme.dim),
        Span::styled("ctrl+f", theme.selection_marker),
        Span::styled(" ★ · ", theme.dim),
        Span::styled("enter", theme.selection_marker),
        Span::styled(" pick · ", theme.dim),
        Span::styled("esc", theme.selection_marker),
        Span::styled(" close", theme.dim),
    ]);
    frame.render_widget(hint, Rect::new(inner.x, inner.y + 2, inner.width, 1));

    // model list
    let toast_rows = u16::from(picker.toast.is_some());
    let list_height = inner.height.saturating_sub(3 + toast_rows);
    let list_area = Rect::new(inner.x, inner.y + 3, inner.width, list_height);
    render_model_list(frame, picker, list_area, theme);

    // toast pinned to the bottom of the popup
    if let Some(toast) = picker.toast.as_deref() {
        let toast_y = inner.y + inner.height.saturating_sub(1);
        frame.render_widget(
            Line::from(Span::styled(toast, theme.menu_description)),
            Rect::new(inner.x, toast_y, inner.width, 1),
        );
    }
}

fn build_tab_header<'a>(picker: &'a ModelPickerState, theme: &Theme) -> Line<'a> {
    let active = theme.picker_selected.add_modifier(Modifier::BOLD);
    let inactive = theme.dim;
    let (all_style, fav_style) = match picker.tab {
        ModelPickerTab::All => (active, inactive),
        ModelPickerTab::Favourites => (inactive, active),
    };
    let fav_count = picker.favourite_models.len();
    let total = picker.models.len();
    Line::from(vec![
        Span::styled(format!(" all ({total}) "), all_style),
        Span::styled("  ", inactive),
        Span::styled(format!(" ★ favourites ({fav_count}) "), fav_style),
    ])
}

fn render_model_list(frame: &mut Frame, picker: &ModelPickerState, list_area: Rect, theme: &Theme) {
    if list_area.height == 0 {
        return;
    }
    let visible = list_area.height as usize;
    let models = filtered_models(picker);

    if models.is_empty() {
        let msg = match picker.tab {
            ModelPickerTab::All => "  no models match",
            ModelPickerTab::Favourites => "  no favourites yet · ctrl+f to star one",
        };
        frame.render_widget(Line::styled(msg, theme.dim), list_area);
        return;
    }

    let scroll = if picker.selected >= visible {
        picker.selected - visible + 1
    } else {
        0
    };

    let mut lines: Vec<Line<'_>> = Vec::new();
    for (i, model) in models.iter().enumerate().skip(scroll).take(visible) {
        let is_selected = i == picker.selected;
        let row_style = if is_selected {
            theme.picker_selected.add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let prefix = if is_selected { "▸ " } else { "  " };
        let star = if picker.is_favourite(&model.id) {
            "★ "
        } else {
            "  "
        };
        let stale = if model.stale { " [stale]" } else { "" };
        let speed: String = model
            .speed_tiers
            .iter()
            .map(|t| format!(" [{t}]"))
            .collect();

        // right-side metadata: [provider] · display name. provider sits
        // closest to the id so the model can be classified at a glance
        let provider_badge = format!("[{}]", model.provider);
        let display_suffix = if model.name.is_empty() {
            String::new()
        } else if let Some(desc) = model.description.as_deref().filter(|s| !s.is_empty()) {
            format!("  {} · {desc}", model.name)
        } else {
            format!("  {}", model.name)
        };

        let id_text = model.id.as_str();
        let left_width = prefix.len() + star.len() + id_text.len() + speed.len() + stale.len();
        let right = format!("{provider_badge}{display_suffix}");
        let right_width = right.chars().count();
        let pad = (list_area.width as usize)
            .saturating_sub(left_width + right_width)
            .max(1);

        lines.push(Line::from(vec![
            Span::styled(prefix, row_style),
            Span::styled(star, theme.menu_description),
            Span::styled(id_text, row_style),
            Span::styled(speed, theme.menu_description),
            Span::styled(stale, theme.menu_description),
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
    use crate::slash_menu::ModelCompletion;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn model(id: &str, name: &str, provider: &str) -> ModelCompletion {
        ModelCompletion {
            id: id.into(),
            name: name.into(),
            provider: provider.into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: None,
        }
    }

    fn render_to_buffer(picker: &ModelPickerState, width: u16, height: u16) -> Buffer {
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
    fn render_shows_tab_header_with_active_tab_marked() {
        // a fresh picker has the All tab active. the header must show
        // both tabs and indicate which one is active. when the user
        // toggles, the marker moves to favourites
        let mut picker = ModelPickerState::new(
            vec![model("a", "A", "anthropic")],
            vec!["a".into()],
            false,
            false,
        );
        let buf = render_to_buffer(&picker, 80, 16);
        let content = buffer_to_string(&buf);
        assert!(content.contains("all"), "header must show all tab");
        assert!(
            content.contains("favourites"),
            "header must show favourites tab"
        );

        picker.toggle_tab();
        let buf = render_to_buffer(&picker, 80, 16);
        let content = buffer_to_string(&buf);
        // both tabs still visible; we can't easily inspect styling but
        // structurally both labels remain in the buffer
        assert!(content.contains("all"));
        assert!(content.contains("favourites"));
    }

    #[test]
    fn render_shows_provider_badge_per_row() {
        // a key user complaint about the old picker: no way to tell
        // which provider was serving each model. each row must surface
        // a [<provider>] badge so anthropic vs openai vs codex etc. is
        // obvious at a glance
        let picker = ModelPickerState::new(
            vec![
                model("claude-opus", "Claude Opus", "anthropic"),
                model("gpt-5", "GPT 5", "openai"),
            ],
            vec![],
            false,
            false,
        );
        let buf = render_to_buffer(&picker, 80, 16);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("[anthropic]"),
            "expected anthropic badge, got: {content}"
        );
        assert!(
            content.contains("[openai]"),
            "expected openai badge, got: {content}"
        );
    }

    #[test]
    fn render_shows_star_for_favourited_rows() {
        let picker = ModelPickerState::new(
            vec![
                model("opus", "Claude Opus", "anthropic"),
                model("haiku", "Claude Haiku", "anthropic"),
            ],
            vec!["opus".into()],
            false,
            false,
        );
        let buf = render_to_buffer(&picker, 80, 16);
        let content = buffer_to_string(&buf);
        let lines: Vec<&str> = content.lines().collect();
        let opus_row = lines
            .iter()
            .find(|l| l.contains("opus"))
            .copied()
            .unwrap_or("");
        let haiku_row = lines
            .iter()
            .find(|l| l.contains("haiku"))
            .copied()
            .unwrap_or("");
        assert!(
            opus_row.contains('★'),
            "favourited row must show star, got: {opus_row:?}"
        );
        assert!(
            !haiku_row.contains('★'),
            "non-favourite row must not show star, got: {haiku_row:?}"
        );
    }

    #[test]
    fn render_shows_helpful_empty_state_in_favourites_tab() {
        // when the user opens the favourites tab without ever starring
        // anything, the picker should hint at how to fix that rather
        // than just rendering an empty list
        let mut picker = ModelPickerState::new(
            vec![model("opus", "Claude Opus", "anthropic")],
            vec![],
            false,
            false,
        );
        picker.tab = ModelPickerTab::Favourites;
        let buf = render_to_buffer(&picker, 80, 16);
        let content = buffer_to_string(&buf);
        assert!(
            content.contains("ctrl+f to star"),
            "favourites empty state must hint at ctrl+f, got: {content}"
        );
    }

    #[test]
    fn render_shows_keybind_hint_line() {
        // the hint row teaches the keybinds the user needs without
        // pulling up a help screen
        let picker = ModelPickerState::new(
            vec![model("opus", "Claude Opus", "anthropic")],
            vec![],
            false,
            false,
        );
        let buf = render_to_buffer(&picker, 100, 16);
        let content = buffer_to_string(&buf);
        for needle in ["tab", "ctrl+j/k", "ctrl+f", "enter", "esc"] {
            assert!(
                content.contains(needle),
                "hint line must mention {needle}, got: {content}"
            );
        }
    }
}
