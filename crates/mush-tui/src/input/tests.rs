use super::pickers::apply_model_completions;
use super::*;
use crate::session_picker::SessionScope;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use mush_ai::types::{ThinkingLevel, TokenCount};
use ratatui::layout::Rect;
use ratatui::text::Line;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

#[test]
fn apply_model_completions_populates_completion_state_from_merged_rows() {
    use mush_ai::discovery::{MergedModel, ModelSource};
    use mush_ai::types::{Api, InputModality, Model, ModelCost, Provider, TokenCount};

    let mut app = App::new("test".into(), TokenCount::new(200_000));
    assert!(app.completion.model_completions.is_empty());

    let merged = vec![MergedModel {
        model: Model {
            id: "gpt-test-discovered".into(),
            name: "GPT Test (discovered)".into(),
            api: Api::OpenaiResponses,
            provider: Provider::Custom("openai-codex".into()),
            base_url: "https://chatgpt.com/backend-api".into(),
            reasoning: true,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(400_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(64_000),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        },
        source: ModelSource::Discovered,
        raw: None,
    }];

    apply_model_completions(&mut app, &merged);

    assert_eq!(app.completion.model_completions.len(), 1);
    assert_eq!(
        app.completion.model_completions[0].id,
        "gpt-test-discovered"
    );
    assert!(
        app.completion
            .completions
            .iter()
            .any(|c| c == "gpt-test-discovered")
    );
}

#[test]
fn ctrl_c_with_empty_input_arms_quit_confirm() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(event.is_none());
    assert!(app.quit_hint_armed());
    assert_eq!(
        app.status.as_ref().map(|s| s.text.as_str()),
        Some("press ctrl+c again to exit")
    );
}

#[test]
fn ctrl_c_twice_quits() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let _ = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(matches!(event, Some(AppEvent::Quit)));
}

#[test]
fn ctrl_c_with_text_arms_quit_without_clearing() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "draft prompt".into();
    app.input.cursor = app.input.text.len();
    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(event.is_none());
    assert_eq!(app.input.text, "draft prompt");
    assert!(app.quit_hint_armed());
}

#[test]
fn ctrl_c_twice_quits_with_text_still_in_the_composer() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "stuff".into();
    app.input.cursor = 5;
    let _ = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(matches!(event, Some(AppEvent::Quit)), "{event:?}");
    assert_eq!(app.input.text, "stuff");
}

#[test]
fn any_other_key_resets_quit_confirm() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let _ = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(app.quit_hint_armed());
    let _ = handle_key(&mut app, key(KeyCode::Char('a')));
    assert!(!app.quit_hint_armed());
    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(event.is_none());
    assert_eq!(app.input.text, "a");
}

#[test]
fn ctrl_c_when_busy_aborts_and_arms_the_exit_hint() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(matches!(event, Some(AppEvent::Abort)));
    assert!(app.quit_hint_armed());

    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(matches!(event, Some(AppEvent::Quit)), "{event:?}");
}

#[test]
fn the_exit_hint_expires() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let _ = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    app.quit_hint_until = Some(std::time::Instant::now() - std::time::Duration::from_millis(1));

    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));

    assert!(event.is_none(), "{event:?}");
    assert!(app.quit_hint_armed());
}

#[test]
fn ctrl_v_emits_paste_image_when_idle() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let event = handle_key(&mut app, ctrl(KeyCode::Char('v')));
    assert!(matches!(event, Some(AppEvent::PasteImage)));
}

#[test]
fn ctrl_v_emits_paste_image_when_streaming() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    let event = handle_key(&mut app, ctrl(KeyCode::Char('v')));
    assert!(matches!(event, Some(AppEvent::PasteImage)));
}

#[test]
fn escape_does_not_abort_when_streaming() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    let event = handle_key(&mut app, key(KeyCode::Esc));
    assert!(event.is_none(), "{event:?}");
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(!app.interaction.focus.is_log());
}

#[test]
fn escape_does_nothing_when_idle() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let event = handle_key(&mut app, key(KeyCode::Esc));
    assert!(event.is_none());
}

#[test]
fn escape_does_not_abort_while_tool_runs_between_messages() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = false;
    app.active_tools
        .push(crate::display_types::ActiveToolState {
            tool_call_id: "bash_1".into(),
            name: "bash".into(),
            summary: "running".into(),
            live_output: None,
            status: crate::display_types::ToolCallStatus::Running,
            output: None,
        });
    assert!(app.is_busy());

    let event = handle_key(&mut app, key(KeyCode::Esc));
    assert!(event.is_none(), "{event:?}");
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(!app.interaction.focus.is_log());
}

#[test]
fn typing_characters() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    handle_key(&mut app, key(KeyCode::Char('h')));
    handle_key(&mut app, key(KeyCode::Char('i')));
    assert_eq!(app.input.text, "hi");
    assert_eq!(app.input.cursor, 2);
}

#[test]
fn enter_submits_input() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "hello".into();
    app.input.cursor = 5;
    let event = handle_key(&mut app, key(KeyCode::Enter));
    match event {
        Some(AppEvent::UserSubmit { text, images }) => {
            assert_eq!(text, "hello");
            assert!(images.is_empty());
        }
        other => panic!("expected UserSubmit, got {other:?}"),
    }
    assert!(app.input.text.is_empty());
}

#[test]
fn enter_on_empty_does_nothing() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let event = handle_key(&mut app, key(KeyCode::Enter));
    assert!(event.is_none());
}

#[test]
fn backspace_deletes() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "abc".into();
    app.input.cursor = 3;
    handle_key(&mut app, key(KeyCode::Backspace));
    assert_eq!(app.input.text, "ab");
}

#[test]
fn ctrl_u_clears_line() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "long text here".into();
    app.input.cursor = 14;
    handle_key(&mut app, ctrl(KeyCode::Char('u')));
    assert!(app.input.text.is_empty());
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn home_end_navigation() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "hello".into();
    app.input.cursor = 3;

    handle_key(&mut app, key(KeyCode::Home));
    assert_eq!(app.input.cursor, 0);

    handle_key(&mut app, key(KeyCode::End));
    assert_eq!(app.input.cursor, 5);
}

#[test]
fn ctrl_t_cycles_thinking_level() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    assert_eq!(app.thinking_level, ThinkingLevel::Off);

    let event = handle_key(&mut app, ctrl(KeyCode::Char('t')));
    assert!(matches!(event, Some(AppEvent::CycleThinkingLevel)));
    assert_eq!(app.thinking_level, ThinkingLevel::Low);

    handle_key(&mut app, ctrl(KeyCode::Char('t')));
    assert_eq!(app.thinking_level, ThinkingLevel::Medium);
}

#[test]
fn ctrl_t_walks_only_curated_levels_when_supplied() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.supported_thinking_levels = vec![
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
    ];
    app.thinking_level = ThinkingLevel::Low;

    handle_key(&mut app, ctrl(KeyCode::Char('t')));
    assert_eq!(app.thinking_level, ThinkingLevel::Medium);
    handle_key(&mut app, ctrl(KeyCode::Char('t')));
    assert_eq!(app.thinking_level, ThinkingLevel::High);
    handle_key(&mut app, ctrl(KeyCode::Char('t')));
    assert_eq!(app.thinking_level, ThinkingLevel::Low);
}

#[test]
fn ctrl_t_resets_to_first_curated_level_when_current_not_supported() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.supported_thinking_levels = vec![ThinkingLevel::Medium, ThinkingLevel::High];
    app.thinking_level = ThinkingLevel::Xhigh;

    handle_key(&mut app, ctrl(KeyCode::Char('t')));
    assert_eq!(app.thinking_level, ThinkingLevel::Medium);
}

#[test]
fn ctrl_o_no_longer_toggles_thinking_expanded() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.push_thinking_delta("thoughts");
    app.push_text_delta("text");
    app.finish_streaming(None, None);

    let initial = app.messages[0].thinking_expanded;
    handle_key(&mut app, ctrl(KeyCode::Char('o')));
    assert_eq!(
        app.messages[0].thinking_expanded, initial,
        "ctrl+o must not toggle thinking expansion any more"
    );
}

#[test]
fn alt_enter_inserts_newline() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input_char('a');
    let event = handle_key(
        &mut app,
        KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::ALT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        },
    );
    assert!(event.is_none());
    assert_eq!(app.input.text, "a\n");
}

#[test]
fn ctrl_j_inserts_newline() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input_char('a');
    let event = handle_key(
        &mut app,
        KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        },
    );
    assert!(event.is_none());
    assert_eq!(app.input.text, "a\n");
}

#[test]
fn slash_command_parsed() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "/help".into();
    app.input.cursor = 5;
    let event = handle_key(&mut app, key(KeyCode::Enter));
    match event {
        Some(AppEvent::SlashCommand {
            action: crate::slash::SlashAction::Help,
        }) => {}
        other => panic!("expected SlashCommand, got {other:?}"),
    }
}

#[test]
fn slash_command_with_args() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion
        .slash_commands
        .push(crate::app::SlashCommand {
            name: "review".into(),
            description: "review a file".into(),
        });
    app.input.text = "/review src/main.rs".into();
    app.input.cursor = 19;
    let event = handle_key(&mut app, key(KeyCode::Enter));
    match event {
        Some(AppEvent::SlashCommand {
            action: crate::slash::SlashAction::Other { name, args },
        }) => {
            assert_eq!(name, "review");
            assert_eq!(args, "src/main.rs");
        }
        other => panic!("expected SlashCommand, got {other:?}"),
    }
}

fn app_with_commands(names: &[&str]) -> App {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.slash_commands = names
        .iter()
        .map(|name| crate::app::SlashCommand {
            name: (*name).into(),
            description: String::new(),
        })
        .collect();
    app
}

#[test]
fn a_slash_typo_is_refused_with_a_suggestion() {
    let mut app = app_with_commands(&["help", "resume", "skill:python"]);
    for (typed, message) in [
        ("/resuem", "Unknown command: /resuem. Did you mean /resume?"),
        (
            "/skill:pythno fix the bug",
            "Unknown command: /skill:pythno. Did you mean /skill:python?",
        ),
    ] {
        app.input.text = typed.into();
        app.input.cursor = typed.len();

        let event = handle_key(&mut app, key(KeyCode::Enter));

        assert!(event.is_none(), "{event:?}");
        assert_eq!(app.input.text, typed);
        assert_eq!(app.status.as_ref().map(|s| s.text.as_str()), Some(message));
    }
}

#[test]
fn a_slash_prompt_with_no_near_command_is_sent_as_is() {
    let mut app = app_with_commands(&["help", "mcp", "resume"]);
    for typed in ["/usr/bin/foo is broken", "/tmp notes for the cleanup"] {
        app.input.text = typed.into();
        app.input.cursor = typed.len();

        let event = handle_key(&mut app, key(KeyCode::Enter));

        match event {
            Some(AppEvent::UserSubmit { text, .. }) => assert_eq!(text, typed),
            other => panic!("expected UserSubmit, got {other:?}"),
        }
    }
}

#[test]
fn a_slash_prompt_while_streaming_is_steered_as_is() {
    let mut app = app_with_commands(&["help", "resume"]);
    app.start_streaming();
    app.input.text = "/etc/hosts is where lookups start".into();
    app.input.cursor = app.input.text.len();

    let event = handle_key(&mut app, key(KeyCode::Enter));

    match event {
        Some(AppEvent::UserSubmit { text, .. }) => {
            assert_eq!(text, "/etc/hosts is where lookups start");
        }
        other => panic!("expected UserSubmit, got {other:?}"),
    }
}

#[test]
fn a_known_skill_command_still_runs() {
    let mut app = app_with_commands(&["help", "skill:python"]);
    app.input.text = "/skill:python fix it".into();
    app.input.cursor = app.input.text.len();

    let event = handle_key(&mut app, key(KeyCode::Enter));

    match event {
        Some(AppEvent::SlashCommand {
            action: crate::slash::SlashAction::Other { name, args },
        }) => {
            assert_eq!(name, "skill:python");
            assert_eq!(args, "fix it");
        }
        other => panic!("expected SlashCommand, got {other:?}"),
    }
}

#[test]
fn enter_after_trailing_backslash_inserts_newline() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "hello\\".into();
    app.input.cursor = app.input.text.len();

    let event = handle_key(&mut app, key(KeyCode::Enter));

    assert!(event.is_none());
    assert_eq!(app.input.text, "hello\n");
    assert_eq!(app.input.cursor, app.input.text.len());
}

#[test]
fn enter_after_backslash_mid_input_does_not_submit() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "hello\\\nworld".into();
    app.input.cursor = "hello\\".len();

    let event = handle_key(&mut app, key(KeyCode::Enter));

    assert!(event.is_none());
    assert_eq!(app.input.text, "hello\n\nworld");
    assert_eq!(app.input.cursor, "hello\n".len());
}

#[test]
fn ctrl_w_deletes_word_backward() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "hello world".into();
    app.input.cursor = 11;
    handle_key(&mut app, ctrl(KeyCode::Char('w')));
    assert_eq!(app.input.text, "hello ");
}

#[test]
fn configured_word_delete_binding_is_used() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let raw = crate::keybinds::scoped_test_config(
        crate::keybinds::KeyScope::Edit,
        [(
            "delete_word_backward",
            crate::keybinds::BindingSource::One("alt+q".into()),
        )],
    );
    app.keymap.override_scoped(&raw);
    app.input.text = "hello world".into();
    app.input.cursor = 11;

    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT),
    );

    assert_eq!(app.input.text, "hello ");
}

#[test]
fn ctrl_k_deletes_to_end() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "hello world".into();
    app.input.cursor = 5;
    handle_key(&mut app, ctrl(KeyCode::Char('k')));
    assert_eq!(app.input.text, "hello");
}

#[test]
fn ctrl_d_with_input_deletes_char() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "abc".into();
    app.input.cursor = 1;
    handle_key(&mut app, ctrl(KeyCode::Char('d')));
    assert_eq!(app.input.text, "ac");
}

#[test]
fn configured_delete_char_binding_is_used() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let raw = crate::keybinds::scoped_test_config(
        crate::keybinds::KeyScope::Edit,
        [(
            "delete_char",
            crate::keybinds::BindingSource::One("alt+x".into()),
        )],
    );
    app.keymap.override_scoped(&raw);
    app.input.text = "abc".into();
    app.input.cursor = 1;

    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
    );

    assert_eq!(app.input.text, "ac");
}

#[test]
fn configured_line_delete_bindings_are_used() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let raw = crate::keybinds::scoped_test_config(
        crate::keybinds::KeyScope::Edit,
        [
            (
                "delete_to_start",
                crate::keybinds::BindingSource::One("alt+u".into()),
            ),
            (
                "delete_to_end",
                crate::keybinds::BindingSource::One("alt+k".into()),
            ),
        ],
    );
    app.keymap.override_scoped(&raw);
    app.input.text = "hello world".into();
    app.input.cursor = 5;

    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::ALT),
    );
    assert_eq!(app.input.text, " world");

    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT),
    );
    assert_eq!(app.input.text, "");
}

#[test]
fn configured_cursor_bindings_are_used() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let raw = crate::keybinds::scoped_test_config(
        crate::keybinds::KeyScope::Edit,
        [
            (
                "cursor_line_start",
                crate::keybinds::BindingSource::One("alt+a".into()),
            ),
            (
                "cursor_line_end",
                crate::keybinds::BindingSource::One("alt+e".into()),
            ),
        ],
    );
    app.keymap.override_scoped(&raw);
    app.input.text = "hello world".into();
    app.input.cursor = 5;

    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT),
    );
    assert_eq!(app.input.cursor, 11);
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT),
    );
    assert_eq!(app.input.cursor, 0);
}

#[test]
fn configured_insert_newline_binding_is_used() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let raw = crate::keybinds::scoped_test_config(
        crate::keybinds::KeyScope::Insert,
        [(
            "insert_newline",
            crate::keybinds::BindingSource::One("alt+n".into()),
        )],
    );
    app.keymap.override_scoped(&raw);
    app.input.text = "a".into();
    app.input.cursor = 1;

    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT),
    );

    assert_eq!(app.input.text, "a\n");
}

#[test]
fn configured_global_bindings_are_used() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let raw = crate::keybinds::scoped_test_config(
        crate::keybinds::KeyScope::Shared,
        [(
            "quit_or_abort",
            crate::keybinds::BindingSource::One("alt+q".into()),
        )],
    );
    app.keymap.override_scoped(&raw);

    let event = handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT),
    );

    assert!(event.is_none());
    assert!(app.quit_hint_armed());
}

#[test]
fn typing_allowed_while_streaming() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    handle_key(&mut app, key(KeyCode::Char('h')));
    handle_key(&mut app, key(KeyCode::Char('i')));
    assert_eq!(app.input.text, "hi");
}

#[test]
fn submit_allowed_while_streaming() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    app.input.text = "steer this".into();
    app.input.cursor = 10;
    let event = handle_key(&mut app, key(KeyCode::Enter));
    match event {
        Some(AppEvent::UserSubmit { text, images }) => {
            assert_eq!(text, "steer this");
            assert!(images.is_empty());
        }
        other => panic!("expected UserSubmit, got {other:?}"),
    }
}

#[test]
fn slash_commands_blocked_while_streaming() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    app.input.text = "/compact".into();
    app.input.cursor = app.input.text.len();
    let event = handle_key(&mut app, key(KeyCode::Enter));
    assert!(event.is_none(), "expected None, got {event:?}");
    assert_eq!(app.input.text, "/compact");
    assert!(
        app.status
            .as_ref()
            .is_some_and(|s| s.text.contains("blocked while streaming")),
        "status was {:?}",
        app.status
    );
}

#[test]
fn safe_slash_commands_run_during_streaming() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    app.input.text = "/help".into();
    app.input.cursor = 5;
    let event = handle_key(&mut app, key(KeyCode::Enter));
    match event {
        Some(AppEvent::SlashCommand { action }) => {
            assert_eq!(action, crate::slash::SlashAction::Help);
        }
        other => panic!("expected SlashCommand, got {other:?}"),
    }
}

#[test]
fn tab_completes_slash_commands() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.completions = vec!["/help".into(), "/history".into(), "/clear".into()];
    app.input.text = "/h".into();
    app.input.cursor = 2;
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.input.text, "/help");

    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.input.text, "/history");

    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.input.text, "/help");
}

#[test]
fn tab_completes_model_ids() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.completions = vec![
        "/model".into(),
        "claude-opus-4-7".into(),
        "claude-sonnet-4-20250514".into(),
    ];
    app.input.text = "/model claude-o".into();
    app.input.cursor = 15;
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.input.text, "/model claude-opus-4-7");
}

#[test]
fn tab_no_match_does_nothing() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.completions = vec!["/help".into()];
    app.input.text = "/zzz".into();
    app.input.cursor = 4;
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.input.text, "/zzz");
}

#[test]
fn tab_resets_on_typing() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.completions = vec!["/help".into(), "/history".into()];
    app.input.text = "/h".into();
    app.input.cursor = 2;
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.input.text, "/help");

    handle_key(&mut app, key(KeyCode::Char('x')));
    assert_eq!(app.input.text, "/helpx");
}

fn push_test_template(app: &mut App, name: &str, content: &str) {
    app.completion.templates.push(mush_ext::PromptTemplate {
        name: name.into(),
        description: None,
        content: content.into(),
        source: mush_ext::TemplateSource::User,
        path: std::path::PathBuf::from("/tmp/test.md"),
    });
}

#[test]
fn at_template_tab_expands_exact_match() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "please review the changes");
    app.input.text = "hi @review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.input.text, "hi please review the changes");
}

#[test]
fn at_template_enter_sends_literal_without_expansion() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "asdf", "TEMPLATE CONTENT");
    app.input.text = "@asdf".into();
    app.input.cursor = app.input.text.len();
    let event = handle_key(&mut app, key(KeyCode::Enter));
    match event {
        Some(AppEvent::UserSubmit { text, images }) => {
            assert_eq!(text, "@asdf", "enter must send the raw @word");
            assert!(images.is_empty());
        }
        other => panic!("expected UserSubmit, got {other:?}"),
    }
}

#[test]
fn at_template_tab_falls_through_when_no_match() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "irrelevant");
    app.input.text = "@missing".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.input.text, "@missing");
}

#[test]
fn at_template_tab_opens_picker_when_multiple_prefix_matches() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "review body");
    push_test_template(&mut app, "review-pr", "review-pr body");
    push_test_template(&mut app, "plan", "plan body");
    app.input.text = "@rev".into();
    app.input.cursor = app.input.text.len();

    handle_key(&mut app, key(KeyCode::Tab));

    assert_eq!(app.interaction.mode, AppMode::AtPicker);
    let picker = app
        .interaction
        .at_picker
        .as_ref()
        .expect("picker state should be populated");
    let names: Vec<&str> = picker.matches.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["review", "review-pr"]);
    assert_eq!(picker.selected, 0);
    assert_eq!(app.input.text, "@rev", "input untouched until enter");
}

#[test]
fn at_picker_tab_cycles_selection() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "a");
    push_test_template(&mut app, "review-pr", "b");
    app.input.text = "@rev".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.interaction.at_picker.as_ref().unwrap().selected, 1);

    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.interaction.at_picker.as_ref().unwrap().selected, 0);
}

#[test]
fn at_picker_shift_tab_cycles_backwards() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "a");
    push_test_template(&mut app, "review-pr", "b");
    app.input.text = "@rev".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
    );
    assert_eq!(app.interaction.at_picker.as_ref().unwrap().selected, 1);
}

#[test]
fn at_picker_enter_inserts_selected_template() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "REVIEW");
    push_test_template(&mut app, "review-pr", "REVIEW PR");
    app.input.text = "hi @rev".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));
    handle_key(&mut app, key(KeyCode::Tab));

    let event = handle_key(&mut app, key(KeyCode::Enter));

    assert!(
        event.is_none(),
        "enter inside the picker must not submit, got {event:?}"
    );
    assert_eq!(app.input.text, "hi REVIEW PR");
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.interaction.at_picker.is_none());
}

#[test]
fn at_picker_esc_closes_without_touching_input() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "REVIEW");
    push_test_template(&mut app, "review-pr", "REVIEW PR");
    app.input.text = "@rev".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.interaction.mode, AppMode::AtPicker);

    handle_key(&mut app, key(KeyCode::Esc));

    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.interaction.at_picker.is_none());
    assert_eq!(app.input.text, "@rev", "esc preserves the trigger text");
}

#[test]
fn at_picker_typing_narrows_matches() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "REVIEW");
    push_test_template(&mut app, "review-pr", "REVIEW PR");
    push_test_template(&mut app, "release", "RELEASE");
    app.input.text = "@re".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    handle_key(&mut app, key(KeyCode::Char('v')));

    assert_eq!(app.input.text, "@rev");
    assert_eq!(app.interaction.mode, AppMode::AtPicker);
    let names: Vec<&str> = app
        .interaction
        .at_picker
        .as_ref()
        .unwrap()
        .matches
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(names, vec!["review", "review-pr"]);
}

#[test]
fn at_picker_typing_past_all_matches_closes_and_keeps_text() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "REVIEW");
    app.input.text = "@re".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    handle_key(&mut app, key(KeyCode::Char('z')));

    assert_eq!(app.input.text, "@rez");
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.interaction.at_picker.is_none());
}

#[test]
fn at_picker_backspace_past_at_sign_closes() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "REVIEW");
    push_test_template(&mut app, "review-pr", "REVIEW PR");
    app.input.text = "@r".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));
    handle_key(&mut app, key(KeyCode::Backspace));
    handle_key(&mut app, key(KeyCode::Backspace));

    assert_eq!(app.input.text, "");
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.interaction.at_picker.is_none());
}

#[test]
fn template_with_placeholders_enters_slot_edit_mode() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "review $1 with $2");
    app.input.text = "@review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    assert_eq!(app.input.text, "review  with ");
    assert_eq!(app.interaction.mode, AppMode::SlotEdit);
    let slot_state = app
        .interaction
        .slot_edit
        .as_ref()
        .expect("slot state should be populated");
    assert_eq!(slot_state.slots, vec![7, 13]);
    assert_eq!(slot_state.current, 0);
    assert_eq!(app.input.cursor, 7);
}

#[test]
fn template_without_placeholders_stays_in_normal_mode() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "plain content");
    app.input.text = "@review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    assert_eq!(app.input.text, "plain content");
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.interaction.slot_edit.is_none());
}

#[test]
fn slot_edit_typing_shifts_later_slot_offsets() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "$1 then $2");
    app.input.text = "@review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    for c in "abc".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    assert_eq!(app.input.text, "abc then ");
    let state = app.interaction.slot_edit.as_ref().unwrap();
    assert_eq!(state.slots, vec![0, 9]);
}

#[test]
fn slot_edit_tab_jumps_to_next_slot() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "$1 then $2");
    app.input.text = "@review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    for c in "abc".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    handle_key(&mut app, key(KeyCode::Tab));

    let state = app.interaction.slot_edit.as_ref().unwrap();
    assert_eq!(state.current, 1);
    assert_eq!(app.input.cursor, 9);
}

#[test]
fn slot_edit_enter_exits_without_submitting() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "fix $1");
    app.input.text = "@review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));
    for c in "bug".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }

    let event = handle_key(&mut app, key(KeyCode::Enter));

    assert!(event.is_none(), "first enter must not submit");
    assert_eq!(app.input.text, "fix bug");
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.interaction.slot_edit.is_none());
}

#[test]
fn slot_edit_esc_keeps_typed_content_and_exits_mode() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "fix $1");
    app.input.text = "@review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));
    handle_key(&mut app, key(KeyCode::Char('x')));

    handle_key(&mut app, key(KeyCode::Esc));

    assert_eq!(app.input.text, "fix x");
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.interaction.slot_edit.is_none());
}

#[test]
fn slot_edit_shift_tab_cycles_backwards_with_wrap() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "$1 / $2 / $3");
    app.input.text = "@review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
    );
    assert_eq!(app.interaction.slot_edit.as_ref().unwrap().current, 2);
}

#[test]
fn slot_edit_tab_from_last_slot_wraps_to_first() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "$1 / $2");
    app.input.text = "@review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    handle_key(&mut app, key(KeyCode::Tab));
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.interaction.slot_edit.as_ref().unwrap().current, 0);
}

#[test]
fn slot_edit_backspace_at_slot_start_is_a_noop() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "fix $1 now");
    app.input.text = "@review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));
    let before_text = app.input.text.clone();

    handle_key(&mut app, key(KeyCode::Backspace));

    assert_eq!(
        app.input.text, before_text,
        "backspace must not eat the leading verbatim chars"
    );
}

#[test]
fn tab_ignored_for_non_slash_input() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.completions = vec!["/help".into()];
    app.input.text = "hello".into();
    app.input.cursor = 5;
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.input.text, "hello"); // unchanged
}

#[test]
fn slash_menu_opens_on_tab_with_commands() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.slash_commands = vec![
        crate::app::SlashCommand {
            name: "help".into(),
            description: "show help".into(),
        },
        crate::app::SlashCommand {
            name: "clear".into(),
            description: "clear chat".into(),
        },
    ];
    app.input.text = "/".into();
    app.input.cursor = 1;
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.interaction.mode, AppMode::SlashComplete);
    assert!(app.completion.slash_menu.is_some());
    let menu = app.completion.slash_menu.as_ref().unwrap();
    assert_eq!(menu.matches.len(), 2);
    assert_eq!(menu.selected, 0);
}

#[test]
fn the_effort_menu_lists_levels_and_picking_one_runs_it() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.slash_commands = vec![crate::app::SlashCommand {
        name: "effort".into(),
        description: "select reasoning/thinking level ([level])".into(),
    }];
    app.supported_thinking_levels = vec![ThinkingLevel::Medium, ThinkingLevel::High];
    app.thinking_level = ThinkingLevel::High;
    app.input.text = "/effort ".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    let menu = app.completion.slash_menu.as_ref().unwrap();
    let rows: Vec<(&str, &str)> = menu
        .matches
        .iter()
        .map(|m| (m.name.as_str(), m.description.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            ("effort off", "No reasoning"),
            ("effort medium", "Moderate reasoning"),
            ("effort high", "Deep reasoning (current)"),
        ]
    );

    handle_key(&mut app, key(KeyCode::Char('m')));
    assert_eq!(app.completion.slash_menu.as_ref().unwrap().matches.len(), 1);
    let event = handle_key(&mut app, key(KeyCode::Enter));
    match event {
        Some(AppEvent::SlashCommand {
            action: crate::slash::SlashAction::Effort { level },
        }) => assert_eq!(level.as_deref(), Some("medium")),
        other => panic!("expected an effort command, got {other:?}"),
    }
    assert!(app.input.text.is_empty());
}

#[test]
fn slash_menu_navigate_and_select() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.slash_commands = vec![
        crate::app::SlashCommand {
            name: "help".into(),
            description: "show help".into(),
        },
        crate::app::SlashCommand {
            name: "clear".into(),
            description: "clear chat".into(),
        },
    ];
    app.input.text = "/".into();
    app.input.cursor = 1;
    handle_key(&mut app, key(KeyCode::Tab));

    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    assert_eq!(app.completion.slash_menu.as_ref().unwrap().selected, 1);

    handle_key(&mut app, ctrl(KeyCode::Char('k')));
    assert_eq!(app.completion.slash_menu.as_ref().unwrap().selected, 0);

    handle_key(&mut app, key(KeyCode::Enter));
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert_eq!(app.input.text, "/help");
}

#[test]
fn slash_menu_tab_cycles_selection_with_wrap() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.slash_commands = vec![
        crate::app::SlashCommand {
            name: "login".into(),
            description: "oauth login".into(),
        },
        crate::app::SlashCommand {
            name: "login-complete".into(),
            description: "finish oauth".into(),
        },
    ];
    app.input.text = "/login".into();
    app.input.cursor = app.input.text.len();

    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.interaction.mode, AppMode::SlashComplete);
    assert_eq!(app.completion.slash_menu.as_ref().unwrap().selected, 0);

    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.completion.slash_menu.as_ref().unwrap().selected, 1);

    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.completion.slash_menu.as_ref().unwrap().selected, 0);

    let shift_backtab = KeyEvent {
        code: KeyCode::BackTab,
        modifiers: KeyModifiers::SHIFT,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    };
    handle_key(&mut app, shift_backtab);
    assert_eq!(app.completion.slash_menu.as_ref().unwrap().selected, 1);

    assert!(app.completion.slash_menu.is_some());
}

#[test]
fn slash_menu_enter_accepts_and_closes() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.slash_commands = vec![
        crate::app::SlashCommand {
            name: "login".into(),
            description: "oauth login".into(),
        },
        crate::app::SlashCommand {
            name: "login-complete".into(),
            description: "finish oauth".into(),
        },
    ];
    app.input.text = "/login".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));
    handle_key(&mut app, key(KeyCode::Tab));

    handle_key(&mut app, key(KeyCode::Enter));
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.completion.slash_menu.is_none());
    assert_eq!(app.input.text, "/login-complete");
}

fn ctrl_shift(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

#[test]
fn alt_m_cycles_to_next_favourite_model() {
    let mut app = App::new("a".into(), TokenCount::new(200_000));
    app.completion.favourite_models = vec!["a".into(), "b".into(), "c".into()];

    let event = handle_key(&mut app, alt(KeyCode::Char('m')));
    assert!(
        matches!(event, Some(AppEvent::ModelSelected { ref model_id }) if model_id == "b"),
        "alt+m from 'a' should pick 'b', got {event:?}"
    );

    app.model_id = "c".into();
    let event = handle_key(&mut app, alt(KeyCode::Char('m')));
    assert!(
        matches!(event, Some(AppEvent::ModelSelected { ref model_id }) if model_id == "a"),
        "alt+m from end should wrap to 'a', got {event:?}"
    );
}

#[test]
fn alt_shift_m_cycles_backward() {
    let mut app = App::new("b".into(), TokenCount::new(200_000));
    app.completion.favourite_models = vec!["a".into(), "b".into(), "c".into()];

    let shift_alt = KeyEvent {
        code: KeyCode::Char('M'),
        modifiers: KeyModifiers::ALT | KeyModifiers::SHIFT,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    };
    let event = handle_key(&mut app, shift_alt);
    assert!(
        matches!(event, Some(AppEvent::ModelSelected { ref model_id }) if model_id == "a"),
        "alt+shift+m from 'b' should pick 'a', got {event:?}"
    );

    app.model_id = "a".into();
    let event = handle_key(&mut app, shift_alt);
    assert!(
        matches!(event, Some(AppEvent::ModelSelected { ref model_id }) if model_id == "c"),
        "alt+shift+m from start should wrap to 'c', got {event:?}"
    );
}

#[test]
fn alt_m_with_no_favourites_nudges_via_status() {
    let mut app = App::new("a".into(), TokenCount::new(200_000));
    assert!(app.completion.favourite_models.is_empty());

    let event = handle_key(&mut app, alt(KeyCode::Char('m')));
    assert!(event.is_none());
    assert!(
        app.status
            .as_ref()
            .is_some_and(|s| s.text.contains("favourite")),
        "expected a status nudge about favourites, got {:?}",
        app.status
    );
}

#[test]
fn slash_menu_typing_filters() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.slash_commands = vec![
        crate::app::SlashCommand {
            name: "help".into(),
            description: "show help".into(),
        },
        crate::app::SlashCommand {
            name: "history".into(),
            description: "show history".into(),
        },
        crate::app::SlashCommand {
            name: "clear".into(),
            description: "clear chat".into(),
        },
    ];
    app.input.text = "/h".into();
    app.input.cursor = 2;
    handle_key(&mut app, key(KeyCode::Tab));

    let menu = app.completion.slash_menu.as_ref().unwrap();
    assert_eq!(menu.matches.len(), 2);

    handle_key(&mut app, key(KeyCode::Char('e')));
    let menu = app.completion.slash_menu.as_ref().unwrap();
    assert_eq!(menu.matches.len(), 1);
    assert_eq!(menu.matches[0].name, "help");
}

#[test]
fn input_reinsert_while_streaming_keeps_cursor_visible() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    app.input.area.set(ratatui::layout::Rect::new(0, 0, 20, 12));
    app.input.visible_lines.set(2);
    app.input.total_lines.set(8);
    app.input.scroll.set(0);
    app.input.text = "/new a\nb\nc\nd".into();
    app.input.cursor = app.input.text.len();

    handle_key(&mut app, key(KeyCode::Enter));

    assert!(app.input.scroll.get() > 0);
}

#[test]
fn slash_menu_esc_closes() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.slash_commands = vec![crate::app::SlashCommand {
        name: "help".into(),
        description: "show help".into(),
    }];
    app.input.text = "/".into();
    app.input.cursor = 1;
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.interaction.mode, AppMode::SlashComplete);

    handle_key(&mut app, key(KeyCode::Esc));
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.completion.slash_menu.is_none());
}

fn alt(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::ALT,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

#[test]
fn ctrl_bracket_closes_slash_menu() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.slash_commands = vec![crate::app::SlashCommand {
        name: "help".into(),
        description: "show help".into(),
    }];
    app.input.text = "/".into();
    app.input.cursor = 1;
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.interaction.mode, AppMode::SlashComplete);

    handle_key(&mut app, ctrl(KeyCode::Char('[')));
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.completion.slash_menu.is_none());
}

#[test]
fn ctrl_bracket_exits_search_mode() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("one");
    app.interaction.mode = AppMode::Search;

    handle_key(&mut app, ctrl(KeyCode::Char('[')));
    assert_eq!(app.interaction.mode, AppMode::Insert);
}

#[test]
fn ctrl_bracket_closes_session_picker() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_session_picker(test_sessions(), "/tmp".into());
    assert_eq!(app.interaction.mode, AppMode::SessionPicker);

    handle_key(&mut app, ctrl(KeyCode::Char('[')));
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.interaction.session_picker.is_none());
}

fn message_items() -> Vec<crate::message_picker::MessagePickerItem> {
    ["fix the build", "add tests", "ship it"]
        .into_iter()
        .enumerate()
        .map(|(i, text)| crate::message_picker::MessagePickerItem::numbered(i + 1, text))
        .collect()
}

#[test]
fn message_picker_enter_forks_before_the_highlighted_message() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_message_picker(
        crate::message_picker::MessagePickerPurpose::Fork,
        message_items(),
    );
    assert_eq!(app.interaction.mode, AppMode::MessagePicker);

    handle_key(&mut app, key(KeyCode::Up));
    let event = handle_key(&mut app, key(KeyCode::Enter));

    assert!(matches!(
        event,
        Some(AppEvent::SlashCommand {
            action: crate::slash::SlashAction::Fork { index: Some(2) }
        })
    ));
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.interaction.message_picker.is_none());
}

#[test]
fn message_picker_esc_clears_the_filter_before_closing() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_message_picker(
        crate::message_picker::MessagePickerPurpose::Tree,
        message_items(),
    );
    for c in "ship".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }

    handle_key(&mut app, key(KeyCode::Esc));

    let picker = app.interaction.message_picker.as_ref().expect("still open");
    assert!(picker.filter.is_empty());
    assert_eq!(app.interaction.mode, AppMode::MessagePicker);

    handle_key(&mut app, key(KeyCode::Esc));

    assert!(app.interaction.message_picker.is_none());
    assert_eq!(app.interaction.mode, AppMode::Insert);
}

#[test]
fn message_picker_filter_then_enter_branches_before_the_match() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_message_picker(
        crate::message_picker::MessagePickerPurpose::Branch,
        message_items(),
    );

    for c in "build".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    let event = handle_key(&mut app, key(KeyCode::Enter));

    assert!(matches!(
        event,
        Some(AppEvent::SlashCommand {
            action: crate::slash::SlashAction::Branch { index: Some(1) }
        })
    ));
}

#[test]
fn tree_picker_enter_navigates_to_the_highlighted_entry() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let entry = mush_session::tree::EntryId::new();
    let mut item = crate::message_picker::MessagePickerItem::numbered(1, "q0");
    item.entry_id = Some(entry.clone());
    app.open_message_picker(
        crate::message_picker::MessagePickerPurpose::Tree,
        vec![item],
    );

    let event = handle_key(&mut app, key(KeyCode::Enter));

    assert!(matches!(
        event,
        Some(AppEvent::SlashCommand {
            action: crate::slash::SlashAction::NavigateTree { entry_id }
        }) if entry_id == entry
    ));
}

#[test]
fn esc_closes_the_message_picker_without_forking() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_message_picker(
        crate::message_picker::MessagePickerPurpose::Fork,
        message_items(),
    );

    let event = handle_key(&mut app, key(KeyCode::Esc));

    assert!(event.is_none());
    assert_eq!(app.interaction.mode, AppMode::Insert);
}

#[test]
fn session_picker_plain_j_k_filter_instead_of_navigate() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_session_picker(test_sessions(), "/tmp".into());

    handle_key(&mut app, key(KeyCode::Char('j')));
    handle_key(&mut app, key(KeyCode::Char('k')));

    let picker = app.interaction.session_picker.as_ref().unwrap();
    assert_eq!(picker.selected, 0);
    assert_eq!(picker.filter, "jk");
}

#[test]
fn session_picker_down_up_and_ctrl_j_k_navigate_visible_rows() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_session_picker(test_sessions(), "/tmp".into());

    handle_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.interaction.session_picker.as_ref().unwrap().selected, 1);

    handle_key(&mut app, key(KeyCode::Up));
    assert_eq!(app.interaction.session_picker.as_ref().unwrap().selected, 0);

    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    assert_eq!(app.interaction.session_picker.as_ref().unwrap().selected, 1);

    handle_key(&mut app, ctrl(KeyCode::Char('k')));
    assert_eq!(app.interaction.session_picker.as_ref().unwrap().selected, 0);
}

#[test]
fn session_picker_tab_toggles_scope() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_session_picker(test_sessions(), "/tmp".into());
    assert_eq!(
        app.interaction.session_picker.as_ref().unwrap().scope,
        SessionScope::ThisDir
    );

    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(
        app.interaction.session_picker.as_ref().unwrap().scope,
        SessionScope::AllDirs
    );
}

fn test_sessions() -> Vec<mush_session::SessionMeta> {
    use mush_ai::types::{ModelId, SessionId, Timestamp};
    vec![
        mush_session::SessionMeta {
            paused: None,
            rlm_max_depth: None,
            id: SessionId::from("one"),
            title: Some("hello".into()),
            model_id: ModelId::from("m"),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 1,
            cwd: "/tmp".into(),
            parent: None,
            subagent_model: None,
            accounts: Default::default(),
            lifecycle: mush_session::Lifecycle::Live,
        },
        mush_session::SessionMeta {
            paused: None,
            rlm_max_depth: None,
            id: SessionId::from("two"),
            title: Some("jkl session".into()),
            model_id: ModelId::from("m"),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 1,
            cwd: "/tmp".into(),
            parent: None,
            subagent_model: None,
            accounts: Default::default(),
            lifecycle: mush_session::Lifecycle::Live,
        },
    ]
}

fn app_with_models() -> App {
    use crate::slash_menu::ModelCompletion;
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.model_completions = vec![
        ModelCompletion {
            id: "claude-opus-4".into(),
            name: "Claude Opus 4".into(),
            provider: "anthropic".into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: None,
        },
        ModelCompletion {
            id: "claude-haiku-4".into(),
            name: "Claude Haiku 4".into(),
            provider: "anthropic".into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: None,
        },
        ModelCompletion {
            id: "gpt-5".into(),
            name: "GPT 5".into(),
            provider: "openai".into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: None,
        },
    ];
    app
}

#[test]
fn model_picker_overlay_esc_closes() {
    let mut app = app_with_models();
    app.open_model_picker_overlay();
    assert_eq!(app.interaction.mode, AppMode::ModelPicker);

    handle_key(&mut app, key(KeyCode::Esc));
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.interaction.model_picker.is_none());
}

#[test]
fn model_picker_overlay_ctrl_j_k_navigate_visible_rows() {
    let mut app = app_with_models();
    app.open_model_picker_overlay();
    assert_eq!(
        app.interaction.model_picker.as_ref().unwrap().selected,
        0,
        "starts at row 0"
    );

    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    assert_eq!(app.interaction.model_picker.as_ref().unwrap().selected, 1);
    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    assert_eq!(app.interaction.model_picker.as_ref().unwrap().selected, 2);
    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    assert_eq!(app.interaction.model_picker.as_ref().unwrap().selected, 2);
    handle_key(&mut app, ctrl(KeyCode::Char('k')));
    assert_eq!(app.interaction.model_picker.as_ref().unwrap().selected, 1);
}

#[test]
fn model_picker_overlay_plain_j_k_filter_instead_of_navigate() {
    let mut app = app_with_models();
    app.open_model_picker_overlay();

    handle_key(&mut app, key(KeyCode::Char('j')));
    handle_key(&mut app, key(KeyCode::Char('k')));

    let picker = app.interaction.model_picker.as_ref().unwrap();
    assert_eq!(picker.selected, 0);
    assert_eq!(picker.filter, "jk");
}

#[test]
fn model_picker_overlay_ctrl_n_p_do_not_navigate() {
    let mut app = app_with_models();
    app.open_model_picker_overlay();

    handle_key(&mut app, ctrl(KeyCode::Char('n')));
    handle_key(&mut app, ctrl(KeyCode::Char('p')));

    assert_eq!(app.interaction.model_picker.as_ref().unwrap().selected, 0);
}

#[test]
fn model_picker_overlay_tab_switches_tabs() {
    let mut app = app_with_models();
    app.completion.favourite_models = vec!["gpt-5".into()];
    app.open_model_picker_overlay();
    assert_eq!(
        app.interaction.model_picker.as_ref().unwrap().tab,
        crate::model_picker::ModelPickerTab::Favourites
    );

    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(
        app.interaction.model_picker.as_ref().unwrap().tab,
        crate::model_picker::ModelPickerTab::All
    );
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(
        app.interaction.model_picker.as_ref().unwrap().tab,
        crate::model_picker::ModelPickerTab::Favourites
    );
}

#[test]
fn model_picker_overlay_enter_emits_model_action_and_closes() {
    let mut app = app_with_models();
    app.open_model_picker_overlay();
    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    let event = handle_key(&mut app, key(KeyCode::Enter));
    match event {
        Some(AppEvent::SlashCommand {
            action:
                crate::slash::SlashAction::Model {
                    model_id: Some(id),
                    show_all: false,
                },
        }) => assert_eq!(id.as_str(), "claude-haiku-4"),
        other => panic!("expected SlashAction::Model, got {other:?}"),
    }
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.interaction.model_picker.is_none());
}

#[test]
fn model_picker_overlay_typing_filters_visible_rows() {
    let mut app = app_with_models();
    app.open_model_picker_overlay();
    for c in "openai".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    let picker = app.interaction.model_picker.as_ref().unwrap();
    let visible = crate::model_picker::filtered_models(picker);
    let ids: Vec<&str> = visible.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["gpt-5"]);
}

#[test]
fn reload_model_completions_refreshes_open_picker_models() {
    use crate::slash_menu::ModelCompletion;

    let mut app = app_with_models();
    app.open_model_picker_overlay();
    app.completion.model_completions.push(ModelCompletion {
        id: "stale-only".into(),
        name: "Stale Only".into(),
        provider: "openai".into(),
        stale: false,
        description: None,
        speed_tiers: Vec::new(),
        priority: 0,
        visibility: None,
    });

    reload_model_completions(&mut app);

    let picker = app.interaction.model_picker.as_ref().unwrap();
    assert!(
        picker
            .models
            .iter()
            .any(|m| m.id == "gpt-5.5" || m.id == "claude-opus-4-7")
    );
    assert!(!picker.models.iter().any(|m| m.id == "stale-only"));
}

#[test]
fn model_picker_overlay_ctrl_f_toggles_favourite_when_unlocked() {
    let mut app = app_with_models();
    app.open_model_picker_overlay();
    let event = handle_key(&mut app, ctrl(KeyCode::Char('f')));
    assert!(matches!(event, Some(AppEvent::PersistFavourites)));
    assert_eq!(app.completion.favourite_models, vec!["claude-opus-4"]);

    handle_key(&mut app, ctrl(KeyCode::Char('f')));
    assert!(app.completion.favourite_models.is_empty());
}

#[test]
fn model_picker_overlay_ctrl_f_toasts_when_locked() {
    let mut app = app_with_models();
    app.completion.favourites_locked = true;
    app.open_model_picker_overlay();
    let event = handle_key(&mut app, ctrl(KeyCode::Char('f')));
    assert!(event.is_none(), "no PersistFavourites when locked");
    assert!(app.completion.favourite_models.is_empty());
    assert!(
        app.interaction
            .model_picker
            .as_ref()
            .unwrap()
            .toast
            .as_deref()
            .unwrap_or("")
            .contains("locked"),
        "should toast about lock"
    );
}

fn app_with_one_stale_row() -> App {
    use crate::slash_menu::ModelCompletion;
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.model_completions = vec![
        ModelCompletion {
            id: "alive".into(),
            name: "Alive".into(),
            provider: "anthropic".into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: None,
        },
        ModelCompletion {
            id: "removed".into(),
            name: "Removed".into(),
            provider: "anthropic".into(),
            stale: true,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: None,
        },
    ];
    app
}

#[test]
fn model_picker_overlay_ctrl_d_arms_confirm_only_for_stale_rows() {
    let mut app = app_with_one_stale_row();
    app.open_model_picker_overlay();

    handle_key(&mut app, ctrl(KeyCode::Char('d')));
    let picker = app.interaction.model_picker.as_ref().unwrap();
    assert!(picker.confirm_delete.is_none());
    assert!(picker.toast.as_deref().unwrap().contains("only stale"));

    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    handle_key(&mut app, ctrl(KeyCode::Char('d')));
    let picker = app.interaction.model_picker.as_ref().unwrap();
    assert!(matches!(
        picker.confirm_delete,
        Some(crate::slash_menu::DeleteConfirm::Single { .. })
    ));

    handle_key(&mut app, key(KeyCode::Char('n')));
    let picker = app.interaction.model_picker.as_ref().unwrap();
    assert!(picker.confirm_delete.is_none());
    assert!(picker.toast.is_none());
}

#[test]
fn model_picker_overlay_ctrl_shift_d_arms_delete_all_stale() {
    use crate::slash_menu::ModelCompletion;
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.completion.model_completions = vec![
        ModelCompletion {
            id: "stale-a".into(),
            name: "A".into(),
            provider: "anthropic".into(),
            stale: true,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: None,
        },
        ModelCompletion {
            id: "stale-b".into(),
            name: "B".into(),
            provider: "openrouter".into(),
            stale: true,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: None,
        },
    ];
    app.open_model_picker_overlay();

    handle_key(&mut app, ctrl_shift(KeyCode::Char('d')));
    let picker = app.interaction.model_picker.as_ref().unwrap();
    assert_eq!(
        picker.confirm_delete,
        Some(crate::slash_menu::DeleteConfirm::AllStale)
    );
    assert!(picker.toast.as_deref().unwrap().contains("delete all 2"));
}

#[test]
fn ctrl_bracket_does_not_abort_streaming() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;

    let event = handle_key(&mut app, ctrl(KeyCode::Char('[')));
    assert!(event.is_none(), "{event:?}");
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(!app.interaction.focus.is_log());
}

#[test]
fn up_arrow_lifts_queued_when_input_empty_streaming() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    app.push_queued_message("queued steering");

    let event = handle_key(&mut app, key(KeyCode::Up));
    assert!(
        matches!(event, Some(AppEvent::EditSteering)),
        "Up on empty input with queued msg should lift, got {event:?}"
    );
}

#[test]
fn ctrl_k_lifts_queued_when_input_empty_streaming() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    app.push_queued_message("queued steering");

    let event = handle_key(&mut app, ctrl(KeyCode::Char('k')));
    assert!(
        matches!(event, Some(AppEvent::EditSteering)),
        "Ctrl+K on empty input with queued msg should lift, got {event:?}"
    );
}

#[test]
fn up_arrow_lifts_queued_when_input_empty_idle() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = false;
    app.push_queued_message("queued steering");

    let event = handle_key(&mut app, key(KeyCode::Up));
    assert!(
        matches!(event, Some(AppEvent::EditSteering)),
        "Up in idle mode with queued msg should lift, got {event:?}"
    );
}

#[test]
fn ctrl_k_deletes_to_end_when_input_has_text() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "hello world".into();
    app.input.cursor = 5;
    app.push_queued_message("queued steering");

    let event = handle_key(&mut app, ctrl(KeyCode::Char('k')));
    assert!(
        event.is_none(),
        "Ctrl+K should kill-line (not lift) when input has text: {event:?}"
    );
    assert_eq!(
        app.input.text, "hello",
        "Ctrl+K should have killed from cursor to end"
    );
}

#[test]
fn up_arrow_without_queued_does_not_emit_edit_steering() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;

    let event = handle_key(&mut app, key(KeyCode::Up));
    assert!(
        !matches!(event, Some(AppEvent::EditSteering)),
        "Up without queued msg must not emit EditSteering: {event:?}"
    );
}

fn oauth_login_entry(
    id: &str,
    oauth_id: &str,
    name: &str,
    source: Option<mush_ai::login::LoginSource>,
) -> crate::login_picker::LoginEntry {
    crate::login_picker::LoginEntry {
        id: id.into(),
        name: name.into(),
        method: mush_ai::login::LoginMethod::OAuth {
            oauth_provider_id: oauth_id.into(),
        },
        source,
        account_id: None,
    }
}

fn api_key_login_entry(
    id: &str,
    name: &str,
    source: Option<mush_ai::login::LoginSource>,
) -> crate::login_picker::LoginEntry {
    crate::login_picker::LoginEntry {
        id: id.into(),
        name: name.into(),
        method: mush_ai::login::LoginMethod::ApiKey {
            storage_key: id.into(),
            env_var: format!("{}_API_KEY", id.to_uppercase().replace('-', "_")),
            config_key: id.into(),
        },
        source,
        account_id: None,
    }
}

fn app_with_login_picker() -> App {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_login_picker(
        vec![
            oauth_login_entry(
                "anthropic-pro-max",
                "anthropic",
                "Anthropic",
                Some(mush_ai::login::LoginSource::OAuth),
            ),
            oauth_login_entry("openai-codex", "openai-codex", "ChatGPT Codex", None),
        ],
        std::collections::HashMap::new(),
    );
    app
}

#[test]
fn login_picker_esc_closes() {
    let mut app = app_with_login_picker();
    assert_eq!(app.interaction.mode, AppMode::LoginPicker);
    handle_key(&mut app, key(KeyCode::Esc));
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.interaction.login_picker.is_none());
}

#[test]
fn login_picker_ctrl_j_k_navigate_visible_rows() {
    let mut app = app_with_login_picker();
    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    assert_eq!(app.interaction.login_picker.as_ref().unwrap().selected, 1);
    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    assert_eq!(app.interaction.login_picker.as_ref().unwrap().selected, 1);
    handle_key(&mut app, ctrl(KeyCode::Char('k')));
    assert_eq!(app.interaction.login_picker.as_ref().unwrap().selected, 0);
}

#[test]
fn login_picker_typing_filters_and_resets_selection() {
    let mut app = app_with_login_picker();
    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    assert_eq!(app.interaction.login_picker.as_ref().unwrap().selected, 1);

    for c in "codex".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    let picker = app.interaction.login_picker.as_ref().unwrap();
    assert_eq!(picker.filter, "codex");
    assert_eq!(picker.selected, 0);
    assert_eq!(
        crate::login_picker::filtered_entries(picker)
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        vec!["openai-codex"]
    );
}

#[test]
fn login_picker_enter_on_logged_out_oauth_emits_login_action_and_closes() {
    let mut app = app_with_login_picker();
    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    let event = handle_key(&mut app, key(KeyCode::Enter));
    match event {
        Some(AppEvent::SlashCommand {
            action:
                crate::slash::SlashAction::Login {
                    provider: Some(id),
                    account: None,
                },
        }) => assert_eq!(id, "openai-codex"),
        other => panic!("expected SlashAction::Login, got {other:?}"),
    }
    assert_eq!(app.interaction.mode, AppMode::Insert);
    assert!(app.interaction.login_picker.is_none());
}

#[test]
fn login_picker_enter_on_logged_out_api_key_arms_entry_prompt() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_login_picker(
        vec![api_key_login_entry("openrouter", "OpenRouter", None)],
        std::collections::HashMap::new(),
    );
    let event = handle_key(&mut app, key(KeyCode::Enter));
    assert!(event.is_none(), "api-key enter must not emit a slash event");
    let picker = app.interaction.login_picker.as_ref().unwrap();
    let prompt = picker.entry.as_ref().unwrap();
    assert_eq!(prompt.entry_id, "openrouter");
    assert!(prompt.buffer.is_empty());
    assert_eq!(app.interaction.mode, AppMode::LoginPicker);
}

#[test]
fn login_picker_entry_typing_pushes_into_buffer() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_login_picker(
        vec![api_key_login_entry("openrouter", "OpenRouter", None)],
        std::collections::HashMap::new(),
    );
    handle_key(&mut app, key(KeyCode::Enter));
    for c in "sk-or-test".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    let prompt = app
        .interaction
        .login_picker
        .as_ref()
        .unwrap()
        .entry
        .as_ref()
        .unwrap();
    assert_eq!(prompt.buffer, "sk-or-test");
}

#[test]
fn login_picker_entry_backspace_pops_char() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_login_picker(
        vec![api_key_login_entry("openrouter", "OpenRouter", None)],
        std::collections::HashMap::new(),
    );
    handle_key(&mut app, key(KeyCode::Enter));
    for c in "abc".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    handle_key(&mut app, key(KeyCode::Backspace));
    let prompt = app
        .interaction
        .login_picker
        .as_ref()
        .unwrap()
        .entry
        .as_ref()
        .unwrap();
    assert_eq!(prompt.buffer, "ab");
}

#[test]
fn login_picker_entry_esc_cancels_back_to_picker() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_login_picker(
        vec![api_key_login_entry("openrouter", "OpenRouter", None)],
        std::collections::HashMap::new(),
    );
    handle_key(&mut app, key(KeyCode::Enter));
    assert!(
        app.interaction
            .login_picker
            .as_ref()
            .unwrap()
            .entry
            .is_some()
    );
    handle_key(&mut app, key(KeyCode::Esc));
    let picker = app.interaction.login_picker.as_ref().unwrap();
    assert!(
        picker.entry.is_none(),
        "esc in entry mode must cancel the prompt, not close the picker"
    );
    assert_eq!(app.interaction.mode, AppMode::LoginPicker);
}

#[test]
fn login_picker_entry_empty_save_shows_error_toast_and_keeps_open() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_login_picker(
        vec![api_key_login_entry("openrouter", "OpenRouter", None)],
        std::collections::HashMap::new(),
    );
    handle_key(&mut app, key(KeyCode::Enter));
    handle_key(&mut app, key(KeyCode::Enter));
    let picker = app.interaction.login_picker.as_ref().unwrap();
    assert!(
        picker
            .toast
            .as_deref()
            .unwrap_or_default()
            .to_lowercase()
            .contains("empty"),
        "expected empty-key toast, got: {:?}",
        picker.toast
    );
}
#[test]
fn login_picker_enter_on_logged_in_arms_logout_confirm() {
    let mut app = app_with_login_picker();
    let event = handle_key(&mut app, key(KeyCode::Enter));
    assert!(event.is_none(), "no event while confirm is pending");
    let picker = app.interaction.login_picker.as_ref().unwrap();
    assert_eq!(picker.confirm_logout.as_deref(), Some("anthropic-pro-max"));
    assert_eq!(app.interaction.mode, AppMode::LoginPicker);
}

#[test]
fn login_picker_n_cancels_logout_confirm() {
    let mut app = app_with_login_picker();
    handle_key(&mut app, key(KeyCode::Enter));
    assert!(
        app.interaction
            .login_picker
            .as_ref()
            .unwrap()
            .confirm_logout
            .is_some()
    );
    handle_key(&mut app, key(KeyCode::Char('n')));
    let picker = app.interaction.login_picker.as_ref().unwrap();
    assert!(picker.confirm_logout.is_none());
    assert!(picker.toast.is_none());
}

#[test]
fn login_picker_entry_enter_with_filled_buffer_emits_validate_event_and_begins_validating() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_login_picker(
        vec![api_key_login_entry("openrouter", "OpenRouter", None)],
        std::collections::HashMap::new(),
    );
    handle_key(&mut app, key(KeyCode::Enter));
    for c in "sk-or-test".chars() {
        handle_key(&mut app, key(KeyCode::Char(c)));
    }
    let event = handle_key(&mut app, key(KeyCode::Enter));
    match event {
        Some(AppEvent::ValidateAndSaveApiKey { entry_id, key, .. }) => {
            assert_eq!(entry_id, "openrouter");
            assert_eq!(key, "sk-or-test");
        }
        other => panic!("expected ValidateAndSaveApiKey, got {other:?}"),
    }
    let picker = app.interaction.login_picker.as_ref().unwrap();
    assert_eq!(picker.validating.as_deref(), Some("openrouter"));
    let prompt = picker.entry.as_ref().expect("popover prompt should remain");
    assert!(prompt.buffer.is_empty());
}

#[test]
fn login_picker_keys_swallowed_while_validating() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_login_picker(
        vec![api_key_login_entry("openrouter", "OpenRouter", None)],
        std::collections::HashMap::new(),
    );
    if let Some(picker) = app.interaction.login_picker.as_mut() {
        picker.begin_validation("openrouter", "OpenRouter");
    }
    for k in [
        key(KeyCode::Char('x')),
        key(KeyCode::Enter),
        key(KeyCode::Backspace),
        key(KeyCode::Down),
    ] {
        assert!(handle_key(&mut app, k).is_none());
    }
    let picker = app.interaction.login_picker.as_ref().unwrap();
    assert_eq!(picker.validating.as_deref(), Some("openrouter"));
    assert!(picker.filter.is_empty());
    assert!(
        picker.entry.as_ref().is_some_and(|p| p.buffer.is_empty()),
        "entry buffer must not capture keystrokes while validating"
    );
}

#[test]
fn esc_on_an_empty_input_enters_normal_on_the_composer() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));

    handle_key(&mut app, key(KeyCode::Esc));

    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(!app.interaction.focus.is_log());
}

#[test]
fn esc_leaves_what_is_typed_alone() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "hello".into();
    app.input.cursor = 5;

    handle_key(&mut app, key(KeyCode::Esc));

    assert_eq!(app.input.text, "hello");
}

#[test]
fn ctrl_o_focuses_the_log_from_insert() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));

    handle_key(&mut app, ctrl(KeyCode::Char('o')));

    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.focus.is_log());
}

#[test]
fn ctrl_o_focuses_the_log_from_normal_on_the_composer() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    handle_key(&mut app, key(KeyCode::Esc));

    handle_key(&mut app, ctrl(KeyCode::Char('o')));

    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.focus.is_log());
}

#[test]
fn esc_while_navigating_stays_in_the_log() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    handle_key(&mut app, key(KeyCode::Esc));
    handle_key(&mut app, key(KeyCode::Esc));

    handle_key(&mut app, key(KeyCode::Esc));

    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.focus.is_log());
}

#[test]
fn k_moves_the_cursor_up_one_row_while_navigating() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.viewport.window(200, 20);
    handle_key(&mut app, key(KeyCode::Esc));
    handle_key(&mut app, key(KeyCode::Esc));
    let start = app.viewport.cursor_row();

    handle_key(&mut app, key(KeyCode::Char('k')));

    assert_eq!(app.viewport.cursor_row(), start - 1);
    assert_eq!(app.viewport.window(200, 20), 180..200, "the window held");
}

fn app_with_log() -> App {
    let app = App::new("test".into(), TokenCount::new(200_000));
    let lines = vec![Line::raw("xxxxxxxxxx"); 200];
    let transcript = crate::widgets::message_list::Transcript {
        lines: lines.iter().collect(),
        chunks: Vec::new(),
    };
    app.viewport.place(&transcript, Rect::new(0, 0, 80, 20));
    app
}

fn wheel(kind: crossterm::event::MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn a_wheel_notch_up_scrolls_the_log_back() {
    let mut app = app_with_log();

    handle_mouse(
        &mut app,
        wheel(crossterm::event::MouseEventKind::ScrollUp, 0, 0),
    );

    assert_eq!(app.viewport.window(200, 20), 179..199);
}

#[test]
fn a_wheel_notch_down_scrolls_the_log_forward() {
    let mut app = app_with_log();
    app.viewport.scroll_by(-5);

    handle_mouse(
        &mut app,
        wheel(crossterm::event::MouseEventKind::ScrollDown, 0, 0),
    );

    assert_eq!(app.viewport.window(200, 20), 176..196);
}

#[test]
fn the_wheel_is_dead_while_an_overlay_is_up() {
    for mode in [
        AppMode::SessionPicker,
        AppMode::ModelPicker,
        AppMode::LoginPicker,
        AppMode::SlashComplete,
        AppMode::AtPicker,
        AppMode::SlotEdit,
        AppMode::ToolConfirm,
        AppMode::Search,
        AppMode::Settings,
        AppMode::CompactionRecovery,
    ] {
        let mut app = app_with_log();
        app.interaction.mode = mode.clone();

        handle_mouse(
            &mut app,
            wheel(crossterm::event::MouseEventKind::ScrollUp, 0, 0),
        );

        assert_eq!(app.viewport.window(200, 20), 180..200, "mode {mode:?}");
    }
}

#[test]
fn the_wheel_lives_in_the_log_and_composer_modes() {
    for mode in [AppMode::Insert, AppMode::Normal, AppMode::Visual] {
        let mut app = app_with_log();
        app.interaction.mode = mode.clone();

        handle_mouse(
            &mut app,
            wheel(crossterm::event::MouseEventKind::ScrollUp, 0, 0),
        );

        assert_eq!(app.viewport.window(200, 20), 179..199, "mode {mode:?}");
    }
}

#[test]
fn a_notch_outside_the_log_is_dead() {
    let mut app = app_with_log();

    handle_mouse(
        &mut app,
        wheel(crossterm::event::MouseEventKind::ScrollUp, 10, 21),
    );

    assert_eq!(app.viewport.window(200, 20), 180..200);
}

#[test]
fn a_notch_moves_the_configured_number_of_lines() {
    let mut app = app_with_log();
    app.interaction.mouse_scroll_lines = 5;

    handle_mouse(
        &mut app,
        wheel(crossterm::event::MouseEventKind::ScrollUp, 0, 0),
    );

    assert_eq!(app.viewport.window(200, 20), 175..195);
}

#[test]
fn v_in_the_log_anchors_a_selection_at_the_cursor() {
    let mut app = app_with_log();
    app.interaction.mode = AppMode::Normal;
    app.interaction.focus = crate::app_event::Focus::Log;
    app.viewport.cursor_to(190);
    app.viewport.move_cursor(3, 0);

    handle_key(&mut app, key(KeyCode::Char('v')));

    assert_eq!(app.interaction.mode, AppMode::Visual);
    assert_eq!(app.viewport.selection(), Some((190, 3)..=(190, 3)));
}

#[test]
fn motions_move_the_head_of_the_selection() {
    let mut app = app_with_log();
    app.interaction.mode = AppMode::Normal;
    app.interaction.focus = crate::app_event::Focus::Log;
    app.viewport.cursor_to(190);
    handle_key(&mut app, key(KeyCode::Char('v')));

    handle_key(&mut app, key(KeyCode::Char('l')));
    handle_key(&mut app, key(KeyCode::Char('j')));

    assert_eq!(app.interaction.mode, AppMode::Visual);
    assert_eq!(app.viewport.selection(), Some((190, 0)..=(191, 1)));
}

#[test]
fn capital_y_skips_the_indent_strip() {
    let mut app = app_with_log();
    app.interaction.mode = AppMode::Visual;
    app.interaction.focus = crate::app_event::Focus::Log;
    app.viewport.cursor_to(190);
    app.viewport
        .start_selection(crate::screen::SelectionKind::Char);

    let event = handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
    );

    assert!(matches!(event, Some(AppEvent::Copy { .. })), "{event:?}");
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.focus.is_log());
}

#[test]
fn esc_leaves_visual_mode_for_the_log() {
    let mut app = app_with_log();
    app.interaction.mode = AppMode::Visual;
    app.interaction.focus = crate::app_event::Focus::Log;
    app.viewport.cursor_to(190);
    app.viewport
        .start_selection(crate::screen::SelectionKind::Char);

    handle_key(&mut app, key(KeyCode::Esc));

    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.focus.is_log());
    assert_eq!(app.viewport.selection(), None);
}

#[test]
fn a_digit_holds_a_count_for_the_next_motion() {
    let mut app = app_with_log();
    app.interaction.mode = AppMode::Normal;
    app.interaction.focus = crate::app_event::Focus::Log;
    app.viewport.cursor_to(190);

    handle_key(&mut app, key(KeyCode::Char('3')));
    handle_key(&mut app, key(KeyCode::Char('j')));
    assert_eq!(app.viewport.cursor().line, 193);

    handle_key(&mut app, key(KeyCode::Char('j')));
    assert_eq!(app.viewport.cursor().line, 194, "the count was spent");
}

#[test]
fn up_and_down_walk_prompt_history_and_restore_the_draft() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("first");
    app.push_user_message("second");
    app.input.text = "draft".into();
    app.input.cursor = 5;

    handle_key(&mut app, key(KeyCode::Up));
    assert_eq!(app.input.text, "second");
    handle_key(&mut app, key(KeyCode::Up));
    assert_eq!(app.input.text, "first");
    handle_key(&mut app, key(KeyCode::Up));
    assert_eq!(app.input.text, "first");

    handle_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.input.text, "second");
    handle_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.input.text, "draft");
    handle_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.input.text, "draft");
}

#[test]
fn up_and_down_move_between_lines_before_reaching_history() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("earlier");
    app.input.text = "abcdef\nxy".into();
    app.input.cursor = app.input.text.len();

    handle_key(&mut app, key(KeyCode::Up));
    assert_eq!(app.input.text, "abcdef\nxy");
    assert_eq!(app.input.cursor, 2);

    handle_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.input.cursor, app.input.text.len());

    handle_key(&mut app, key(KeyCode::Up));
    handle_key(&mut app, key(KeyCode::Up));
    assert_eq!(app.input.text, "earlier");
}

#[test]
fn ctrl_g_asks_for_the_external_editor() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "draft".into();
    let event = handle_key(&mut app, ctrl(KeyCode::Char('g')));
    assert!(matches!(event, Some(AppEvent::ExternalEditor)));
    assert_eq!(app.input.text, "draft");
}

#[test]
fn yanking_across_a_soft_wrap_copies_one_line() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.messages.push(crate::display_types::DisplayMessage::new(
        crate::display_types::MessageRole::Assistant,
        "alpha bravo charlie delta echo foxtrot golf hotel india",
    ));
    let area = Rect::new(0, 0, 24, 20);
    let guards = crate::widgets::message_list::transcript_guards(&app, area.width);
    let transcript = guards.flatten();
    let rows: Vec<String> = transcript.iter().map(ToString::to_string).collect();
    let last = rows.iter().rposition(|row| !row.trim().is_empty()).unwrap();
    let first = rows.iter().position(|row| row.contains("alpha")).unwrap();
    assert!(last > first, "the message should wrap: {rows:?}");
    app.viewport.place(&transcript, area);
    drop(transcript);
    drop(guards);

    app.interaction.mode = AppMode::Normal;
    app.interaction.focus = crate::app_event::Focus::Log;
    app.viewport.cursor_to(first);
    handle_key(&mut app, key(KeyCode::Char('V')));
    app.viewport.cursor_to(last);
    let event = handle_key(&mut app, key(KeyCode::Char('y')));

    match event {
        Some(AppEvent::Copy { text }) => assert_eq!(
            text,
            "alpha bravo charlie delta echo foxtrot golf hotel india"
        ),
        other => panic!("expected a copy, got {other:?}"),
    }
}

fn composer_normal_with_an_image() -> App {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = format!("a{}b", crate::input_buffer::IMAGE_PLACEHOLDER);
    app.input.images.push(crate::app::PendingImage {
        data: vec![1, 2, 3],
        mime_type: mush_ai::types::ImageMimeType::Png,
        dimensions: None,
    });
    app.input.cursor = 1;
    app.interaction.mode = AppMode::Normal;
    app
}

#[test]
fn x_on_an_image_carries_the_image_into_the_register_and_p_brings_it_back() {
    let mut app = composer_normal_with_an_image();

    handle_key(&mut app, key(KeyCode::Char('x')));
    assert_eq!(app.input.text, "ab");
    assert!(app.input.images.is_empty());

    handle_key(&mut app, key(KeyCode::Char('p')));
    assert_eq!(
        app.input.text,
        format!("ab{}", crate::input_buffer::IMAGE_PLACEHOLDER)
    );
    assert_eq!(app.input.images.len(), 1);
    assert_eq!(app.input.images[0].data, vec![1, 2, 3]);
}

#[test]
fn yanking_an_image_line_and_putting_it_doubles_the_image() {
    let mut app = composer_normal_with_an_image();

    handle_key(&mut app, key(KeyCode::Char('y')));
    handle_key(&mut app, key(KeyCode::Char('y')));
    handle_key(&mut app, key(KeyCode::Char('p')));

    assert_eq!(app.input.images.len(), 2);
    assert_eq!(
        app.input
            .text
            .matches(crate::input_buffer::IMAGE_PLACEHOLDER)
            .count(),
        2
    );
}
