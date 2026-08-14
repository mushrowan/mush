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
