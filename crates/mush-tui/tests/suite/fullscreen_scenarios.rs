use chrono::{TimeDelta, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use mush_ai::oauth::usage::{OAuthUsage, UsageWindow};
use mush_ai::types::{TokenCount, ToolCallId};
use mush_tui::AppEvent;
use mush_tui::app::{App, Caret, DisplayMessage, MessageRole};
use mush_tui::input::{handle_key, handle_mouse};
use mush_tui::keybinds::{BindingSource, ScopedKeysConfig};
use mush_tui::screen::Screen;
use mush_tui::slash_menu::ModelCompletion;
use mush_tui::widgets::top_bar::TOP_BAR_HEIGHT as TOP;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;

const SCREEN: u16 = 24 + TOP;

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
    (TOP..buffer.area.height)
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
    rows(app, 80, SCREEN)[..20]
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
    let rows = rows(&conversation(20), 80, SCREEN);

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
    let screen = rows(&conversation(20), 80, SCREEN).join("\n");

    assert!(screen.contains("answer 19"), "{screen}");
    assert!(!screen.contains("question 0"), "{screen}");
}

#[test]
fn the_tail_follows_while_i_type() {
    let mut app = conversation(20);
    let before = rows(&app, 80, SCREEN).join("\n");
    assert!(!before.contains("answer 20"));

    app.push_user_message("question 20");
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, "answer 20"));

    assert!(rows(&app, 80, SCREEN).join("\n").contains("answer 20"));
}

#[test]
fn a_short_conversation_sits_at_the_top() {
    let rows = rows(&conversation(1), 80, SCREEN);

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
    rows(app, 80, SCREEN);
    press(app, KeyCode::Esc);
    press(app, KeyCode::Esc);
}

#[test]
fn esc_leaves_insert_without_interrupting_work() {
    let mut app = conversation(20);
    app.start_streaming();
    rows(&app, 80, SCREEN);

    press(&mut app, KeyCode::Esc);
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "cargo build line",
    ));

    assert_eq!(
        app.interaction.mode.caret(app.interaction.focus),
        Caret::ComposerBlock
    );
    assert!(
        rows(&app, 80, SCREEN)
            .join("\n")
            .contains("cargo build line")
    );
}

#[test]
fn a_second_esc_moves_me_to_the_log() {
    let mut app = conversation(20);
    press(&mut app, KeyCode::Esc);

    press(&mut app, KeyCode::Esc);

    assert_eq!(
        app.interaction.mode.caret(app.interaction.focus),
        Caret::Log
    );
}

#[test]
fn coming_back_to_the_composer_leaves_the_view_where_i_left_it() {
    let mut app = conversation(20);
    let bottom = window_rows(&app);
    start_reading(&mut app);
    press_with(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
    let scrolled = window_rows(&app);
    assert_ne!(scrolled, bottom, "the view moved off the tail");

    press(&mut app, KeyCode::Char('i'));
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

    assert_eq!(
        app.interaction.mode.caret(app.interaction.focus),
        Caret::Log
    );
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
fn i_while_reading_goes_back_to_typing() {
    let mut app = conversation(20);
    start_reading(&mut app);

    press(&mut app, KeyCode::Char('i'));
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
fn gg_and_shift_g_jump_to_the_ends() {
    let mut app = conversation(20);
    let bottom = window_rows(&app);
    start_reading(&mut app);

    press(&mut app, KeyCode::Char('g'));
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
fn a_count_before_shift_g_goes_to_that_row() {
    let mut app = conversation(20);
    start_reading(&mut app);

    press(&mut app, KeyCode::Char('3'));
    press(&mut app, KeyCode::Char('G'));

    assert_eq!(app.viewport.cursor().line, 2);
}

#[test]
fn esc_drops_a_half_typed_g_in_the_log() {
    let mut app = conversation(20);
    start_reading(&mut app);
    let start = app.viewport.cursor().line;

    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('k'));

    assert_eq!(app.viewport.cursor().line, start - 1);
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
    assert!(!rows(&app, 80, SCREEN).join("\n").contains("brand new"));
}

#[test]
fn coming_back_to_the_bottom_follows_again() {
    let mut app = conversation(20);
    start_reading(&mut app);
    press(&mut app, KeyCode::Char('k'));

    press(&mut app, KeyCode::Char('G'));
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, "brand new"));

    assert!(rows(&app, 80, SCREEN).join("\n").contains("brand new"));
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
        .override_scoped(&scoped_keys("normal", "line_up", "ctrl+y"));
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
    let resting = right_column(&rows(&app, 80, SCREEN), 80);
    start_reading(&mut app);

    press(&mut app, KeyCode::Char('k'));

    let moving = right_column(&rows(&app, 80, SCREEN), 80);
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

#[test]
fn typing_does_nothing_while_the_selection_is_live() {
    let mut app = conversation(1);
    app.input.text = "explain the borrow checker".into();
    app.input.cursor = app.input.text.len();
    press_with(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);

    press(&mut app, KeyCode::Char('z'));

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
fn esc_after_select_all_leaves_the_text_alone() {
    let mut app = conversation(1);
    app.input.text = "explain the borrow checker".into();
    app.input.cursor = 0;
    press_with(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);

    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('!'));

    assert_eq!(app.input.text, "explain the borrow checker");
    assert_eq!(tinted_text(&app), "");
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
fn ctrl_c_stops_and_arms_then_quits() {
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

    assert!(matches!(first, Some(AppEvent::Abort)), "{first:?}");
    assert!(matches!(second, Some(AppEvent::Quit)), "{second:?}");
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
    let lines = rows(&app, 80, SCREEN);

    start_reading(&mut app);

    assert_eq!(app.viewport.cursor_row(), 19, "{lines:?}");
    assert_eq!(cursor(&app).1, 0);
}

#[expect(clippy::panic, reason = "the fixture needs a line with text")]
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
    rows(&app, 80, SCREEN);

    assert_eq!(app.viewport.cursor_row(), 19);
}

#[test]
fn a_row_with_a_stitched_cluster_is_never_trusted_stale() {
    let mut app = conversation(1);
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "gotcha, dropped \u{d26}\u{d4d}\u{d26}(\u{1d54}\u{15dc}\u{1d54})",
    ));

    let area = Rect::new(0, 0, 80, SCREEN);
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
    rows(&app, 80, SCREEN);
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('j'));
    }
    press(&mut app, KeyCode::Char('l'));
    app
}

fn selected_cells(app: &App) -> Vec<(u16, u16)> {
    let area = Rect::new(0, 0, 80, SCREEN);
    let mut buffer = Buffer::empty(area);
    Screen::new().render(app, area, &mut buffer);
    let mut cells = Vec::new();
    for y in TOP..area.height {
        for x in 0..area.width {
            if buffer[(x, y)].style().bg == app.theme.selection.bg {
                cells.push((x, y - TOP));
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
    rows(&app, 80, SCREEN);
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
    rows(&app, 80, SCREEN);
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
    rows(&app, 80, SCREEN);
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
    rows(&app, 80, SCREEN);
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
    rows(&app, 80, SCREEN);
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

fn linewise_over_indented_hello() -> App {
    let mut app = conversation(1);
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "  hello\n    hello",
    ));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    rows(&app, 80, SCREEN);
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('j'));
    }
    press_with(&mut app, KeyCode::Char('V'), KeyModifiers::SHIFT);
    app
}

#[test]
fn capital_y_after_a_linewise_selection_copies_the_rows_as_drawn() {
    let mut app = linewise_over_indented_hello();
    press(&mut app, KeyCode::Char('j'));

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
fn esc_drops_a_linewise_selection() {
    let mut app = linewise_over_indented_hello();

    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Esc));

    assert!(event.is_none(), "{event:?}");
    assert!(selected_cells(&app).is_empty());
}

fn caret(app: &App) -> Option<(u16, u16)> {
    rows(app, 80, SCREEN);
    Screen::new()
        .cursor_cell(app, Rect::new(0, 0, 80, SCREEN))
        .map(|(x, y)| (x, y - TOP))
}

fn log_on_indented_hello() -> App {
    let mut app = conversation(1);
    app.messages.push(DisplayMessage::new(
        MessageRole::Assistant,
        "  hello\n    hello",
    ));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    rows(&app, 80, SCREEN);
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('j'));
    }
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('l'));
    }
    app
}

#[test]
fn zero_goes_to_the_first_cell_of_the_row() {
    let mut app = log_on_indented_hello();
    assert_eq!(caret(&app), Some((6, 6)));

    press(&mut app, KeyCode::Char('0'));

    assert_eq!(caret(&app), Some((0, 6)));
}

#[test]
fn caret_goes_to_the_first_non_blank() {
    let mut app = log_on_indented_hello();
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(caret(&app), Some((7, 6)));

    press(&mut app, KeyCode::Char('^'));

    assert_eq!(caret(&app), Some((3, 6)));
}

#[test]
fn dollar_goes_to_the_last_cell() {
    let mut app = log_on_indented_hello();
    press(&mut app, KeyCode::Char('0'));

    press(&mut app, KeyCode::Char('$'));

    assert_eq!(caret(&app), Some((7, 6)));
}

fn log_on(text: &str) -> App {
    let mut app = conversation(1);
    app.messages
        .push(DisplayMessage::new(MessageRole::Assistant, text));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    rows(&app, 80, SCREEN);
    for _ in 0..6 {
        press(&mut app, KeyCode::Char('j'));
    }
    press(&mut app, KeyCode::Char('^'));
    app
}

#[test]
fn w_goes_to_the_start_of_the_next_word() {
    let mut app = log_on("hello world");
    assert_eq!(caret(&app), Some((1, 6)));

    press(&mut app, KeyCode::Char('w'));

    assert_eq!(caret(&app), Some((7, 6)));
}

#[test]
fn punctuation_is_a_word_of_its_own() {
    let mut app = log_on("foo.bar baz");

    press(&mut app, KeyCode::Char('w'));

    assert_eq!(caret(&app), Some((4, 6)));
}

#[test]
fn capital_w_skips_to_the_next_chunk() {
    let mut app = log_on("foo.bar baz");

    press_with(&mut app, KeyCode::Char('W'), KeyModifiers::SHIFT);

    assert_eq!(caret(&app), Some((9, 6)));
}

#[test]
fn b_goes_back_to_the_start_of_the_previous_word() {
    let mut app = log_on("hello world");
    press(&mut app, KeyCode::Char('w'));

    press(&mut app, KeyCode::Char('b'));

    assert_eq!(caret(&app), Some((1, 6)));
}

#[test]
fn e_goes_to_the_end_of_the_word() {
    let mut app = log_on("hello world");

    press(&mut app, KeyCode::Char('e'));

    assert_eq!(caret(&app), Some((5, 6)));
}

#[test]
fn capital_b_goes_back_a_whole_chunk() {
    let mut app = log_on("foo.bar baz");
    press_with(&mut app, KeyCode::Char('W'), KeyModifiers::SHIFT);

    press_with(&mut app, KeyCode::Char('B'), KeyModifiers::SHIFT);

    assert_eq!(caret(&app), Some((1, 6)));
}

#[test]
fn a_word_motion_crosses_into_the_next_row() {
    let mut app = log_on("hello\nworld");
    press(&mut app, KeyCode::Char('e'));
    assert_eq!(caret(&app), Some((5, 6)));

    press(&mut app, KeyCode::Char('w'));

    assert_eq!(caret(&app), Some((1, 7)));
}

#[test]
fn a_count_repeats_a_motion() {
    let mut app = log_on("hello world");
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(caret(&app), Some((1, 4)));

    press(&mut app, KeyCode::Char('3'));
    press(&mut app, KeyCode::Char('k'));

    assert_eq!(caret(&app), Some((1, 1)));
}

#[test]
fn a_count_repeats_a_word_motion() {
    let mut app = log_on("hello brave new world");

    press(&mut app, KeyCode::Char('3'));
    press(&mut app, KeyCode::Char('w'));

    assert_eq!(caret(&app), Some((17, 6)));
}

#[test]
fn a_count_with_no_motion_evaporates() {
    let mut app = log_on("hello brave new world");

    press(&mut app, KeyCode::Char('4'));
    press(&mut app, KeyCode::Char('z'));
    press(&mut app, KeyCode::Char('w'));

    assert_eq!(caret(&app), Some((7, 6)));
}

#[test]
fn f_finds_a_character_in_the_log_row() {
    let mut app = log_on("hello brave new world");

    press(&mut app, KeyCode::Char('f'));
    press(&mut app, KeyCode::Char('n'));

    assert_eq!(caret(&app), Some((13, 6)));
}

#[test]
fn viw_in_the_log_selects_a_word_to_copy() {
    let mut app = log_on("hello brave new world");
    press(&mut app, KeyCode::Char('w'));

    press(&mut app, KeyCode::Char('v'));
    press(&mut app, KeyCode::Char('i'));
    press(&mut app, KeyCode::Char('w'));
    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Char('y')));

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "brave"),
        "{event:?}"
    );
}

#[test]
fn o_swaps_the_ends_of_a_log_selection() {
    let mut app = log_on("hello brave new world");
    press(&mut app, KeyCode::Char('w'));
    press(&mut app, KeyCode::Char('v'));
    press(&mut app, KeyCode::Char('e'));

    press(&mut app, KeyCode::Char('o'));
    press(&mut app, KeyCode::Char('b'));
    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Char('y')));

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "hello brave"),
        "{event:?}"
    );
}

#[test]
fn braces_hop_between_paragraphs_in_the_log() {
    let mut app = log_on("first para\n\nsecond para");
    assert_eq!(caret(&app), Some((1, 6)));

    press(&mut app, KeyCode::Char('}'));
    let between = caret(&app);
    press(&mut app, KeyCode::Char('}'));
    press(&mut app, KeyCode::Char('{'));

    assert_eq!(between, Some((0, 7)));
    assert_eq!(caret(&app), Some((0, 7)));
}

#[test]
fn a_count_works_in_visual_mode() {
    let mut app = log_on("hello brave new world");
    press(&mut app, KeyCode::Char('v'));

    press(&mut app, KeyCode::Char('2'));
    press(&mut app, KeyCode::Char('e'));

    let expected: Vec<(u16, u16)> = (1..12).map(|x| (x, 6)).collect();
    assert_eq!(selected_cells(&app), expected);
}

fn with_running_bash() -> App {
    let mut app = conversation(1);
    app.start_streaming();
    app.start_tool(&ToolCallId::from("tc_1"), "bash", "ls -la");
    app
}

#[test]
fn a_running_tool_draws_in_the_transcript() {
    let app = with_running_bash();

    let transcript = window_rows(&app);

    assert!(
        transcript.iter().any(|row| row.contains("bash · running")),
        "{transcript:#?}"
    );
}

fn transcript_height(app: &App) -> usize {
    rows(app, 80, SCREEN);
    app.viewport.page()
}

#[test]
fn nothing_is_reserved_above_the_composer() {
    let idle = conversation(1);
    let running = with_running_bash();

    assert_eq!(transcript_height(&running), transcript_height(&idle));
}

#[test]
fn no_tool_rendering_draws_a_border() {
    let mut app = with_running_bash();
    app.end_tool(
        &ToolCallId::from("tc_1"),
        "bash",
        mush_ai::types::ToolOutcome::Success,
        Some("file1.txt"),
        None,
    );
    app.start_tool(&ToolCallId::from("tc_2"), "read", "src/main.rs");

    let transcript = window_rows(&app).join("");

    for border in ['┌', '─', '│', '└'] {
        assert!(!transcript.contains(border), "{border} in {transcript:?}");
    }
}

#[test]
fn two_running_tools_stack_in_call_order() {
    let mut app = with_running_bash();
    app.start_tool(&ToolCallId::from("tc_2"), "read", "src/main.rs");

    let transcript = window_rows(&app);
    let bash = transcript.iter().position(|r| r.contains("bash"));
    let read = transcript.iter().position(|r| r.contains("read"));

    assert!(bash < read, "{transcript:#?}");
}

fn bash_with_output(lines: usize) -> App {
    let mut app = conversation(1);
    app.start_streaming();
    app.start_tool(&ToolCallId::from("tc_1"), "bash", "seq");
    let output: Vec<String> = (1..=lines).map(|i| format!("line {i}")).collect();
    app.push_tool_output_by_exec_id("tc_1", &output.join("\n"));
    app
}

#[test]
fn a_running_tool_shows_the_last_five_lines() {
    let app = bash_with_output(8);

    let transcript = window_rows(&app);

    assert!(
        transcript.iter().any(|r| r.contains("… 3 earlier lines")),
        "{transcript:#?}"
    );
    assert!(transcript.iter().any(|r| r.contains("line 8")));
    assert!(!transcript.iter().any(|r| r.contains("line 3")));
}

#[test]
fn short_output_gets_no_note() {
    let app = bash_with_output(3);

    let transcript = window_rows(&app);

    assert!(transcript.iter().any(|r| r.contains("line 1")));
    assert!(!transcript.iter().any(|r| r.contains("earlier lines")));
}

#[test]
fn a_finished_tool_keeps_the_tail_too() {
    let mut app = bash_with_output(45);
    let output: Vec<String> = (1..=45).map(|i| format!("line {i}")).collect();
    app.end_tool(
        &ToolCallId::from("tc_1"),
        "bash",
        mush_ai::types::ToolOutcome::Success,
        Some(&output.join("\n")),
        None,
    );

    let transcript = window_rows(&app);

    assert!(
        transcript.iter().any(|r| r.contains("… 40 earlier lines")),
        "{transcript:#?}"
    );
    assert!(transcript.iter().any(|r| r.contains("line 45")));
    assert!(!transcript.iter().any(|r| r.contains("line 40")));
}

#[test]
fn all_output_shows_every_line_of_a_finished_tool() {
    let mut app = bash_with_output(8);
    let output: Vec<String> = (1..=8).map(|i| format!("line {i}")).collect();
    app.end_tool(
        &ToolCallId::from("tc_1"),
        "bash",
        mush_ai::types::ToolOutcome::Success,
        Some(&output.join("\n")),
        None,
    );

    app.detail = mush_tui::app::ChatDetail::All;
    let transcript = window_rows(&app);

    assert!(
        transcript.iter().any(|r| r.contains("line 1")),
        "{transcript:#?}"
    );
    assert!(transcript.iter().any(|r| r.contains("line 8")));
    assert!(!transcript.iter().any(|r| r.contains("earlier lines")));
}

#[test]
fn details_reveals_collapsed_thinking() {
    let mut app = conversation(1);
    let mut reply = DisplayMessage::new(MessageRole::Assistant, "done");
    reply.thinking = Some("first weigh the options\nthen pick one".into());
    app.messages.push(reply);
    app.detail = mush_tui::app::ChatDetail::Overview;
    let before = window_rows(&app);
    assert!(
        !before.iter().any(|r| r.contains("then pick one")),
        "{before:#?}"
    );

    app.detail = mush_tui::app::ChatDetail::Details;
    let after = window_rows(&app);

    assert!(
        after.iter().any(|r| r.contains("then pick one")),
        "{after:#?}"
    );
}

#[test]
fn the_detail_key_cycles_from_the_composer_and_the_log() {
    let mut app = conversation(1);

    press_with(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(app.detail, mush_tui::app::ChatDetail::All);

    start_reading(&mut app);
    press_with(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);

    assert_eq!(app.detail, mush_tui::app::ChatDetail::Overview);
}

#[test]
fn a_tool_settles_in_place_when_it_finishes() {
    let mut app = bash_with_output(8);
    let output: Vec<String> = (1..=8).map(|i| format!("line {i}")).collect();
    let running = window_rows(&app);

    app.end_tool(
        &ToolCallId::from("tc_1"),
        "bash",
        mush_ai::types::ToolOutcome::Success,
        Some(&output.join("\n")),
        None,
    );

    let settled = window_rows(&app);
    assert_eq!(
        settled
            .iter()
            .map(|r| r.replace("bash · done", "bash · running"))
            .collect::<Vec<_>>(),
        running
    );
}

fn panel_row_indices(app: &App) -> Vec<u16> {
    let area = Rect::new(0, 0, 80, SCREEN);
    let mut buffer = Buffer::empty(area);
    Screen::new().render(app, area, &mut buffer);
    (TOP..20 + TOP)
        .filter(|y| (0..80).all(|x| buffer[(x, *y)].style().bg == app.theme.tool_panel_bg.bg))
        .map(|y| y - TOP)
        .collect()
}

#[test]
fn the_panel_keeps_its_background_and_padding() {
    let app = bash_with_output(3);

    let filled = panel_row_indices(&app);
    let transcript = window_rows(&app);

    assert!(!filled.is_empty(), "{transcript:#?}");
    for y in filled {
        let row = &transcript[y as usize];
        assert!(row.starts_with("  "), "{row:?}");
    }
}

fn selected_panel_row(app: &mut App, y: u16) {
    press(&mut *app, KeyCode::Esc);
    press(&mut *app, KeyCode::Esc);
    rows(app, 80, SCREEN);
    press(&mut *app, KeyCode::Char('g'));
    press(&mut *app, KeyCode::Char('g'));
    for _ in 0..y {
        press(&mut *app, KeyCode::Char('j'));
    }
    press_with(&mut *app, KeyCode::Char('V'), KeyModifiers::SHIFT);
}

#[test]
fn y_trims_the_padding_off_the_copy() {
    let mut app = bash_with_output(3);
    let row = panel_row_indices(&app)[1];
    selected_panel_row(&mut app, row);

    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Char('y')));

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "seq"),
        "{event:?}"
    );
}

#[test]
fn capital_y_keeps_the_row_as_drawn() {
    let mut app = bash_with_output(3);
    let row = panel_row_indices(&app)[1];
    selected_panel_row(&mut app, row);

    let event = handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
    );

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text.len() == 80 && text.trim() == "seq"),
        "{event:?}"
    );
}

#[test]
fn the_caret_stays_put_when_a_selection_starts() {
    let mut app = log_with_world();
    let before = caret(&app);

    press(&mut app, KeyCode::Char('v'));

    assert_eq!(caret(&app), before);
}

fn composer_in_normal(text: &str) -> App {
    let mut app = conversation(1);
    app.input.text = text.into();
    app.input.cursor = 0;
    press(&mut app, KeyCode::Esc);
    app
}

#[test]
fn w_moves_the_composer_caret_to_the_next_word() {
    let mut app = composer_in_normal("hello world");

    press(&mut app, KeyCode::Char('w'));

    assert_eq!(app.input.cursor, 6);
}

#[test]
fn a_count_repeats_a_composer_motion() {
    let mut app = composer_in_normal("hello world");

    press(&mut app, KeyCode::Char('3'));
    press(&mut app, KeyCode::Char('l'));

    assert_eq!(app.input.cursor, 3);
}

#[test]
fn dollar_goes_to_the_end_of_the_composer_line() {
    let mut app = composer_in_normal("hello world");

    press(&mut app, KeyCode::Char('$'));

    assert_eq!(app.input.cursor, 10);
}

#[test]
fn j_moves_down_a_logical_line_not_a_wrapped_one() {
    let mut app = composer_in_normal(&format!("{}\nsecond", "x".repeat(200)));

    press(&mut app, KeyCode::Char('j'));

    assert_eq!(app.input.cursor, 201);
}

#[test]
fn gg_and_shift_g_reach_the_first_and_last_composer_lines() {
    let mut app = composer_in_normal("one\ntwo\nthree");

    press(&mut app, KeyCode::Char('G'));
    assert_eq!(app.input.cursor, 8);

    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('g'));
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn a_count_before_g_names_the_composer_line() {
    let mut app = composer_in_normal("one\ntwo\nthree");

    press(&mut app, KeyCode::Char('2'));
    press(&mut app, KeyCode::Char('G'));
    assert_eq!(app.input.cursor, 4);

    press(&mut app, KeyCode::Char('3'));
    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('g'));
    assert_eq!(app.input.cursor, 8);
}

#[test]
fn ge_goes_back_to_the_end_of_the_previous_composer_word() {
    let mut app = composer_in_normal("hello brave world");
    press(&mut app, KeyCode::Char('$'));

    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('e'));

    assert_eq!(app.input.cursor, 10);
}

#[test]
fn w_stops_on_an_empty_composer_line() {
    let mut app = composer_in_normal("one\n\ntwo");

    press(&mut app, KeyCode::Char('w'));

    assert_eq!(app.input.cursor, 4);
}

#[test]
fn esc_drops_a_half_typed_command_and_keeps_the_composer() {
    let mut app = composer_in_normal("one\ntwo");
    press(&mut app, KeyCode::Char('j'));

    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('k'));

    assert_eq!(app.interaction.mode, mush_tui::app::AppMode::Normal);
    assert!(!app.interaction.focus.is_log());
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn f_and_t_find_a_character_on_the_composer_line() {
    let mut app = composer_in_normal("one, two, three");

    press(&mut app, KeyCode::Char('f'));
    press(&mut app, KeyCode::Char(','));
    assert_eq!(app.input.cursor, 3);

    press(&mut app, KeyCode::Char('t'));
    press(&mut app, KeyCode::Char(','));
    assert_eq!(app.input.cursor, 7);

    press_with(&mut app, KeyCode::Char('F'), KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.input.cursor, 1);

    press(&mut app, KeyCode::Char('$'));
    press_with(&mut app, KeyCode::Char('T'), KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Char('t'));
    assert_eq!(app.input.cursor, 11);
}

#[test]
fn semicolon_and_comma_repeat_the_find_both_ways() {
    let mut app = composer_in_normal("a-b-c-d");
    press(&mut app, KeyCode::Char('f'));
    press(&mut app, KeyCode::Char('-'));

    press(&mut app, KeyCode::Char(';'));
    assert_eq!(app.input.cursor, 3);
    press(&mut app, KeyCode::Char('2'));
    press(&mut app, KeyCode::Char(';'));
    assert_eq!(app.input.cursor, 3, "no third dash after this one");

    press(&mut app, KeyCode::Char(','));
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn a_counted_f_skips_matches() {
    let mut app = composer_in_normal("a-b-c-d");

    press(&mut app, KeyCode::Char('3'));
    press(&mut app, KeyCode::Char('f'));
    press(&mut app, KeyCode::Char('-'));

    assert_eq!(app.input.cursor, 5);
}

#[test]
fn a_find_that_misses_leaves_the_caret() {
    let mut app = composer_in_normal("hello");

    press(&mut app, KeyCode::Char('f'));
    press(&mut app, KeyCode::Char('z'));

    assert_eq!(app.input.cursor, 0);
    assert_eq!(app.input.text, "hello");
}

#[test]
fn percent_jumps_to_the_matching_bracket_in_the_composer() {
    let mut app = composer_in_normal("fn go() {\n    call(1, [2]);\n}");
    press(&mut app, KeyCode::Char('$'));

    press(&mut app, KeyCode::Char('%'));
    assert_eq!(app.input.cursor, 28);

    press(&mut app, KeyCode::Char('%'));
    assert_eq!(app.input.cursor, 8);
}

#[test]
fn a_count_before_percent_goes_that_far_down() {
    let mut app = composer_in_normal("one\ntwo\nthree\nfour");

    press(&mut app, KeyCode::Char('5'));
    press(&mut app, KeyCode::Char('0'));
    press(&mut app, KeyCode::Char('%'));

    assert_eq!(app.input.cursor, 4);
}

#[test]
fn l_stops_on_the_last_composer_character() {
    let mut app = composer_in_normal("hello");

    press(&mut app, KeyCode::Char('9'));
    press(&mut app, KeyCode::Char('l'));

    assert_eq!(app.input.cursor, 4);
}

#[test]
fn j_and_k_keep_the_column_across_a_short_line() {
    let mut app = composer_in_normal("hello\nhi\nworld");
    press(&mut app, KeyCode::Char('$'));

    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.input.cursor, 7, "rests on the last character of hi");

    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.input.cursor, 13);

    press(&mut app, KeyCode::Char('2'));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.input.cursor, 4);
}

#[test]
fn esc_at_the_start_of_a_line_keeps_the_caret_on_that_line() {
    let mut app = conversation(1);
    app.input.text = "ab\n".into();
    app.input.cursor = 3;

    press(&mut app, KeyCode::Esc);

    assert_eq!(app.input.cursor, 3);
}

fn type_keys(app: &mut App, keys: &str) {
    for c in keys.chars() {
        if c.is_ascii_uppercase() {
            press_with(app, KeyCode::Char(c), KeyModifiers::SHIFT);
        } else {
            press(app, KeyCode::Char(c));
        }
    }
}

fn composer_mode(app: &App) -> mush_tui::app::AppMode {
    app.interaction.mode.clone()
}

#[test]
fn dw_deletes_a_word_and_its_space() {
    let mut app = composer_in_normal("one two three");

    type_keys(&mut app, "dw");

    assert_eq!(app.input.text, "two three");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn dw_on_the_last_word_of_a_line_keeps_the_break() {
    let mut app = composer_in_normal("one two\nthree");

    type_keys(&mut app, "wdw");

    assert_eq!(app.input.text, "one \nthree");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn a_count_on_either_side_of_d_multiplies() {
    let mut app = composer_in_normal("a b c d e f g h");

    type_keys(&mut app, "2d3w");

    assert_eq!(app.input.text, "g h");
}

#[test]
fn cw_changes_to_the_end_of_the_word_and_enters_insert() {
    let mut app = composer_in_normal("one two");

    type_keys(&mut app, "cw1");

    assert_eq!(app.input.text, "1 two");
    assert_eq!(composer_mode(&app), mush_tui::app::AppMode::Insert);
}

#[test]
fn dd_takes_the_line_and_p_puts_it_below() {
    let mut app = composer_in_normal("a\nb\nc");

    type_keys(&mut app, "dd");
    assert_eq!(app.input.text, "b\nc");
    assert_eq!(app.input.cursor, 0);

    type_keys(&mut app, "p");
    assert_eq!(app.input.text, "b\na\nc");
    assert_eq!(app.input.cursor, 2);
}

#[test]
fn a_count_before_dd_takes_more_lines() {
    let mut app = composer_in_normal("a\nb\nc");

    type_keys(&mut app, "2dd");

    assert_eq!(app.input.text, "c");
}

#[test]
fn dd_on_the_last_line_takes_the_break_before_it() {
    let mut app = composer_in_normal("a\n  b");

    type_keys(&mut app, "jdd");

    assert_eq!(app.input.text, "a");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn dd_lands_on_the_first_non_blank_of_the_next_line() {
    let mut app = composer_in_normal("a\n  b");

    type_keys(&mut app, "dd");

    assert_eq!(app.input.text, "  b");
    assert_eq!(app.input.cursor, 2);
}

#[test]
fn cc_empties_the_line_but_keeps_its_indent() {
    let mut app = composer_in_normal("  foo\nbar");

    type_keys(&mut app, "ccx");

    assert_eq!(app.input.text, "  x\nbar");
    assert_eq!(composer_mode(&app), mush_tui::app::AppMode::Insert);
}

#[test]
fn yy_copies_the_line_and_p_puts_a_copy_below() {
    let mut app = composer_in_normal("a\nb");

    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Char('y')));
    assert!(event.is_none());
    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Char('y')));
    type_keys(&mut app, "p");

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "a"),
        "{event:?}"
    );
    assert_eq!(app.input.text, "a\na\nb");
}

#[test]
fn yw_then_capital_p_puts_the_word_before_the_caret() {
    let mut app = composer_in_normal("one two");

    type_keys(&mut app, "ywP");

    assert_eq!(app.input.text, "one one two");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn a_count_before_p_puts_that_many_copies() {
    let mut app = composer_in_normal("ab");

    type_keys(&mut app, "x3p");

    assert_eq!(app.input.text, "baaa");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn p_with_nothing_taken_does_nothing() {
    let mut app = composer_in_normal("ab");

    type_keys(&mut app, "pP");

    assert_eq!(app.input.text, "ab");
}

#[test]
fn x_takes_the_character_and_p_puts_it_back_after() {
    let mut app = composer_in_normal("ab");

    type_keys(&mut app, "xp");

    assert_eq!(app.input.text, "ba");
}

#[test]
fn x_on_the_last_character_steps_the_caret_back() {
    let mut app = composer_in_normal("ab");

    type_keys(&mut app, "$x");

    assert_eq!(app.input.text, "a");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn a_counted_x_stays_on_its_line() {
    let mut app = composer_in_normal("ab\ncd");

    type_keys(&mut app, "5x");

    assert_eq!(app.input.text, "\ncd");
}

#[test]
fn capital_x_deletes_before_the_caret() {
    let mut app = composer_in_normal("abc");

    type_keys(&mut app, "$X");

    assert_eq!(app.input.text, "ac");
    assert_eq!(app.input.cursor, 1);
}

#[test]
fn capital_d_deletes_to_the_end_of_the_line() {
    let mut app = composer_in_normal("one two\nthree");

    type_keys(&mut app, "wD");

    assert_eq!(app.input.text, "one \nthree");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn capital_c_changes_to_the_end_of_the_line() {
    let mut app = composer_in_normal("one two\nthree");

    type_keys(&mut app, "wCx");

    assert_eq!(app.input.text, "one x\nthree");
}

#[test]
fn s_and_capital_s_substitute() {
    let mut app = composer_in_normal("abc\ndef");
    type_keys(&mut app, "sx");
    assert_eq!(app.input.text, "xbc\ndef");

    press(&mut app, KeyCode::Esc);
    type_keys(&mut app, "jSy");
    assert_eq!(app.input.text, "xbc\ny");
}

#[test]
fn linewise_motions_delete_whole_lines() {
    let mut app = composer_in_normal("a\nb\nc\nd");
    type_keys(&mut app, "jdj");
    assert_eq!(app.input.text, "a\nd");

    let mut app = composer_in_normal("a\nb\nc\nd");
    type_keys(&mut app, "jdG");
    assert_eq!(app.input.text, "a");

    let mut app = composer_in_normal("a\nb\nc\nd");
    type_keys(&mut app, "jjdgg");
    assert_eq!(app.input.text, "d");
}

#[test]
fn d_takes_a_find_and_a_bracket_match() {
    let mut app = composer_in_normal("foo(bar) baz");
    type_keys(&mut app, "dt(");
    assert_eq!(app.input.text, "(bar) baz");

    type_keys(&mut app, "d%");
    assert_eq!(app.input.text, " baz");

    let mut app = composer_in_normal("a-b-c");
    type_keys(&mut app, "d2f-");
    assert_eq!(app.input.text, "c");
}

#[test]
fn esc_drops_a_pending_operator() {
    let mut app = composer_in_normal("one two");

    type_keys(&mut app, "d");
    press(&mut app, KeyCode::Esc);
    type_keys(&mut app, "w");

    assert_eq!(app.input.text, "one two");
    assert_eq!(app.input.cursor, 4);
    assert!(!app.interaction.focus.is_log());
}

#[test]
fn dd_on_the_only_line_empties_the_composer() {
    let mut app = composer_in_normal("hello");

    type_keys(&mut app, "dd");

    assert_eq!(app.input.text, "");
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn u_undoes_a_change_and_ctrl_r_redoes_it() {
    let mut app = composer_in_normal("one two three");
    type_keys(&mut app, "wdw");
    assert_eq!(app.input.text, "one three");

    type_keys(&mut app, "u");
    assert_eq!(app.input.text, "one two three");
    assert_eq!(app.input.cursor, 4);

    press_with(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(app.input.text, "one three");
}

#[test]
fn u_undoes_everything_typed_in_one_insert() {
    let mut app = conversation(1);
    type_keys(&mut app, "hello world");
    press(&mut app, KeyCode::Esc);

    type_keys(&mut app, "u");

    assert_eq!(app.input.text, "");
}

#[test]
fn a_change_and_what_it_typed_undo_together() {
    let mut app = composer_in_normal("one two");
    type_keys(&mut app, "cwfirst");
    press(&mut app, KeyCode::Esc);

    type_keys(&mut app, "u");

    assert_eq!(app.input.text, "one two");
}

#[test]
fn a_count_before_u_undoes_that_many_changes() {
    let mut app = composer_in_normal("abcd");
    type_keys(&mut app, "xxx");

    type_keys(&mut app, "2u");
    assert_eq!(app.input.text, "bcd");

    press(&mut app, KeyCode::Char('2'));
    press_with(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(app.input.text, "d");
}

#[test]
fn u_with_nothing_left_says_so() {
    let mut app = conversation(1);
    type_keys(&mut app, "hello");
    press(&mut app, KeyCode::Esc);

    type_keys(&mut app, "uu");

    assert_eq!(app.input.text, "");
    assert_eq!(
        app.status.as_ref().map(|status| status.text.as_str()),
        Some("already at the oldest change")
    );
}

#[test]
fn a_sent_prompt_leaves_nothing_to_undo() {
    let mut app = conversation(1);
    type_keys(&mut app, "first");
    press(&mut app, KeyCode::Enter);
    type_keys(&mut app, "second");
    press(&mut app, KeyCode::Esc);

    type_keys(&mut app, "uu");

    assert_eq!(app.input.text, "");
}

#[test]
fn a_appends_after_the_caret() {
    let mut app = composer_in_normal("ab");

    type_keys(&mut app, "ax");

    assert_eq!(app.input.text, "axb");
}

#[test]
fn a_on_an_empty_composer_just_inserts() {
    let mut app = composer_in_normal("");

    type_keys(&mut app, "ax");

    assert_eq!(app.input.text, "x");
}

#[test]
fn capital_a_appends_at_the_end_of_the_line() {
    let mut app = composer_in_normal("one\ntwo");

    type_keys(&mut app, "A!");

    assert_eq!(app.input.text, "one!\ntwo");
}

#[test]
fn capital_i_inserts_before_the_first_non_blank() {
    let mut app = composer_in_normal("  foo");
    type_keys(&mut app, "$");

    type_keys(&mut app, "Ix");

    assert_eq!(app.input.text, "  xfoo");
}

#[test]
fn o_opens_a_line_below_with_the_same_indent() {
    let mut app = composer_in_normal("  foo\nbar");

    type_keys(&mut app, "ox");

    assert_eq!(app.input.text, "  foo\n  x\nbar");
    assert_eq!(composer_mode(&app), mush_tui::app::AppMode::Insert);
}

#[test]
fn capital_o_opens_a_line_above() {
    let mut app = composer_in_normal("foo\nbar");

    type_keys(&mut app, "jOx");

    assert_eq!(app.input.text, "foo\nx\nbar");
}

#[test]
fn diw_and_daw_delete_a_word() {
    let mut app = composer_in_normal("one two three");
    type_keys(&mut app, "wdiw");
    assert_eq!(app.input.text, "one  three");

    let mut app = composer_in_normal("one two three");
    type_keys(&mut app, "wdaw");
    assert_eq!(app.input.text, "one three");
}

#[test]
fn ciw_changes_the_word_under_the_caret() {
    let mut app = composer_in_normal("one two three");

    type_keys(&mut app, "wlciwX");

    assert_eq!(app.input.text, "one X three");
}

#[test]
fn quote_objects_take_the_inside_or_the_whole_string() {
    let mut app = composer_in_normal(r#"say "hi there" now"#);
    type_keys(&mut app, "fhdi\"");
    assert_eq!(app.input.text, r#"say "" now"#);

    let mut app = composer_in_normal(r#"say "hi there" now"#);
    type_keys(&mut app, "fhda\"");
    assert_eq!(app.input.text, "say now");
}

#[test]
fn ci_quote_on_an_empty_string_types_between_the_quotes() {
    let mut app = composer_in_normal(r#"x = """#);

    type_keys(&mut app, "f\"ci\"a");

    assert_eq!(app.input.text, r#"x = "a""#);
}

#[test]
fn bracket_objects_take_what_is_between_or_the_whole_pair() {
    let mut app = composer_in_normal("call(a, [b]) x");
    type_keys(&mut app, "fa;di(");
    assert_eq!(app.input.text, "call() x");

    let mut app = composer_in_normal("call(a, [b]) x");
    type_keys(&mut app, "fbda[");
    assert_eq!(app.input.text, "call(a, ) x");

    let mut app = composer_in_normal("call(a, [b]) x");
    type_keys(&mut app, "fbdib");
    assert_eq!(app.input.text, "call() x");

    let mut app = composer_in_normal("f(g(x))");
    type_keys(&mut app, "fxd2ib");
    assert_eq!(app.input.text, "f()");
}

#[test]
fn di_brace_over_lines_takes_the_lines_inside() {
    let mut app = composer_in_normal("fn f() {\n    a();\n    b();\n}");

    type_keys(&mut app, "jdi{");

    assert_eq!(app.input.text, "fn f() {\n}");
}

#[test]
fn ci_brace_over_lines_leaves_an_indented_line_to_type_on() {
    let mut app = composer_in_normal("fn f() {\n    a();\n    b();\n}");

    type_keys(&mut app, "jci{x");

    assert_eq!(app.input.text, "fn f() {\n    x\n}");
}

#[test]
fn yi_bracket_then_p_puts_the_inside() {
    let mut app = composer_in_normal("[ab] ");

    type_keys(&mut app, "lyi[$p");

    assert_eq!(app.input.text, "[ab] ab");
}

#[test]
fn viw_selects_the_word_in_the_composer() {
    let mut app = composer_in_normal("one two three");
    type_keys(&mut app, "wl");

    type_keys(&mut app, "viw");
    assert_eq!(tinted_text(&app), "two");

    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Char('y')));
    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "two"),
        "{event:?}"
    );
}

#[test]
fn va_paren_selects_the_whole_pair() {
    let mut app = composer_in_normal("f(a, b) c");
    type_keys(&mut app, "fa");

    type_keys(&mut app, "va(");

    assert_eq!(tinted_text(&app), "(a, b)");
}

#[test]
fn d_and_x_delete_the_composer_selection_into_the_register() {
    let mut app = composer_in_normal("one two three");
    type_keys(&mut app, "wvee");
    type_keys(&mut app, "d");
    assert_eq!(app.input.text, "one ");
    assert_eq!(composer_mode(&app), mush_tui::app::AppMode::Normal);

    type_keys(&mut app, "0P");
    assert_eq!(app.input.text, "two threeone ");

    let mut app = composer_in_normal("abc");
    type_keys(&mut app, "vlx");
    assert_eq!(app.input.text, "c");
}

#[test]
fn capital_v_then_d_takes_whole_lines() {
    let mut app = composer_in_normal("a\nb\nc");

    type_keys(&mut app, "Vjd");

    assert_eq!(app.input.text, "c");
}

#[test]
fn c_and_s_change_the_composer_selection() {
    let mut app = composer_in_normal("one two");
    type_keys(&mut app, "wvecX");
    assert_eq!(app.input.text, "one X");
    assert_eq!(composer_mode(&app), mush_tui::app::AppMode::Insert);

    let mut app = composer_in_normal("one two");
    type_keys(&mut app, "veslast");
    assert_eq!(app.input.text, "last two");
}

#[test]
fn p_over_a_selection_swaps_it_for_the_register() {
    let mut app = composer_in_normal("one two");
    type_keys(&mut app, "yiwwviwp");
    assert_eq!(app.input.text, "one one");

    type_keys(&mut app, "0viwp");
    assert_eq!(
        app.input.text, "two one",
        "the register held what p replaced"
    );
}

#[test]
fn capital_p_over_a_selection_keeps_the_register() {
    let mut app = composer_in_normal("a b c");
    type_keys(&mut app, "yiwwviwP");
    type_keys(&mut app, "wviwP");

    assert_eq!(app.input.text, "a a a");
}

#[test]
fn a_line_put_over_a_word_lands_on_its_own_line() {
    let mut app = composer_in_normal("head\none two");
    type_keys(&mut app, "yyjwviwp");

    assert_eq!(app.input.text, "head\none \nhead\n");
}

#[test]
fn y_leaves_the_caret_at_the_start_of_the_selection() {
    let mut app = composer_in_normal("one two three");
    type_keys(&mut app, "wvey");

    assert_eq!(app.input.cursor, 4);
    type_keys(&mut app, "$p");
    assert_eq!(app.input.text, "one two threetwo");
}

#[test]
fn o_swaps_the_ends_of_the_composer_selection() {
    let mut app = composer_in_normal("one two three");
    type_keys(&mut app, "wve");

    type_keys(&mut app, "ob");

    assert_eq!(tinted_text(&app), "one two");
}

#[test]
fn r_replaces_characters_under_the_caret() {
    let mut app = composer_in_normal("abcd");
    type_keys(&mut app, "rx");
    assert_eq!(app.input.text, "xbcd");
    assert_eq!(app.input.cursor, 0);

    type_keys(&mut app, "l3ry");
    assert_eq!(app.input.text, "xyyy");
    assert_eq!(app.input.cursor, 3);
}

#[test]
fn r_past_the_end_of_the_line_does_nothing() {
    let mut app = composer_in_normal("ab\ncd");

    type_keys(&mut app, "3rx");

    assert_eq!(app.input.text, "ab\ncd");
}

#[test]
fn r_in_visual_mode_replaces_the_selection() {
    let mut app = composer_in_normal("one two");

    type_keys(&mut app, "ver-");

    assert_eq!(app.input.text, "--- two");
    assert_eq!(composer_mode(&app), mush_tui::app::AppMode::Normal);
}

#[test]
fn capital_j_joins_the_next_line_with_one_space() {
    let mut app = composer_in_normal("one\n    two\nthree");
    type_keys(&mut app, "J");
    assert_eq!(app.input.text, "one two\nthree");
    assert_eq!(app.input.cursor, 3);

    let mut app = composer_in_normal("a\nb\nc\nd");
    type_keys(&mut app, "3J");
    assert_eq!(app.input.text, "a b c\nd");
}

#[test]
fn capital_j_adds_no_space_after_a_space_or_before_a_paren() {
    let mut app = composer_in_normal("one \ntwo");
    type_keys(&mut app, "J");
    assert_eq!(app.input.text, "one two");

    let mut app = composer_in_normal("f(a\n)");
    type_keys(&mut app, "J");
    assert_eq!(app.input.text, "f(a)");
}

#[test]
fn capital_j_on_the_last_line_does_nothing() {
    let mut app = composer_in_normal("one");

    type_keys(&mut app, "J");

    assert_eq!(app.input.text, "one");
}

#[test]
fn capital_j_in_visual_mode_joins_the_selected_lines() {
    let mut app = composer_in_normal("a\nb\nc\nd");

    type_keys(&mut app, "Vjj");
    type_keys(&mut app, "J");

    assert_eq!(app.input.text, "a b c\nd");
}

#[test]
fn tilde_toggles_case_and_moves_on() {
    let mut app = composer_in_normal("Hello");
    type_keys(&mut app, "~");
    assert_eq!(app.input.text, "hello");
    assert_eq!(app.input.cursor, 1);

    type_keys(&mut app, "9~");
    assert_eq!(app.input.text, "hELLO");
    assert_eq!(app.input.cursor, 4);
}

#[test]
fn tilde_in_visual_mode_toggles_the_selection() {
    let mut app = composer_in_normal("one two");

    type_keys(&mut app, "wve~");

    assert_eq!(app.input.text, "one TWO");
    assert_eq!(app.input.cursor, 4);
}

#[test]
fn dot_repeats_a_delete() {
    let mut app = composer_in_normal("one two three four");

    type_keys(&mut app, "dw..");

    assert_eq!(app.input.text, "four");
}

#[test]
fn a_count_before_dot_replaces_the_old_count() {
    let mut app = composer_in_normal("abcdefghij");

    type_keys(&mut app, "2x");
    assert_eq!(app.input.text, "cdefghij");

    type_keys(&mut app, "3.");
    assert_eq!(app.input.text, "fghij");

    type_keys(&mut app, ".");
    assert_eq!(app.input.text, "ij", "the new count stuck");
}

#[test]
fn dot_repeats_a_change_with_what_was_typed() {
    let mut app = composer_in_normal("one two three");
    type_keys(&mut app, "cwnew");
    press(&mut app, KeyCode::Esc);

    type_keys(&mut app, "w.");

    assert_eq!(app.input.text, "new new three");
    assert_eq!(composer_mode(&app), mush_tui::app::AppMode::Normal);
    assert_eq!(app.input.cursor, 6);
}

#[test]
fn dot_repeats_an_append_at_the_line_end() {
    let mut app = composer_in_normal("one\ntwo");
    type_keys(&mut app, "A!");
    press(&mut app, KeyCode::Esc);

    type_keys(&mut app, "j.");

    assert_eq!(app.input.text, "one!\ntwo!");
}

#[test]
fn dot_repeats_an_opened_line() {
    let mut app = composer_in_normal("top");
    type_keys(&mut app, "onext");
    press(&mut app, KeyCode::Esc);

    type_keys(&mut app, ".");

    assert_eq!(app.input.text, "top\nnext\nnext");
}

#[test]
fn dot_repeats_dd_and_p() {
    let mut app = composer_in_normal("a\nb\nc\nd");
    type_keys(&mut app, "dd.");
    assert_eq!(app.input.text, "c\nd");

    type_keys(&mut app, "p.");
    assert_eq!(app.input.text, "c\nb\nb\nd");
}

#[test]
fn dot_repeats_r_j_and_tilde() {
    let mut app = composer_in_normal("abcd");
    type_keys(&mut app, "rxl.");
    assert_eq!(app.input.text, "xxcd");

    let mut app = composer_in_normal("a\nb\nc");
    type_keys(&mut app, "J.");
    assert_eq!(app.input.text, "a b c");

    let mut app = composer_in_normal("abcd");
    type_keys(&mut app, "~.");
    assert_eq!(app.input.text, "ABcd");
}

#[test]
fn dot_repeats_a_visual_delete_over_as_much_text() {
    let mut app = composer_in_normal("abcdefgh");

    type_keys(&mut app, "vld");
    type_keys(&mut app, ".");

    assert_eq!(app.input.text, "efgh");
}

#[test]
fn a_yank_is_not_a_change_to_repeat() {
    let mut app = composer_in_normal("abc");

    type_keys(&mut app, "xyl.");

    assert_eq!(app.input.text, "c");
}

#[test]
fn dot_with_nothing_to_repeat_does_nothing() {
    let mut app = composer_in_normal("abc");

    type_keys(&mut app, ".");

    assert_eq!(app.input.text, "abc");
}

#[test]
fn a_repeated_change_undoes_in_one_step() {
    let mut app = composer_in_normal("one two three");
    type_keys(&mut app, "cwnew");
    press(&mut app, KeyCode::Esc);
    type_keys(&mut app, "w.");

    type_keys(&mut app, "u");

    assert_eq!(app.input.text, "new two three");
}

#[test]
fn operators_work_over_wide_and_accented_text() {
    let mut app = composer_in_normal("naïve 日本語 café");
    type_keys(&mut app, "wdw");
    assert_eq!(app.input.text, "naïve café");

    type_keys(&mut app, "ciwthé");
    assert_eq!(app.input.text, "naïve thé");

    press(&mut app, KeyCode::Esc);
    type_keys(&mut app, "0fïx");
    assert_eq!(app.input.text, "nave thé");

    type_keys(&mut app, "$p");
    assert_eq!(app.input.text, "nave théï");
}

#[test]
fn capital_y_yanks_to_the_end_of_the_line_like_neovim() {
    let mut app = composer_in_normal("one two\nthree");
    type_keys(&mut app, "w");

    let event = handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
    );
    type_keys(&mut app, "$p");

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "two"),
        "{event:?}"
    );
    assert_eq!(app.input.text, "one twotwo\nthree");
}

#[test]
fn a_counted_capital_y_reaches_into_the_next_line() {
    let mut app = composer_in_normal("one two\nthree\nfour");
    type_keys(&mut app, "w2Y");

    type_keys(&mut app, "GP");

    assert_eq!(app.input.text, "one two\nthree\ntwo\nthreefour");
}

#[test]
fn capital_y_still_copies_a_selection_raw() {
    let mut app = composer_in_normal("  one\n    two");
    press_with(&mut app, KeyCode::Char('V'), KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Char('j'));

    let event = handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
    );

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "  one\n    two"),
        "{event:?}"
    );
}

#[test]
fn braces_move_between_composer_paragraphs() {
    let mut app = composer_in_normal("one\ntwo\n\nthree\n\nfour");

    type_keys(&mut app, "}");
    assert_eq!(app.input.cursor, 8);

    type_keys(&mut app, "2}");
    assert_eq!(
        app.input.cursor, 19,
        "past the last paragraph it rests on the end"
    );

    type_keys(&mut app, "{");
    assert_eq!(app.input.cursor, 15);
}

#[test]
fn d_closing_brace_deletes_the_rest_of_the_paragraph() {
    let mut app = composer_in_normal("one\ntwo\n\nthree");

    type_keys(&mut app, "d}");

    assert_eq!(app.input.text, "\nthree");
}

#[test]
fn dip_and_dap_take_a_composer_paragraph() {
    let mut app = composer_in_normal("one\ntwo\n\nthree");
    type_keys(&mut app, "jdip");
    assert_eq!(app.input.text, "\nthree");

    let mut app = composer_in_normal("one\ntwo\n\nthree");
    type_keys(&mut app, "dap");
    assert_eq!(app.input.text, "three");
}

#[test]
fn vip_selects_the_paragraph_lines() {
    let mut app = composer_in_normal("one\ntwo\n\nthree");

    type_keys(&mut app, "vip");

    assert_eq!(tinted_text(&app), "onetwo");
}

#[test]
fn cis_changes_a_sentence_and_das_deletes_one() {
    let mut app = composer_in_normal("Fix the bug. Then test it. Ship.");
    type_keys(&mut app, "fTcisRun the tests.");
    assert_eq!(app.input.text, "Fix the bug. Run the tests. Ship.");

    press(&mut app, KeyCode::Esc);
    type_keys(&mut app, "0das");
    assert_eq!(app.input.text, "Run the tests. Ship.");
}

#[test]
fn a_motion_key_types_nothing() {
    let mut app = composer_in_normal("hello");

    press(&mut app, KeyCode::Char('w'));

    assert_eq!(app.input.text, "hello");
}

#[test]
fn esc_steps_back_onto_the_last_character() {
    let mut app = conversation(1);
    for c in "hello".chars() {
        press(&mut app, KeyCode::Char(c));
    }

    press(&mut app, KeyCode::Esc);

    assert_eq!(app.input.text, "hello");
    assert_eq!(app.input.cursor, 4);
}

#[test]
fn i_returns_to_insert_at_the_caret() {
    let mut app = composer_in_normal("hello");
    press(&mut app, KeyCode::Char('$'));

    press(&mut app, KeyCode::Char('i'));
    press(&mut app, KeyCode::Char('!'));

    assert_eq!(app.input.text, "hell!o");
}

#[test]
fn ctrl_o_moves_focus_to_the_log() {
    let mut app = composer_in_normal("hello");
    rows(&app, 80, SCREEN);

    press_with(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    let before = app.viewport.cursor().line;
    press(&mut app, KeyCode::Char('k'));

    assert_eq!(app.viewport.cursor().line, before - 1);
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn ctrl_o_comes_back_to_the_composer() {
    let mut app = composer_in_normal("hello");
    rows(&app, 80, SCREEN);
    press_with(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);

    press_with(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);

    assert_eq!(app.interaction.mode, mush_tui::app::AppMode::Normal);
    assert!(!app.interaction.focus.is_log());
}

#[test]
fn i_from_the_log_appends_at_the_end_of_the_composer() {
    let mut app = composer_in_normal("hello");
    rows(&app, 80, SCREEN);
    press_with(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);

    press(&mut app, KeyCode::Char('i'));
    press(&mut app, KeyCode::Char('!'));

    assert_eq!(app.input.text, "hello!");
}

#[test]
fn esc_with_the_log_focused_does_nothing() {
    let mut app = composer_in_normal("hello");
    rows(&app, 80, SCREEN);
    press_with(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Char('k'));
    let before = app.viewport.cursor();

    press(&mut app, KeyCode::Esc);

    assert_eq!(app.viewport.cursor(), before);
    assert_eq!(app.interaction.mode, mush_tui::app::AppMode::Normal);
    assert!(app.interaction.focus.is_log());
}

fn tinted_text(app: &App) -> String {
    let area = Rect::new(0, 0, 80, SCREEN);
    let mut buffer = Buffer::empty(area);
    Screen::new().render(app, area, &mut buffer);
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            if buffer[(x, y)].style().bg == app.theme.selection.bg {
                out.push_str(buffer[(x, y)].symbol());
            }
        }
    }
    out
}

#[test]
fn v_starts_a_selection_at_the_composer_caret() {
    let mut app = composer_in_normal("hello world");
    press(&mut app, KeyCode::Char('w'));

    press(&mut app, KeyCode::Char('v'));

    assert_eq!(tinted_text(&app), "w");
}

#[test]
fn esc_drops_the_composer_selection() {
    let mut app = composer_in_normal("hello world");
    press(&mut app, KeyCode::Char('w'));
    press(&mut app, KeyCode::Char('v'));

    press(&mut app, KeyCode::Esc);

    assert_eq!(tinted_text(&app), "");
    assert_eq!(app.interaction.mode, mush_tui::app::AppMode::Normal);
    assert!(!app.interaction.focus.is_log());
}

#[test]
fn a_motion_extends_the_composer_selection() {
    let mut app = composer_in_normal("hello world");
    press(&mut app, KeyCode::Char('w'));
    press(&mut app, KeyCode::Char('v'));

    press(&mut app, KeyCode::Char('e'));

    assert_eq!(tinted_text(&app), "world");
}

#[test]
fn capital_v_takes_the_whole_composer_line() {
    let mut app = composer_in_normal("hello\nworld");

    press_with(&mut app, KeyCode::Char('V'), KeyModifiers::SHIFT);

    assert_eq!(tinted_text(&app), "hello");
}

#[test]
fn y_copies_the_composer_selection_and_returns_to_normal() {
    let mut app = composer_in_normal("hello world");
    press(&mut app, KeyCode::Char('w'));
    press(&mut app, KeyCode::Char('v'));
    press(&mut app, KeyCode::Char('e'));

    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Char('y')));
    press(&mut app, KeyCode::Char('z'));

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "world"),
        "{event:?}"
    );
    assert_eq!(tinted_text(&app), "");
    assert_eq!(app.input.text, "hello world");
}

#[test]
fn y_in_the_composer_strips_the_common_indent() {
    let mut app = composer_in_normal("  one\n    two");
    press_with(&mut app, KeyCode::Char('V'), KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Char('j'));

    let event = handle_key(&mut app, KeyEvent::from(KeyCode::Char('y')));

    assert!(
        matches!(&event, Some(AppEvent::Copy { text }) if text == "one\n  two"),
        "{event:?}"
    );
}

#[test]
fn ctrl_a_selects_the_whole_composer() {
    let mut app = composer_in_normal("hello\nworld");

    press_with(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);

    assert_eq!(tinted_text(&app), "helloworld");
}

#[test]
fn backspace_does_nothing_to_the_log() {
    let mut app = log_with_world();
    press(&mut app, KeyCode::Char('v'));
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('l'));
    }
    let before = window_rows(&app);
    let selected = selected_cells(&app);

    press(&mut app, KeyCode::Backspace);

    assert_eq!(window_rows(&app), before);
    assert_eq!(selected_cells(&app), selected);
}
