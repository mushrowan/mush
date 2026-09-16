use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use mush_ai::types::Dollars;

use crate::app::App;

pub const TOP_BAR_HEIGHT: u16 = 1;

pub struct TopBar<'a> {
    app: &'a App,
}

impl<'a> TopBar<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }
}

pub fn sanitise_title(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn cost_label(cost: Dollars) -> Option<String> {
    let amount = cost.get();
    (amount.is_finite() && amount > 0.0).then(|| format!("${amount:.2}"))
}

pub fn top_bar_line(app: &App, width: u16) -> Line<'static> {
    let Some(title) = app
        .session_title
        .as_deref()
        .map(sanitise_title)
        .filter(|t| !t.is_empty())
    else {
        return Line::default();
    };
    let width = usize::from(width);
    let title_width = title.width();
    let start = width.saturating_sub(title_width) / 2;
    let mut spans = vec![
        Span::raw(" ".repeat(start)),
        Span::styled(title, app.theme.status_model),
    ];
    if let Some(cost) = cost_label(app.stats.total_cost) {
        spans.push(Span::styled(format!("  {cost}"), app.theme.dim));
    }
    Line::from(spans)
}

impl Widget for TopBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let row = Rect { height: 1, ..area };
        top_bar_line(self.app, area.width).render(row, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::buffer_to_string;
    use mush_ai::types::TokenCount;

    fn app() -> App {
        App::new("test".into(), TokenCount::new(200_000))
    }

    fn render(app: &App, width: u16) -> String {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));
        TopBar::new(app).render(Rect::new(0, 0, width, 1), &mut buf);
        buffer_to_string(&buf).trim_end_matches('\n').to_string()
    }

    #[test]
    fn the_title_is_centred() {
        let mut app = app();
        app.session_title = Some("hello".into());

        let row = render(&app, 21);

        assert_eq!(row.trim_end(), format!("{}hello", " ".repeat(8)));
    }

    #[test]
    fn the_cost_trails_the_title() {
        let mut app = app();
        app.session_title = Some("hello".into());
        app.stats.total_cost = Dollars::new(1.2345);

        let row = render(&app, 40);

        assert!(row.contains("hello  $1.23"), "{row:?}");
    }

    #[test]
    fn a_zero_cost_is_not_shown() {
        let mut app = app();
        app.session_title = Some("hello".into());

        let row = render(&app, 40);

        assert!(!row.contains('$'), "{row:?}");
    }

    #[test]
    fn no_title_renders_a_blank_row() {
        let app = app();

        let row = render(&app, 40);

        assert_eq!(row.trim(), "");
    }

    #[test]
    fn control_characters_and_runs_of_whitespace_are_collapsed() {
        assert_eq!(sanitise_title("  a\x07b\n\tc  "), "a b c");
    }

    #[test]
    fn a_title_of_only_control_characters_renders_blank() {
        let mut app = app();
        app.session_title = Some("\x07\n".into());

        let row = render(&app, 40);

        assert_eq!(row.trim(), "");
    }

    #[test]
    fn a_title_wider_than_the_bar_is_clipped() {
        let mut app = app();
        app.session_title = Some("x".repeat(60));

        let row = render(&app, 20);

        assert_eq!(row.len(), 20);
        assert_eq!(row, "x".repeat(20));
    }
}
