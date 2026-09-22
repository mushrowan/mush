use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mush_ai::types::TokenCount;
use ratatui::backend::TestBackend;

use super::*;
use crate::app::AppMode;
use crate::app_event::Focus;
use crate::input::handle_key;
use crate::settings::ScopedSettings;
use crate::test_support::buffer_to_string;

#[derive(Debug, Clone, Copy)]
enum Owner {
    Log,
    Composer,
    Hidden,
}

const DRAFTS: [&str; 4] = [
    "",
    "draft",
    "first draft line\nsecond draft line\nthird draft line",
    "a draft long enough to wrap across several rows in a narrow viewport and keep its cursor",
];

fn synthetic_app(streaming: bool) -> App {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.cwd = "/synthetic".into();
    for line in 0..30 {
        app.push_user_message(format!("synthetic log line {line}"));
    }
    if streaming {
        app.start_streaming();
        app.push_text_delta("synthetic response");
        app.tick_animation();
    }
    app
}

fn draw(terminal: &mut Terminal<TestBackend>, screen: &mut Screen, app: &mut App) {
    let text = app.input.text.clone();
    let cursor = app.input.cursor;
    let messages: Vec<_> = app.messages.iter().map(|m| m.content.clone()).collect();
    let stream = app.stream.text.clone();
    let active = app.stream.active;
    terminal
        .draw(|frame| draw_frame(frame, screen, app, &ScopedSettings::default()))
        .unwrap();
    assert_eq!(app.input.text, text);
    assert_eq!(app.input.cursor, cursor);
    assert_eq!(app.stream.text, stream);
    assert_eq!(app.stream.active, active);
    assert_eq!(
        app.messages.iter().map(|m| &m.content).collect::<Vec<_>>(),
        messages.iter().collect::<Vec<_>>()
    );
}

fn key(app: &mut App, code: KeyCode) {
    assert!(handle_key(app, KeyEvent::new(code, KeyModifiers::NONE)).is_none());
}

fn type_draft(app: &mut App, draft: &str) {
    for c in draft.chars() {
        let event = if c == '\n' {
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
        } else {
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
        };
        assert!(handle_key(app, event).is_none());
    }
}

fn switch_focus(app: &mut App) {
    assert!(
        handle_key(
            app,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)
        )
        .is_none()
    );
}

fn assert_owner(terminal: &Terminal<TestBackend>, app: &App, owner: Owner) {
    let backend = terminal.backend();
    let expected = match owner {
        Owner::Log => Some((
            app.viewport.area().x + app.viewport.cursor().column as u16,
            app.viewport.area().y + app.viewport.cursor_row(),
        )),
        Owner::Composer => {
            Some(widgets::input_box::InputBox::new(app).cursor_position(app.input.area.get()))
        }
        Owner::Hidden => None,
    };
    assert_eq!(
        backend.cursor_visible(),
        expected.is_some(),
        "owner={owner:?} mode={:?} focus={:?} streaming={} draft={:?}",
        app.interaction.mode,
        app.interaction.focus,
        app.stream.active,
        app.input.text
    );
    if let Some(expected) = expected {
        assert_eq!(backend.cursor_position(), expected.into(), "{owner:?}");
    }
}

fn repeat_frames(
    terminal: &mut Terminal<TestBackend>,
    screen: &mut Screen,
    app: &mut App,
    owner: Owner,
) {
    for _ in 0..3 {
        app.tick_animation();
        draw(terminal, screen, app);
        assert_owner(terminal, app, owner);
    }
}

fn log_draft_matrix(streaming: bool) {
    let mut failures = Vec::new();
    for draft in DRAFTS {
        let mut app = synthetic_app(streaming);
        let mut screen = Screen::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        draw(&mut terminal, &mut screen, &mut app);
        type_draft(&mut app, draft);
        let input_cursor = app.input.cursor;
        switch_focus(&mut app);
        assert_eq!(app.interaction.mode, AppMode::Normal);
        assert_eq!(app.interaction.focus, Focus::Log);
        assert_eq!(app.input.cursor, input_cursor);

        for (width, height) in [(80, 24), (24, 18), (80, 24)] {
            terminal.backend_mut().resize(width, height);
            for frame in 0..3 {
                if streaming {
                    app.push_text_delta(" more");
                    app.tick_animation();
                }
                draw(&mut terminal, &mut screen, &mut app);
                let expected = (
                    app.viewport.area().x + app.viewport.cursor().column as u16,
                    app.viewport.area().y + app.viewport.cursor_row(),
                );
                let actual = terminal.backend().cursor_position();
                let visible = terminal.backend().cursor_visible();
                if !visible || actual != expected.into() {
                    failures.push(format!(
                        "draft={draft:?} streaming={streaming} size={width}x{height} frame={frame} expected={expected:?} actual={actual:?} visible={visible}"
                    ));
                }
                assert_eq!(app.input.text, draft);
                assert_eq!(app.input.cursor, input_cursor);
                if draft == "draft" {
                    assert!(buffer_to_string(terminal.backend().buffer()).contains("> draft"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn normal_log_cursor_with_draft_idle() {
    log_draft_matrix(false);
}

#[test]
fn normal_log_cursor_with_draft_streaming() {
    log_draft_matrix(true);
}

#[test]
fn composer_modes_own_the_input_cursor() {
    for streaming in [false, true] {
        for draft in DRAFTS {
            for mode in [AppMode::Insert, AppMode::Normal, AppMode::Visual] {
                let mut app = synthetic_app(streaming);
                app.input.text = draft.into();
                app.input.cursor = draft.len().saturating_sub(2);
                app.interaction.mode = mode.clone();
                if mode == AppMode::Visual {
                    app.input.anchor = Some(0);
                }
                let anchor = app.input.anchor;
                let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
                let mut screen = Screen::new();
                let owner = if streaming && draft.is_empty() {
                    Owner::Hidden
                } else {
                    Owner::Composer
                };
                for (width, height) in [(80, 24), (24, 18)] {
                    terminal.backend_mut().resize(width, height);
                    repeat_frames(&mut terminal, &mut screen, &mut app, owner);
                    assert_eq!(app.input.anchor, anchor);
                }
                assert_eq!(
                    app.interaction.mode.caret(Focus::Composer).is_block(),
                    mode != AppMode::Insert
                );
            }
        }
    }
}

#[test]
fn focus_escape_navigation_and_selection_keep_the_draft() {
    for streaming in [false, true] {
        let mut app = synthetic_app(streaming);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut screen = Screen::new();
        type_draft(&mut app, "keep this draft");
        repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Composer);
        let end = app.input.cursor;
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.interaction.mode, AppMode::Normal);
        assert_eq!(app.interaction.focus, Focus::Composer);
        assert_eq!(app.input.cursor, end - 1);
        repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Composer);
        let input_cursor = app.input.cursor;
        key(&mut app, KeyCode::Esc);
        repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Log);
        let log_cursor = app.viewport.cursor();
        key(&mut app, KeyCode::Char('k'));
        repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Log);
        assert_eq!(app.viewport.cursor().line, log_cursor.line - 1);
        key(&mut app, KeyCode::Char('v'));
        assert_eq!(app.interaction.mode, AppMode::Visual);
        key(&mut app, KeyCode::Char('k'));
        let selection = app.viewport.selection();
        assert!(selection.is_some());
        repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Log);
        assert_eq!(app.viewport.selection(), selection);
        assert!(app.interaction.mode.caret(app.interaction.focus).is_block());
        key(&mut app, KeyCode::Esc);
        assert!(app.viewport.selection().is_none());
        repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Log);
        switch_focus(&mut app);
        assert_eq!(app.interaction.focus, Focus::Composer);
        assert_eq!(app.input.cursor, input_cursor);
        repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Composer);
        switch_focus(&mut app);
        repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Log);
        key(&mut app, KeyCode::Char('i'));
        assert_eq!(app.interaction.mode, AppMode::Insert);
        assert_eq!(app.interaction.focus, Focus::Composer);
        assert_eq!(app.input.cursor, end);
        repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Composer);
        assert_eq!(app.input.text, "keep this draft");
    }
}

#[test]
fn log_cursor_stays_scrolled_away_from_the_streaming_tail() {
    for draft in DRAFTS {
        let mut app = synthetic_app(true);
        app.input.text = draft.into();
        app.input.cursor = draft.len();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut screen = Screen::new();
        draw(&mut terminal, &mut screen, &mut app);
        switch_focus(&mut app);
        key(&mut app, KeyCode::Char('g'));
        repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Log);
        assert_eq!(app.viewport.cursor().line, 0);
        assert!(!app.viewport.is_following());
        for (width, height) in [(24, 18), (80, 24)] {
            terminal.backend_mut().resize(width, height);
            app.push_text_delta("\nmore synthetic output");
            repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Log);
            assert_eq!(app.viewport.cursor().line, 0);
            assert!(!app.viewport.is_following());
            assert_eq!(app.input.text, draft);
        }
    }
}

#[test]
fn modal_modes_hide_the_underlying_cursor() {
    for streaming in [false, true] {
        for draft in ["", "draft"] {
            for focus in [Focus::Composer, Focus::Log] {
                let mut app = synthetic_app(streaming);
                app.input.text = draft.into();
                app.input.cursor = draft.len();
                app.interaction.focus = focus;
                let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
                let mut screen = Screen::new();
                for mode in [
                    AppMode::SessionPicker,
                    AppMode::ModelPicker,
                    AppMode::LoginPicker,
                    AppMode::AtPicker,
                    AppMode::SlotEdit,
                    AppMode::ToolConfirm,
                    AppMode::Search,
                    AppMode::Settings,
                    AppMode::CompactionRecovery,
                ] {
                    app.interaction.mode = mode;
                    repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Hidden);
                }
            }
        }
    }
}

#[test]
fn picker_open_and_close_release_cursor_ownership() {
    for streaming in [false, true] {
        for focus in [Focus::Composer, Focus::Log] {
            let mut app = synthetic_app(streaming);
            app.input.text = "draft".into();
            app.input.cursor = 2;
            app.interaction.mode = AppMode::Normal;
            app.interaction.focus = focus;
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let mut screen = Screen::new();
            let owner = if focus.is_log() {
                Owner::Log
            } else {
                Owner::Composer
            };
            repeat_frames(&mut terminal, &mut screen, &mut app, owner);
            app.open_session_picker(vec![], "/synthetic".into());
            repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Hidden);
            assert!(buffer_to_string(terminal.backend().buffer()).contains("sessions (this dir)"));
            for (width, height) in [(24, 18), (80, 24)] {
                terminal.backend_mut().resize(width, height);
                repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Hidden);
            }
            key(&mut app, KeyCode::Esc);
            assert!(app.interaction.session_picker.is_none());
            repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Composer);
            assert_eq!(app.input.cursor, 2);
            assert_eq!(app.input.text, "draft");
        }
    }
}

#[test]
fn slash_completion_retains_the_input_cursor() {
    for streaming in [false, true] {
        let mut app = synthetic_app(streaming);
        app.input.text = "/help".into();
        app.input.cursor = 3;
        app.completion.slash_commands = vec![crate::slash_menu::SlashCommand {
            name: "help".into(),
            description: "synthetic help".into(),
        }];
        app.open_slash_menu();
        assert_eq!(app.interaction.mode, AppMode::SlashComplete);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut screen = Screen::new();
        repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Composer);
        assert!(buffer_to_string(terminal.backend().buffer()).contains("synthetic help"));
        for (width, height) in [(24, 18), (80, 24)] {
            terminal.backend_mut().resize(width, height);
            repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Composer);
        }
        key(&mut app, KeyCode::Esc);
        assert!(app.completion.slash_menu.is_none());
        repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Composer);
        assert_eq!(app.input.cursor, 3);
        assert_eq!(app.input.text, "/help");
    }
}

#[test]
fn tool_confirmation_releases_and_restores_the_log_cursor() {
    let mut app = synthetic_app(true);
    app.input.text = "draft".into();
    app.input.cursor = 2;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut screen = Screen::new();
    draw(&mut terminal, &mut screen, &mut app);
    switch_focus(&mut app);
    key(&mut app, KeyCode::Char('k'));
    repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Log);
    let cursor = app.viewport.cursor();
    app.interaction.mode = AppMode::ToolConfirm;
    app.interaction.confirm_prompt = Some("synthetic confirmation".into());
    repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Hidden);
    assert!(buffer_to_string(terminal.backend().buffer()).contains("synthetic confirmation"));
    app.interaction.confirm_prompt = None;
    app.interaction.mode = AppMode::Normal;
    repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Log);
    assert_eq!(app.viewport.cursor(), cursor);
}

#[test]
fn subagents_overlay_preserves_log_and_modal_across_live_frames() {
    let mut app = synthetic_app(true);
    app.input.text = "draft 界".into();
    app.input.cursor = 2;
    app.input.anchor = Some(4);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut screen = Screen::new();
    draw(&mut terminal, &mut screen, &mut app);
    switch_focus(&mut app);
    key(&mut app, KeyCode::Char('k'));
    let cursor = app.viewport.cursor();
    let following = app.viewport.is_following();
    let toggle = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
    handle_key(&mut app, toggle);
    repeat_frames(&mut terminal, &mut screen, &mut app, Owner::Hidden);
    assert!(buffer_to_string(terminal.backend().buffer()).contains("subagents"));
    assert_eq!(app.viewport.cursor(), cursor);
    assert_eq!(app.viewport.is_following(), following);
    app.push_text_delta(" continues under the panel");
    draw(&mut terminal, &mut screen, &mut app);
    handle_key(&mut app, toggle);
    draw(&mut terminal, &mut screen, &mut app);
    assert_eq!(app.viewport.cursor(), cursor);
    assert_eq!(app.input.anchor, Some(4));
    assert_eq!(app.input.cursor, 2);
    app.open_session_picker(Vec::new(), "/synthetic".into());
    handle_key(&mut app, toggle);
    for (width, height) in [(45, 12), (8, 4), (1, 1), (80, 24)] {
        terminal.backend_mut().resize(width, height);
        draw(&mut terminal, &mut screen, &mut app);
        assert!(!terminal.backend().cursor_visible());
    }
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.interaction.mode, AppMode::SessionPicker);
    assert!(app.interaction.session_picker.is_some());
    assert!(app.stream.active);
    assert_eq!(app.input.text, "draft 界");
}

#[test]
fn the_message_picker_lists_numbered_messages_with_the_newest_highlighted() {
    let mut app = synthetic_app(false);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut screen = Screen::new();
    app.open_message_picker(
        crate::message_picker::MessagePickerPurpose::Fork,
        crate::message_picker::user_message_items(&[
            crate::test_support::user_message("fix the build"),
            crate::test_support::user_message("ship it"),
        ]),
    );

    draw(&mut terminal, &mut screen, &mut app);

    let text = buffer_to_string(terminal.backend().buffer());
    assert!(text.contains("fork from message"), "{text}");
    assert!(text.contains("  1  fix the build"), "{text}");
    assert!(text.contains("▸ 2  ship it"), "{text}");
    assert!(text.contains("enter fork before it"), "{text}");
    assert!(!terminal.backend().cursor_visible());
}

#[test]
fn the_terminal_title_names_the_cwd() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.cwd = "~/dev/mush".into();

    assert_eq!(terminal_title(&app), "mush - mush");
}

#[test]
fn the_terminal_title_carries_the_session_title() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.cwd = "~/dev/mush".into();
    app.session_title = Some("port\nprime".into());

    assert_eq!(terminal_title(&app), "mush - port prime - mush");
}

#[test]
fn a_trailing_slash_does_not_blank_the_cwd() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.cwd = "/tmp/work/".into();

    assert_eq!(terminal_title(&app), "mush - work");
}

#[test]
fn resizing_a_long_scrolled_log_keeps_the_same_message() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for index in 0..400 {
        app.push_user_message(format!(
            "marker-{index:03} {}",
            "words that wrap differently when the terminal becomes narrow ".repeat(3)
        ));
    }
    let mut screen = Screen::new();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    draw(&mut terminal, &mut screen, &mut app);
    switch_focus(&mut app);
    app.viewport.page_by(-200);
    draw(&mut terminal, &mut screen, &mut app);
    let cursor_message = app.viewport.cursor().line;
    let before = crate::selection::transcript_rows(&app)[cursor_message]
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    terminal.backend_mut().resize(50, 24);
    draw(&mut terminal, &mut screen, &mut app);
    let after = crate::selection::transcript_rows(&app)[app.viewport.cursor().line]
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    assert_eq!(after, before);
}
