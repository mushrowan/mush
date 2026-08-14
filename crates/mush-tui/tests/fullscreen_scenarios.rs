use chrono::{TimeDelta, Utc};
use mush_ai::oauth::usage::{OAuthUsage, UsageWindow};
use mush_ai::types::TokenCount;
use mush_tui::app::{App, DisplayMessage, MessageRole};
use mush_tui::screen::Screen;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn conversation(exchanges: usize) -> App {
    let mut app = App::new("claude-opus-4-5".into(), TokenCount::new(200_000));
    for i in 0..exchanges {
        app.push_user_message(format!("question {i}"));
        app.messages.push(DisplayMessage::new(
            MessageRole::Assistant,
            format!("answer {i}"),
        ));
    }
    app
}

fn rows(app: &App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut screen = Screen::new();
    terminal
        .draw(|frame| screen.render(app, frame.area(), frame.buffer_mut()))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[test]
fn the_dock_sits_on_the_bottom_rows() {
    let rows = rows(&conversation(20), 80, 24);

    assert!(rows[20].starts_with('─'), "input box top: {:?}", rows[20]);
    assert!(rows[21].contains('>'), "input line: {:?}", rows[21]);
    assert!(
        rows[22].starts_with('─'),
        "input box bottom: {:?}",
        rows[22]
    );
    assert!(rows[23].contains("c-o-4-5"), "status bar: {:?}", rows[23]);
    assert!(
        rows[..20].iter().any(|row| row.contains("answer 19")),
        "transcript above the dock: {rows:?}"
    );
}

#[test]
fn the_status_bar_keeps_its_usage_bars() {
    let mut app = conversation(2);
    app.oauth_usage_provider = Some("anthropic".into());
    app.oauth_usage = Some(OAuthUsage {
        five_hour: Some(UsageWindow {
            utilization: 5.0,
            resets_at: Utc::now() + TimeDelta::minutes(120),
        }),
        seven_day: Some(UsageWindow {
            utilization: 34.0,
            resets_at: Utc::now() + TimeDelta::days(3),
        }),
    });

    let status = rows(&app, 120, 24).pop().unwrap();

    assert!(status.contains("c-o-4-5"), "{status:?}");
    assert!(status.contains("thinking:"), "{status:?}");
    assert!(status.contains("5h"), "{status:?}");
    assert!(status.contains("7d"), "{status:?}");
    assert!(status.contains('░'), "usage bar: {status:?}");
}

#[test]
fn a_long_conversation_shows_its_newest_lines() {
    let screen = rows(&conversation(20), 80, 24).join("\n");

    assert!(screen.contains("answer 19"), "{screen}");
    assert!(!screen.contains("question 0"), "{screen}");
}

#[test]
fn new_output_stays_in_view() {
    let mut app = conversation(20);
    let before = rows(&app, 80, 24).join("\n");
    assert!(!before.contains("answer 20"));

    app.push_user_message("question 20");
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, "answer 20"));

    assert!(rows(&app, 80, 24).join("\n").contains("answer 20"));
}

#[test]
fn a_short_conversation_sits_at_the_top() {
    let rows = rows(&conversation(1), 80, 24);

    assert!(
        rows[..3].iter().any(|row| row.contains("question 0")),
        "the first exchange is at the top: {rows:?}"
    );
    assert!(
        rows[6..20].iter().all(String::is_empty),
        "blank below: {rows:?}"
    );
}

#[test]
fn narrowing_the_terminal_keeps_the_newest_line_in_view() {
    let mut app = conversation(20);
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "a reply long enough that sixty columns has to wrap it across more rows than a hundred does",
    ));

    let wide = rows(&app, 100, 24).join("\n");
    let narrow = rows(&app, 60, 24).join("\n");

    assert!(wide.contains("than a hundred does"), "{wide}");
    assert!(narrow.contains("than a hundred does"), "{narrow}");
}
