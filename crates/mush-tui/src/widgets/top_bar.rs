use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use mush_ai::types::{Dollars, TokenCount};

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
    raw.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn spend_label(tokens: TokenCount, cost: Dollars) -> String {
    let tokens = format!("{} tok", crate::turn_activity::format_tokens(tokens.get()));
    let amount = cost.get();
    if amount.is_finite() && amount >= 0.0 {
        format!("{tokens} \u{b7} ${amount:.2}")
    } else {
        tokens
    }
}

fn cwd_name(cwd: &str) -> &str {
    std::path::Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(cwd)
}

pub fn top_bar_line(app: &App, width: u16) -> Line<'static> {
    let title = sanitise_title(app.session_title.as_deref().unwrap_or(cwd_name(&app.cwd)));
    if title.is_empty() {
        return Line::default();
    }
    let width = usize::from(width);
    let title_width = title.width();
    let start = width.saturating_sub(title_width) / 2;
    let mut spans = vec![
        Span::raw(" ".repeat(start)),
        Span::styled(title, app.theme.status_model),
    ];
    let (tokens, cost) = app.session_spend();
    spans.push(Span::styled(
        format!("  {}", spend_label(tokens, cost)),
        app.theme.dim,
    ));
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

    fn app() -> App {
        App::new("test".into(), TokenCount::new(200_000))
    }

    fn child_with_cost(dollars: f64) -> mush_runtime::ChildSummary {
        mush_runtime::ChildSummary {
            paused: None,
            id: "sub-a".into(),
            name: "a".into(),
            parent: None,
            depth: 1,
            session_id: "s".into(),
            model_id: "m".into(),
            thinking: None,
            status: mush_runtime::ChildStatus::Idle,
            spawned_at: mush_ai::types::Timestamp::zero(),
            updated_at: mush_ai::types::Timestamp::zero(),
            idle_since: mush_ai::types::Timestamp::zero(),
            answer: None,
            error: None,
            progress_note: None,
            usage: mush_ai::types::Usage {
                input_tokens: TokenCount::new(150_000),
                output_tokens: TokenCount::new(50_000),
                ..Default::default()
            },
            cost: mush_ai::types::Cost {
                input: Dollars::new(dollars),
                ..Default::default()
            },
            turns: 1,
            streaming: false,
            tool_running: None,
            awaiting_confirmation: None,
        }
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

        let row = render(&app, 41);

        assert_eq!(
            row.trim_end(),
            format!("{}hello  0 tok \u{b7} $0.00", " ".repeat(18))
        );
    }

    #[test]
    fn the_tokens_and_cost_trail_the_title() {
        let mut app = app();
        app.session_title = Some("hello".into());
        app.stats.total_cost = Dollars::new(1.2345);
        app.stats.total_tokens = TokenCount::new(45_000);

        let row = render(&app, 40);

        assert!(row.contains("hello  45k tok \u{b7} $1.23"), "{row:?}");
    }

    #[test]
    fn the_tokens_and_cost_count_subagent_spend() {
        let mut app = app();
        app.session_title = Some("hello".into());
        app.stats.total_cost = Dollars::new(1.0);
        app.stats.total_tokens = TokenCount::new(1_000_000);
        app.note_child(child_with_cost(0.5));

        let row = render(&app, 40);

        assert!(row.contains("hello  1.2m tok \u{b7} $1.50"), "{row:?}");
    }

    #[test]
    fn a_zero_cost_shows_as_zero() {
        let mut app = app();
        app.session_title = Some("hello".into());

        let row = render(&app, 40);

        assert!(row.contains("hello  0 tok \u{b7} $0.00"), "{row:?}");
    }

    #[test]
    fn no_title_falls_back_to_the_cwd_name() {
        let mut app = app();
        app.cwd = "~/dev/mush".into();

        let row = render(&app, 40);

        assert_eq!(row.trim(), "mush  0 tok \u{b7} $0.00");
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
