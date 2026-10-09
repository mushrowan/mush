use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Widget, Wrap};

use crate::app::App;
use crate::question::{Choice, Question};
use crate::theme::Theme;

const WIDTH_MIN: u16 = 30;
const WIDTH_MAX: u16 = 88;
const FRAME_ROWS: u16 = 2;
const FRAME_COLUMNS: u16 = 4;
const SCREEN_MARGIN_ROWS: u16 = 2;
const CHOICE_GAP: &str = "   ";

pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    let Some(question) = Question::open(app) else {
        return;
    };
    let theme = &app.theme;
    let width = (area.width - area.width / 4)
        .clamp(WIDTH_MIN, WIDTH_MAX)
        .min(area.width);
    let inner_width = width.saturating_sub(FRAME_COLUMNS);
    if inner_width == 0 || area.height < FRAME_ROWS + 1 {
        return;
    }

    let hints = hint_lines(question.choices(), inner_width, theme);
    let hint_rows = u16::try_from(hints.len()).unwrap_or(u16::MAX);
    let body = Paragraph::new(body(question, app)).wrap(Wrap { trim: false });
    let body_rows = u16::try_from(body.line_count(inner_width)).unwrap_or(u16::MAX);

    let tallest = area
        .height
        .saturating_sub(SCREEN_MARGIN_ROWS)
        .max(FRAME_ROWS + hint_rows)
        .min(area.height);
    let height = (FRAME_ROWS + body_rows + 1 + hint_rows).min(tallest);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );

    Clear.render(popup, buf);
    let block = Block::default()
        .title(Span::styled(
            question.title(),
            theme.confirm.add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(theme.confirm)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    block.render(popup, buf);

    let hint_rows = hint_rows.min(inner.height);
    Paragraph::new(hints).render(
        Rect::new(inner.x, inner.bottom() - hint_rows, inner.width, hint_rows),
        buf,
    );

    let room = inner.height - hint_rows;
    let shown = if room >= 2 {
        body_rows.min(room - 1)
    } else {
        room
    };
    if shown >= 2 && shown < body_rows {
        let kept = shown - 1;
        body.render(Rect::new(inner.x, inner.y, inner.width, kept), buf);
        Line::styled(format!("… {} more lines", body_rows - kept), theme.dim)
            .render(Rect::new(inner.x, inner.y + kept, inner.width, 1), buf);
    } else {
        body.render(Rect::new(inner.x, inner.y, inner.width, shown), buf);
    }
}

fn body(question: Question, app: &App) -> Vec<Line<'static>> {
    let headline = app.theme.confirm.add_modifier(Modifier::BOLD);
    match question {
        Question::Tool => {
            let prompt = app.interaction.confirm_prompt.as_deref().unwrap_or("");
            let (tool, detail) = prompt.split_once(' ').unwrap_or((prompt, ""));
            let tool = if tool.is_empty() { "a tool call" } else { tool };
            std::iter::once(Line::styled(tool.to_string(), headline))
                .chain(
                    detail
                        .trim_matches('\n')
                        .lines()
                        .map(|line| Line::raw(line.replace('\t', "    "))),
                )
                .collect()
        }
        Question::KernelBusy => vec![Line::styled(
            "an interrupted python cell is still running",
            headline,
        )],
        Question::BranchSummary => vec![Line::styled(
            "summarise the branch you're leaving?",
            headline,
        )],
    }
}

fn hint_lines(choices: &[Choice], width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let spans = |choice: &Choice| {
        [
            Span::styled(choice.keys, theme.selection_marker),
            Span::styled(format!(" {}", choice.label), theme.dim),
        ]
    };
    let one_row = Line::from(
        choices
            .iter()
            .enumerate()
            .flat_map(|(index, choice)| {
                let gap = (index > 0).then(|| Span::raw(CHOICE_GAP));
                gap.into_iter().chain(spans(choice))
            })
            .collect::<Vec<_>>(),
    );
    if one_row.width() <= usize::from(width) {
        return vec![one_row];
    }
    choices
        .iter()
        .map(|choice| Line::from(spans(choice).to_vec()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppMode;
    use crate::test_support::buffer_to_string;
    use mush_ai::types::TokenCount;

    fn asking(prompt: &str) -> App {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.interaction.mode = AppMode::ToolConfirm;
        app.interaction.confirm_prompt = Some(prompt.into());
        app
    }

    fn draw(app: &App, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        render(app, area, &mut buf);
        buffer_to_string(&buf)
    }

    #[test]
    fn nothing_draws_without_a_question() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        assert_eq!(draw(&app, 40, 10).trim(), "");
    }

    #[test]
    fn a_tool_question_renders_as_a_centred_box() {
        let app = asking("bash rm -rf build");
        let expected = [
            "                                        ",
            "                                        ",
            "     ┌ allow this tool call? ─────┐     ",
            "     │ bash                       │     ",
            "     │ rm -rf build               │     ",
            "     │                            │     ",
            "     │ y/enter allow   n/esc deny │     ",
            "     └────────────────────────────┘     ",
            "                                        ",
            "                                        ",
            "                                        ",
        ];
        let text = draw(&app, 40, 11);
        assert_eq!(text.lines().collect::<Vec<_>>(), expected, "\n{text}");
    }

    #[test]
    fn narrow_terminals_stack_the_choices() {
        let mut app = asking("");
        app.interaction.kernel_busy = crate::app_state::KernelBusy::Asked;
        let text = draw(&app, 24, 12);
        let rows: Vec<&str> = text.lines().collect();
        assert!(rows.iter().all(|row| row.chars().count() == 24));
        for needle in ["y/enter wait", "n kill and restart", "esc cancel"] {
            assert_eq!(
                rows.iter().filter(|row| row.contains(needle)).count(),
                1,
                "{needle}\n{text}"
            );
        }
        assert!(text.contains("python kernel busy"), "{text}");
    }

    #[test]
    fn long_code_is_cut_with_a_count_and_the_keys_stay_visible() {
        let code: Vec<String> = (0..40).map(|i| format!("print({i})")).collect();
        let app = asking(&format!("ipython {}", code.join("\n")));
        let text = draw(&app, 60, 16);
        assert!(text.contains("print(0)"), "{text}");
        assert!(!text.contains("print(39)"), "{text}");
        assert!(text.contains("more lines"), "{text}");
        assert!(text.contains("y/enter allow"), "{text}");
        assert!(text.contains("n/esc deny"), "{text}");
    }

    #[test]
    fn tiny_terminals_keep_the_keys_ahead_of_the_body() {
        let app = asking("bash echo hi");
        let text = draw(&app, 30, 4);
        assert!(text.contains("y/enter allow"), "{text}");
        assert_eq!(draw(&app, 30, 2).trim(), "");
    }
}
