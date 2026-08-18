use chrono::{TimeDelta, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use mush_ai::oauth::usage::{OAuthUsage, UsageWindow};
use mush_ai::types::TokenCount;
use mush_tui::AppEvent;
use mush_tui::app::{App, Caret, DisplayMessage, MessageRole};
use mush_tui::input::{handle_key, handle_mouse};
use mush_tui::keybinds::{BindingSource, ScopedKeysConfig};
use mush_tui::screen::Screen;
use mush_tui::slash_menu::ModelCompletion;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;

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

fn window_rows(app: &App) -> Vec<String> {
    rows(app, 80, 24)[..20]
        .iter()
        .map(|row| {
            row.chars()
                .take(79)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn press(app: &mut App, code: KeyCode) {
    handle_key(app, KeyEvent::from(code));
}

fn press_with(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    handle_key(app, KeyEvent::new(code, modifiers));
}

fn scoped_keys(scope: &str, action: &str, keys: &str) -> ScopedKeysConfig {
    let mut config = ScopedKeysConfig::default();
    config.scopes.insert(
        scope.to_string(),
        [(action.to_string(), BindingSource::One(keys.to_string()))]
            .into_iter()
            .collect(),
    );
    config
}

#[test]
fn esc_twice_on_an_empty_input_starts_reading() {
    let mut app = conversation(20);
    let before = window_rows(&app);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    let start = app.viewport.cursor_row();

    press(&mut app, KeyCode::Char('k'));

    assert_eq!(app.viewport.cursor_row(), start - 1, "the cursor moved");
    assert!(app.input.text.is_empty(), "the k was not typed");
    assert_eq!(window_rows(&app), before, "the window held still");
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
fn the_tail_follows_while_i_type() {
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

fn start_reading(app: &mut App) {
    rows(app, 80, 24);
    press(app, KeyCode::Esc);
    press(app, KeyCode::Esc);
}

#[test]
fn esc_leaves_insert_without_interrupting_work() {
    let mut app = conversation(20);
    app.start_streaming();
    rows(&app, 80, 24);

    press(&mut app, KeyCode::Esc);
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "cargo build line",
    ));

    assert_eq!(app.interaction.mode.caret(), Caret::ComposerBlock);
    assert!(rows(&app, 80, 24).join("\n").contains("cargo build line"));
}

#[test]
fn a_second_esc_moves_me_to_the_log() {
    let mut app = conversation(20);
    press(&mut app, KeyCode::Esc);

    press(&mut app, KeyCode::Esc);

    assert_eq!(app.interaction.mode.caret(), Caret::Log);
}

#[test]
fn a_third_esc_returns_to_insert_with_the_view_where_i_left_it() {
    let mut app = conversation(20);
    let bottom = window_rows(&app);
    start_reading(&mut app);
    press_with(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
    let scrolled = window_rows(&app);
    assert_ne!(scrolled, bottom, "the view moved off the tail");

    press(&mut app, KeyCode::Esc);
    for c in "ok".chars() {
        press(&mut app, KeyCode::Char(c));
    }

    assert_eq!(window_rows(&app), scrolled);
    assert_eq!(app.input.text, "ok");
}

#[test]
fn ctrl_o_reaches_the_log_in_one_press() {
    let mut app = conversation(20);
    app.start_streaming();

    press_with(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);

    assert_eq!(app.interaction.mode.caret(), Caret::Log);
}

#[test]
fn a_scrolled_view_does_not_follow() {
    let mut app = conversation(20);
    start_reading(&mut app);
    press_with(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Esc);
    let scrolled = window_rows(&app);

    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, "brand new"));

    assert_eq!(window_rows(&app), scrolled);
}

#[test]
fn the_composer_survives_the_whole_cycle() {
    let mut app = conversation(20);
    app.input.text = "explain the borrow checker".into();
    app.input.cursor = app.input.text.len();

    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);

    assert_eq!(app.input.text, "explain the borrow checker");
}

#[test]
fn esc_while_reading_goes_back_to_typing() {
    let mut app = conversation(20);
    start_reading(&mut app);

    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('k'));

    assert_eq!(app.input.text, "k");
}

#[test]
fn j_and_k_move_one_line() {
    let mut app = conversation(20);
    let before = window_rows(&app);
    start_reading(&mut app);
    let start = app.viewport.cursor_row();

    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('k'));
    let up = app.viewport.cursor_row();
    press(&mut app, KeyCode::Char('j'));

    assert_eq!(up, start - 2, "two rows up");
    assert_eq!(app.viewport.cursor_row(), start - 1, "one row back down");
    assert_eq!(
        window_rows(&app),
        before,
        "inside the window, it holds still"
    );
}

#[test]
fn the_page_keys_move_by_their_vim_amounts() {
    let mut app = conversation(20);
    let before = window_rows(&app);
    start_reading(&mut app);

    press_with(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
    let half = window_rows(&app);

    assert_eq!(half[10..20], before[..10], "half of a 20-row window");
}

#[test]
fn g_and_shift_g_jump_to_the_ends() {
    let mut app = conversation(20);
    let bottom = window_rows(&app);
    start_reading(&mut app);

    press(&mut app, KeyCode::Char('g'));
    let top = window_rows(&app);
    press(&mut app, KeyCode::Char('G'));

    assert!(
        top[..3].iter().any(|row| row.contains("question 0")),
        "{top:?}"
    );
    assert_eq!(window_rows(&app), bottom, "back at the end");
}

#[test]
fn reading_pauses_the_follow() {
    let mut app = conversation(20);
    start_reading(&mut app);
    press(&mut app, KeyCode::Char('k'));
    let paused = window_rows(&app);

    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, "brand new"));

    assert_eq!(window_rows(&app), paused);
    assert!(!rows(&app, 80, 24).join("\n").contains("brand new"));
}

#[test]
fn coming_back_to_the_bottom_follows_again() {
    let mut app = conversation(20);
    start_reading(&mut app);
    press(&mut app, KeyCode::Char('k'));

    press(&mut app, KeyCode::Char('G'));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, "brand new"));

    assert!(rows(&app, 80, 24).join("\n").contains("brand new"));
}

#[test]
fn ctrl_d_never_quits() {
    let mut app = conversation(1);

    let event = handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
    );

    assert!(!matches!(event, Some(AppEvent::Quit)), "{event:?}");
}

#[test]
fn the_keys_are_rebindable() {
    let mut app = conversation(20);
    let before = window_rows(&app);
    app.keymap
        .override_scoped(&scoped_keys("navigate", "line_up", "ctrl+y"));
    start_reading(&mut app);

    press_with(&mut app, KeyCode::Char('y'), KeyModifiers::CONTROL);

    assert_eq!(window_rows(&app)[1..20], before[..19]);
}

fn right_column(rows: &[String], width: usize) -> String {
    rows[..20]
        .iter()
        .map(|row| {
            let padded = format!("{row:<width$}");
            padded.chars().nth(width - 1).unwrap_or(' ')
        })
        .collect()
}

#[test]
fn the_scrollbar_shows_while_you_move() {
    let mut app = conversation(20);
    let resting = right_column(&rows(&app, 80, 24), 80);
    start_reading(&mut app);

    press(&mut app, KeyCode::Char('k'));

    let moving = right_column(&rows(&app, 80, 24), 80);
    assert!(resting.trim().is_empty(), "at rest: {resting:?}");
    assert!(!moving.trim().is_empty(), "while moving: {moving:?}");
}

#[test]
fn sending_a_message_snaps_to_the_bottom() {
    let mut app = conversation(20);
    let bottom = window_rows(&app);
    start_reading(&mut app);
    press(&mut app, KeyCode::Char('k'));

    press(&mut app, KeyCode::Esc);
    for c in "hello".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);

    assert_eq!(window_rows(&app), bottom, "back at the newest line");
}

fn highlighted(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut screen = Screen::new();
    terminal
        .draw(|frame| screen.render(app, frame.area(), frame.buffer_mut()))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .filter(|x| {
                    buffer[(*x, y)]
                        .modifier
                        .contains(ratatui::style::Modifier::REVERSED)
                })
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("")
}

#[test]
fn ctrl_a_highlights_the_whole_composer() {
    let mut app = conversation(1);
    app.input.text = "explain the borrow checker".into();
    app.input.cursor = app.input.text.len();

    press_with(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);

    assert_eq!(app.input.text, "explain the borrow checker");
    assert_eq!(highlighted(&app), "explain the borrow checker");
}

#[test]
fn typing_does_nothing_while_the_selection_is_live() {
    let mut app = conversation(1);
    app.input.text = "explain the borrow checker".into();
    app.input.cursor = app.input.text.len();
    press_with(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);

    press(&mut app, KeyCode::Char('x'));

    assert_eq!(app.input.text, "explain the borrow checker");
}

#[test]
fn backspace_empties_the_composer() {
    let mut app = conversation(1);
    app.input.text = "explain the borrow checker".into();
    app.input.cursor = app.input.text.len();
    press_with(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);

    press(&mut app, KeyCode::Backspace);
    for c in "hi".chars() {
        press(&mut app, KeyCode::Char(c));
    }

    assert_eq!(app.input.text, "hi");
}

#[test]
fn c_empties_the_composer() {
    let mut app = conversation(1);
    app.input.text = "explain the borrow checker".into();
    app.input.cursor = app.input.text.len();
    press_with(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);

    press(&mut app, KeyCode::Char('c'));
    for c in "hi".chars() {
        press(&mut app, KeyCode::Char(c));
    }

    assert_eq!(app.input.text, "hi");
}

#[test]
fn esc_drops_the_selection_and_leaves_the_cursor_at_the_end() {
    let mut app = conversation(1);
    app.input.text = "explain the borrow checker".into();
    app.input.cursor = 0;
    press_with(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);

    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('!'));

    assert_eq!(app.input.text, "explain the borrow checker!");
}

#[test]
fn ctrl_c_leaves_what_i_typed_alone() {
    let mut app = conversation(1);
    app.input.text = "explain the borrow checker".into();
    app.input.cursor = app.input.text.len();

    let event = handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    );

    assert!(event.is_none(), "{event:?}");
    assert_eq!(app.input.text, "explain the borrow checker");
    assert_eq!(
        app.status.as_ref().map(|s| s.text.as_str()),
        Some("press ctrl+c again to exit")
    );
}

#[test]
fn ctrl_c_stops_then_arms_then_quits() {
    let mut app = conversation(1);
    app.start_streaming();

    let first = handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    );
    app.finish_streaming(None, None);
    let second = handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    );
    let third = handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    );

    assert!(matches!(first, Some(AppEvent::Abort)), "{first:?}");
    assert!(second.is_none(), "{second:?}");
    assert!(matches!(third, Some(AppEvent::Quit)), "{third:?}");
}

#[test]
fn the_log_holds_still_while_i_read_it() {
    let mut app = conversation(20);
    start_reading(&mut app);
    let reading = window_rows(&app);

    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, "brand new"));

    assert_eq!(window_rows(&app), reading);
}

fn cursor(app: &App) -> (usize, usize) {
    let c = app.viewport.cursor();
    (c.line, c.column)
}

#[test]
fn the_cursor_starts_at_the_bottom_left_of_the_window() {
    let mut app = conversation(20);
    let lines = rows(&app, 80, 24);

    start_reading(&mut app);

    assert_eq!(app.viewport.cursor_row(), 19, "{lines:?}");
    assert_eq!(cursor(&app).1, 0);
}

fn cursor_onto_text(app: &mut App) {
    let window = window_rows(app);
    let Some(row) = window.iter().rposition(|row| row.trim().len() > 4) else {
        panic!("a line with text on it: {window:?}");
    };
    for _ in row..window.len() - 1 {
        press(app, KeyCode::Char('k'));
    }
}

#[test]
fn h_j_k_and_l_move_one_cell() {
    let mut app = conversation(20);
    start_reading(&mut app);
    cursor_onto_text(&mut app);
    let (line, column) = cursor(&app);

    press(&mut app, KeyCode::Char('l'));

    assert_eq!(cursor(&app), (line, column + 1));
}

#[test]
fn the_cursor_stops_at_the_start_of_a_line() {
    let mut app = conversation(20);
    start_reading(&mut app);
    cursor_onto_text(&mut app);

    press(&mut app, KeyCode::Char('h'));

    assert_eq!(cursor(&app).1, 0);
}

#[test]
fn g_takes_the_cursor_to_the_last_line() {
    let mut app = conversation(20);
    start_reading(&mut app);
    press_with(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);

    press(&mut app, KeyCode::Char('G'));
    rows(&app, 80, 24);

    assert_eq!(app.viewport.cursor_row(), 19);
}

#[test]
fn a_row_with_a_stitched_cluster_is_never_trusted_stale() {
    let mut app = conversation(1);
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "gotcha, dropped \u{d26}\u{d4d}\u{d26}(\u{1d54}\u{15dc}\u{1d54})",
    ));

    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    Screen::new().render(&app, area, &mut buffer);

    let row = (0..buffer.area.height)
        .find(|y| (0..buffer.area.width).any(|x| buffer[(x, *y)].symbol().chars().count() > 1))
        .expect("the cluster landed in a cell");
    let trusted: Vec<u16> = (0..buffer.area.width)
        .filter(|x| buffer[(*x, row)].diff_option != CellDiffOption::AlwaysUpdate)
        .collect();

    assert!(
        trusted.is_empty(),
        "row {row} still diffs against a model the terminal disagrees with: {trusted:?}"
    );
}

fn wheel(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
    handle_mouse(
        app,
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    );
}

#[test]
fn one_notch_up_moves_the_log_back_one_line() {
    let mut app = conversation(30);
    let before = window_rows(&app);

    wheel(&mut app, MouseEventKind::ScrollUp, 10, 5);

    let after = window_rows(&app);
    assert_eq!(after[1..], before[..before.len() - 1]);
}

#[test]
fn one_notch_down_moves_the_log_forward_one_line() {
    let mut app = conversation(30);
    window_rows(&app);
    wheel(&mut app, MouseEventKind::ScrollUp, 10, 5);
    let before = window_rows(&app);

    wheel(&mut app, MouseEventKind::ScrollDown, 10, 5);

    let after = window_rows(&app);
    assert_eq!(after[..after.len() - 1], before[1..]);
}

#[test]
fn the_wheel_never_changes_mode() {
    let mut app = conversation(30);
    press(&mut app, KeyCode::Char('h'));
    let before = window_rows(&app);

    for _ in 0..3 {
        wheel(&mut app, MouseEventKind::ScrollUp, 10, 5);
    }
    press(&mut app, KeyCode::Char('i'));

    assert_eq!(window_rows(&app)[3..], before[..before.len() - 3]);
    assert_eq!(app.input.text, "hi");
}

fn with_model_picker_open(exchanges: usize) -> App {
    let mut app = conversation(exchanges);
    app.completion.model_completions = vec![ModelCompletion {
        id: "claude-opus-4-5".into(),
        name: "Claude Opus 4.5".into(),
        provider: "anthropic".into(),
        stale: false,
        description: None,
        speed_tiers: Vec::new(),
        priority: 0,
        visibility: None,
    }];
    app.open_model_picker_overlay();
    app
}

#[test]
fn an_open_picker_swallows_the_wheel() {
    let mut app = with_model_picker_open(30);
    let before = window_rows(&app);

    wheel(&mut app, MouseEventKind::ScrollUp, 10, 5);

    assert_eq!(window_rows(&app), before);
}

#[test]
fn the_wheel_over_the_composer_does_nothing() {
    let mut app = conversation(30);
    let before = window_rows(&app);

    wheel(&mut app, MouseEventKind::ScrollUp, 10, 21);

    assert_eq!(window_rows(&app), before);
}

fn stream(app: &mut App, lines: std::ops::Range<usize>) {
    for i in lines {
        app.push_text_delta(&format!("streamed {i}\n"));
        app.tick_animation();
    }
}

#[test]
fn wheeling_up_during_a_stream_parks_the_log() {
    let mut app = conversation(30);
    app.start_streaming();
    stream(&mut app, 0..40);
    window_rows(&app);

    wheel(&mut app, MouseEventKind::ScrollUp, 10, 5);
    let parked = window_rows(&app);
    stream(&mut app, 40..80);

    assert_eq!(window_rows(&app), parked);
}

#[test]
fn wheeling_back_to_the_bottom_follows_again() {
    let mut app = conversation(30);
    app.start_streaming();
    stream(&mut app, 0..40);
    window_rows(&app);
    for _ in 0..5 {
        wheel(&mut app, MouseEventKind::ScrollUp, 10, 5);
    }

    for _ in 0..5 {
        wheel(&mut app, MouseEventKind::ScrollDown, 10, 5);
    }
    stream(&mut app, 40..80);

    assert!(
        window_rows(&app).contains(&" streamed 79".to_string()),
        "{:?}",
        window_rows(&app)
    );
}

#[test]
fn the_wheel_stops_at_the_top() {
    let mut app = conversation(30);
    window_rows(&app);
    for _ in 0..200 {
        wheel(&mut app, MouseEventKind::ScrollUp, 10, 5);
    }
    let before = window_rows(&app);

    wheel(&mut app, MouseEventKind::ScrollUp, 10, 5);

    assert_eq!(window_rows(&app), before);
    assert!(
        before.iter().any(|row| row.contains("question 0")),
        "{before:?}"
    );
}

#[test]
fn a_transcript_shorter_than_the_log_ignores_the_wheel() {
    let mut app = conversation(1);
    let before = window_rows(&app);

    wheel(&mut app, MouseEventKind::ScrollUp, 10, 5);

    assert_eq!(window_rows(&app), before);
}

#[test]
fn the_notch_size_is_configurable() {
    let mut app = conversation(30);
    app.interaction.mouse_scroll_lines = 5;
    let before = window_rows(&app);

    wheel(&mut app, MouseEventKind::ScrollUp, 10, 5);

    assert_eq!(window_rows(&app)[5..], before[..before.len() - 5]);
}

#[test]
fn sideways_wheel_and_middle_click_are_ignored() {
    let mut app = conversation(30);
    let before = window_rows(&app);

    for kind in [
        MouseEventKind::ScrollLeft,
        MouseEventKind::ScrollRight,
        MouseEventKind::Down(MouseButton::Middle),
        MouseEventKind::Up(MouseButton::Middle),
    ] {
        wheel(&mut app, kind, 10, 5);
    }

    assert_eq!(window_rows(&app), before);
    assert!(app.input.text.is_empty());
}

fn log_with_world() -> App {
    let mut app = conversation(1);
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, "world"));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    rows(&app, 80, 24);
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('j'));
    }
    press(&mut app, KeyCode::Char('l'));
    app
}

fn selected_cells(app: &App) -> Vec<(u16, u16)> {
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    Screen::new().render(app, area, &mut buffer);
    let mut cells = Vec::new();
    for y in 0..area.height {
        for x in 0..area.width {
            if buffer[(x, y)].style().bg == app.theme.selection.bg {
                cells.push((x, y));
            }
        }
    }
    cells
}

#[test]
fn v_starts_a_selection_at_the_cursor() {
    let mut app = log_with_world();

    press(&mut app, KeyCode::Char('v'));

    assert_eq!(selected_cells(&app), vec![(1, 6)]);
}

#[test]
fn a_motion_extends_the_selection_from_the_anchor() {
    let mut app = log_with_world();
    press(&mut app, KeyCode::Char('v'));

    for _ in 0..3 {
        press(&mut app, KeyCode::Char('l'));
    }

    assert_eq!(selected_cells(&app), vec![(1, 6), (2, 6), (3, 6), (4, 6)]);
}

#[test]
fn yank_copies_the_selection_and_leaves_the_cursor_put() {
    let mut app = log_with_world();
    press(&mut app, KeyCode::Char('v'));
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('l'));
    }
    let cursor = app.viewport.cursor();

    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Char('y')));

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "world"),
        "{event:?}"
    );
    assert!(selected_cells(&app).is_empty());
    assert_eq!(app.viewport.cursor(), cursor);
}

fn log_with_indented_hello() -> App {
    let mut app = conversation(1);
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "  hello\n    hello",
    ));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    rows(&app, 80, 24);
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('j'));
    }
    press(&mut app, KeyCode::Char('v'));
    press(&mut app, KeyCode::Char('j'));
    for _ in 0..20 {
        press(&mut app, KeyCode::Char('l'));
    }
    app
}

#[test]
fn a_multi_row_yank_drops_the_common_indent() {
    let mut app = log_with_indented_hello();

    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Char('y')));

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "hello\n  hello"),
        "{event:?}"
    );
}

#[test]
fn a_selection_over_two_rows_highlights_both() {
    let app = log_with_indented_hello();

    let cells = selected_cells(&app);

    assert_eq!(cells.first(), Some(&(0, 6)));
    assert_eq!(cells.last(), Some(&(9, 7)));
    assert_eq!(cells.len(), 8 + 10);
}

#[test]
fn capital_y_copies_the_rows_as_they_are_drawn() {
    let mut app = log_with_indented_hello();

    let event = handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
    );

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "   hello\n     hello"),
        "{event:?}"
    );
}

#[test]
fn capital_y_does_nothing_outside_a_selection() {
    let mut app = log_with_world();

    let event = handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
    );

    assert!(event.is_none(), "{event:?}");
    assert!(selected_cells(&app).is_empty());
}

#[test]
fn esc_drops_the_selection_without_copying() {
    let mut app = log_with_world();
    press(&mut app, KeyCode::Char('v'));
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('l'));
    }

    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Esc));

    assert!(event.is_none(), "{event:?}");
    assert!(selected_cells(&app).is_empty());
}

#[test]
fn a_streaming_reply_leaves_the_selection_where_it_is() {
    let mut app = log_with_world();
    press(&mut app, KeyCode::Char('v'));
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('l'));
    }
    let before = selected_cells(&app);

    app.start_streaming();
    stream(&mut app, 0..40);

    assert_eq!(selected_cells(&app), before);
    assert_eq!(before.len(), 5);
}

#[test]
fn capital_v_highlights_the_whole_row() {
    let mut app = log_with_world();

    press_with(&mut app, KeyCode::Char('V'), KeyModifiers::SHIFT);

    let row: Vec<(u16, u16)> = (0..6).map(|x| (x, 6)).collect();
    assert_eq!(selected_cells(&app), row);
}

#[test]
fn a_motion_in_linewise_mode_adds_whole_rows() {
    let mut app = conversation(1);
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "  hello\n    hello",
    ));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    rows(&app, 80, 24);
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('j'));
    }
    press_with(&mut app, KeyCode::Char('V'), KeyModifiers::SHIFT);

    press(&mut app, KeyCode::Char('j'));

    let expected: Vec<(u16, u16)> = (0..8)
        .map(|x| (x, 6))
        .chain((0..10).map(|x| (x, 7)))
        .collect();
    assert_eq!(selected_cells(&app), expected);
}

fn log_with_alphabet() -> App {
    let mut app = conversation(1);
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "abcdefg\nuvwxyz",
    ));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    rows(&app, 80, 24);
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('j'));
    }
    app
}

#[test]
fn ctrl_v_highlights_a_rectangle() {
    let mut app = log_with_alphabet();
    for _ in 0..3 {
        press(&mut app, KeyCode::Char('l'));
    }

    press_with(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('j'));
    for _ in 0..3 {
        press(&mut app, KeyCode::Char('l'));
    }

    let expected: Vec<(u16, u16)> = (3..7)
        .map(|x| (x, 6))
        .chain((3..7).map(|x| (x, 7)))
        .collect();
    assert_eq!(selected_cells(&app), expected);
}

#[test]
fn the_block_is_the_same_either_way_round() {
    let mut app = log_with_alphabet();
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('l'));
    }

    press_with(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
    for _ in 0..3 {
        press(&mut app, KeyCode::Char('h'));
    }
    press(&mut app, KeyCode::Char('j'));

    let expected: Vec<(u16, u16)> = (3..7)
        .map(|x| (x, 6))
        .chain((3..7).map(|x| (x, 7)))
        .collect();
    assert_eq!(selected_cells(&app), expected);
}

#[test]
fn a_linewise_yank_copies_the_rows_with_the_indent_off() {
    let mut app = conversation(1);
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "  hello\n    hello",
    ));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    rows(&app, 80, 24);
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('j'));
    }
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('l'));
    }
    press_with(&mut app, KeyCode::Char('V'), KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Char('j'));

    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Char('y')));

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "hello\n  hello"),
        "{event:?}"
    );
}

#[test]
fn a_blockwise_yank_joins_the_pieces_with_newlines() {
    let mut app = log_with_alphabet();
    for _ in 0..3 {
        press(&mut app, KeyCode::Char('l'));
    }
    press_with(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('j'));
    for _ in 0..3 {
        press(&mut app, KeyCode::Char('l'));
    }

    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Char('y')));

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "cdef\nwxyz"),
        "{event:?}"
    );
}

#[test]
fn a_row_too_short_for_the_block_keeps_its_line() {
    let mut app = conversation(1);
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "abcdefg\nab\nuvwxyz",
    ));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    rows(&app, 80, 24);
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('j'));
    }
    for _ in 0..3 {
        press(&mut app, KeyCode::Char('l'));
    }
    press_with(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
    for _ in 0..2 {
        press(&mut app, KeyCode::Char('j'));
    }
    for _ in 0..3 {
        press(&mut app, KeyCode::Char('l'));
    }

    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Char('y')));

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "cdef\n\nwxyz"),
        "{event:?}"
    );
}
