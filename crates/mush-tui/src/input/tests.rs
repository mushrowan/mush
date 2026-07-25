//! tests for input

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use mush_ai::types::{ThinkingLevel, TokenCount};

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
    // simulates the path after `/refresh-models` (or a fresh discovery
    // fetch) writes new entries into the discovery cache: we hand the
    // helper a hand-built merged-catalogue slice and expect both
    // `completions` and `model_completions` to be replaced with the
    // matching ids/rows. this is the bit that makes new chatgpt
    // subscription models show up in the picker without an app restart
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
    // first ctrl+c on an empty idle prompt arms a confirmation rather
    // than quitting outright. status bar gets the hint
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(event.is_none());
    assert!(app.confirm_quit_pending);
    assert_eq!(app.status.as_deref(), Some("press ctrl+c again to exit"));
}

#[test]
fn ctrl_c_twice_quits() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let _ = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(matches!(event, Some(AppEvent::Quit)));
}

#[test]
fn ctrl_c_with_text_clears_input_without_quitting() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "draft prompt".into();
    app.input.cursor = app.input.text.len();
    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(event.is_none());
    assert!(app.input.text.is_empty());
    assert_eq!(app.input.cursor, 0);
    assert!(!app.confirm_quit_pending);
}

#[test]
fn ctrl_c_clear_then_ctrl_c_arms_quit() {
    // after clearing input, the prompt is empty so the next ctrl+c
    // should arm the confirm (not quit immediately)
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.input.text = "stuff".into();
    app.input.cursor = 5;
    let _ = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(event.is_none());
    assert!(app.confirm_quit_pending);
}

#[test]
fn any_other_key_resets_quit_confirm() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let _ = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(app.confirm_quit_pending);
    // typing 'a' should reset the armed flag so the next ctrl+c
    // re-arms instead of quitting
    let _ = handle_key(&mut app, key(KeyCode::Char('a')));
    assert!(!app.confirm_quit_pending);
    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    // input is "a" now, so ctrl+c clears it
    assert!(event.is_none());
    assert!(app.input.text.is_empty());
}

#[test]
fn ctrl_c_when_busy_aborts_without_confirm() {
    // ctrl+c during a stream should abort the turn (UNIX SIGINT-ish),
    // not quit the program
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
    assert!(matches!(event, Some(AppEvent::Abort)));
    assert!(!app.confirm_quit_pending);
}

#[test]
fn ctrl_v_emits_paste_image_when_idle() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let event = handle_key(&mut app, ctrl(KeyCode::Char('v')));
    assert!(matches!(event, Some(AppEvent::PasteImage)));
}

#[test]
fn ctrl_v_emits_paste_image_when_streaming() {
    // regression: ctrl+v fell through to handle_editing during streaming
    // so image paste only worked while idle, breaking image-based steering
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    let event = handle_key(&mut app, ctrl(KeyCode::Char('v')));
    assert!(matches!(event, Some(AppEvent::PasteImage)));
}

#[test]
fn escape_aborts_when_streaming() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    let event = handle_key(&mut app, key(KeyCode::Esc));
    assert!(matches!(event, Some(AppEvent::Abort)));
}

#[test]
fn escape_scrolls_to_bottom_before_aborting_when_streaming_and_scrolled_up() {
    // cascade:
    //   1. scroll mode with selection: esc clears selection
    //   2. scroll mode no selection: esc exits scroll mode
    //   3. normal mode scrolled up: esc scrolls to bottom (even mid-stream)
    //   4. normal mode at bottom + streaming: esc aborts
    // this test covers step 3: esc while streaming + scrolled shouldn't
    // immediately abort. it should scroll to bottom first; the next esc
    // is the one that cancels
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    app.scroll_offset = 25;
    let event = handle_key(&mut app, key(KeyCode::Esc));
    assert!(
        event.is_none(),
        "first esc should scroll to bottom, not abort (got {event:?})"
    );
    assert_eq!(app.scroll_offset, 0);
    // second esc now aborts since we're at bottom
    let event = handle_key(&mut app, key(KeyCode::Esc));
    assert!(matches!(event, Some(AppEvent::Abort)));
}

#[test]
fn escape_does_nothing_when_idle() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let event = handle_key(&mut app, key(KeyCode::Esc));
    assert!(event.is_none());
}

#[test]
fn escape_aborts_while_tool_runs_between_messages() {
    // between `MessageEnd` and the next `TurnStart`, `stream.active`
    // flips to false even though the agent is still busy executing
    // tools. esc used to only check `stream.active`, so the user had
    // no way to cancel a long-running tool (e.g. a 15 minute bash
    // call via bash_status polling). cancel should fire whenever
    // the agent is busy, not only while text is streaming
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
    assert!(
        matches!(event, Some(AppEvent::Abort)),
        "esc should abort while a tool is running, got {event:?}"
    );
}

#[test]
fn jump_to_bottom_hotkey_scrolls_without_aborting() {
    // a direct hotkey for "go to bottom", independent of the esc cascade.
    // default binding: shift+End. works in any state without cancelling
    // the stream so it's safe to mash while the agent's working
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    app.scroll_offset = 40;
    let event = handle_key(&mut app, KeyEvent::new(KeyCode::End, KeyModifiers::SHIFT));
    assert!(
        event.is_none(),
        "jump_to_bottom should not emit Abort (got {event:?})"
    );
    assert_eq!(app.scroll_offset, 0);
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
fn page_up_down_scrolls() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let event = handle_key(&mut app, key(KeyCode::PageUp));
    assert!(matches!(event, Some(AppEvent::ScrollUp(10))));
    assert_eq!(app.scroll_offset, 10);

    let event = handle_key(&mut app, key(KeyCode::PageDown));
    assert!(matches!(event, Some(AppEvent::ScrollDown(10))));
    assert_eq!(app.scroll_offset, 0);
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
    // codex models advertise a curated `supported_thinking_levels` set.
    // the cycler must walk those exactly (skipping Off when the model
    // is reasoning-only) instead of the legacy hardcoded progression
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
    // wrap around to the first curated entry, not Off
    handle_key(&mut app, ctrl(KeyCode::Char('t')));
    assert_eq!(app.thinking_level, ThinkingLevel::Low);
}

#[test]
fn ctrl_t_resets_to_first_curated_level_when_current_not_supported() {
    // switching to a codex model from Xhigh on a generic one should
    // land us on the model's first supported level, not stall on the
    // unsupported one
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.supported_thinking_levels = vec![ThinkingLevel::Medium, ThinkingLevel::High];
    app.thinking_level = ThinkingLevel::Xhigh; // not in the curated set

    handle_key(&mut app, ctrl(KeyCode::Char('t')));
    assert_eq!(app.thinking_level, ThinkingLevel::Medium);
}

#[test]
fn ctrl_o_no_longer_toggles_thinking_expanded() {
    // moved to /thinking-expand. ctrl+o is no longer bound to
    // anything by default so the keystroke falls through to the
    // input editor (or future user binds)
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
fn configured_pane_split_binding_is_used() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let raw = crate::keybinds::scoped_test_config(
        crate::keybinds::KeyScope::Pane,
        [(
            "split_pane",
            crate::keybinds::BindingSource::One("alt+enter".into()),
        )],
    );
    app.keymap.override_scoped(&raw);

    let event = handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));

    assert!(matches!(event, Some(AppEvent::SplitPane)));
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
fn ctrl_d_on_empty_quits() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let event = handle_key(&mut app, ctrl(KeyCode::Char('d')));
    assert!(matches!(event, Some(AppEvent::Quit)));
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
fn configured_delete_char_or_quit_binding_is_used() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let raw = crate::keybinds::scoped_test_config(
        crate::keybinds::KeyScope::Edit,
        [(
            "delete_char_or_quit",
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
        crate::keybinds::KeyScope::Normal,
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
    assert!(app.confirm_quit_pending);
}

#[test]
fn configured_focus_pane_binding_is_used() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let raw = crate::keybinds::scoped_test_config(
        crate::keybinds::KeyScope::Pane,
        [(
            "focus_pane_two",
            crate::keybinds::BindingSource::One("alt+w".into()),
        )],
    );
    app.keymap.override_scoped(&raw);

    let event = handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::ALT),
    );

    assert!(matches!(event, Some(AppEvent::FocusPaneByIndex(1))));
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
    // input preserved so user can submit after streaming ends
    assert_eq!(app.input.text, "/compact");
    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.contains("blocked while streaming")),
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

    // second tab cycles to next match
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.input.text, "/history");

    // wraps around
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

    // typing resets completion state
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
    // tab on `@review` where a template named "review" exists should
    // replace the `@review` span with the template content. the rest
    // of the input stays untouched
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "please review the changes");
    app.input.text = "hi @review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.input.text, "hi please review the changes");
}

#[test]
fn at_template_enter_sends_literal_without_expansion() {
    // design pin: `@asdf<return>` sends `@asdf` verbatim,
    // even when a template named `asdf` exists. only tab expands
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
    // tab on `@unknown` should NOT consume the keystroke: without a
    // matching template the tab falls through to regular completion.
    // today that just cycles generic completions (none here), but the
    // input must stay intact
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "irrelevant");
    app.input.text = "@missing".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.input.text, "@missing");
}

#[test]
fn at_template_tab_opens_picker_when_multiple_prefix_matches() {
    // d2: tab on `@rev` with two `rev*` templates and no exact match
    // opens the AtPicker popup. input stays untouched until enter
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

    // tab wraps from the end back to the top
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

    // shift+tab from selected==0 wraps to the bottom
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
    // cycle to the second match (review-pr)
    handle_key(&mut app, key(KeyCode::Tab));

    let event = handle_key(&mut app, key(KeyCode::Enter));

    // enter inside the picker inserts and closes; it must NOT submit
    assert!(
        event.is_none(),
        "enter inside the picker must not submit, got {event:?}"
    );
    assert_eq!(app.input.text, "hi REVIEW PR");
    assert_eq!(app.interaction.mode, AppMode::Normal);
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

    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.at_picker.is_none());
    assert_eq!(app.input.text, "@rev", "esc preserves the trigger text");
}

#[test]
fn at_picker_typing_narrows_matches() {
    // typing while the picker is open extends the trigger word and
    // narrows the candidate list. mirrors the slash menu UX so the
    // user can keep refining without esc-ing first
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "REVIEW");
    push_test_template(&mut app, "review-pr", "REVIEW PR");
    push_test_template(&mut app, "release", "RELEASE");
    app.input.text = "@re".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    // type 'v' → @rev → drops `release`, keeps `review` + `review-pr`
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
    // when the user types something that no template prefix-matches,
    // the picker closes silently and the typed character stays put
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "REVIEW");
    app.input.text = "@re".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    // type 'z' → @rez → no template matches
    handle_key(&mut app, key(KeyCode::Char('z')));

    assert_eq!(app.input.text, "@rez");
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.at_picker.is_none());
}

#[test]
fn at_picker_backspace_past_at_sign_closes() {
    // backspacing past the `@` dissolves the trigger entirely; the
    // picker must close so we don't keep stale matches around
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "REVIEW");
    push_test_template(&mut app, "review-pr", "REVIEW PR");
    app.input.text = "@r".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));
    // backspace twice: @r → @ → empty (no trigger)
    handle_key(&mut app, key(KeyCode::Backspace));
    handle_key(&mut app, key(KeyCode::Backspace));

    assert_eq!(app.input.text, "");
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.at_picker.is_none());
}

#[test]
fn template_with_placeholders_enters_slot_edit_mode() {
    // d3: expanding a template that contains `$1`/`$2`/`$@` should
    // enter slot edit mode with placeholders stripped to empty slots
    // and the cursor parked at the first slot
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "review $1 with $2");
    app.input.text = "@review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    // template content "review $1 with $2" expands to "review  with "
    // with two slots at byte offsets 7 and 13 of the input
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
    // expanding a placeholder-free template must NOT enter slot mode
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "plain content");
    app.input.text = "@review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    assert_eq!(app.input.text, "plain content");
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.slot_edit.is_none());
}

#[test]
fn slot_edit_typing_shifts_later_slot_offsets() {
    // typing inside a slot pushes later slots' byte offsets right by
    // the inserted byte count so tab still parks on the right place
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "$1 then $2");
    app.input.text = "@review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));
    // initial slots: [0, 6] (after "$1" stripped: " then ", $2 lived at 8 → now 6)

    // type "abc" into slot 0
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
    // cursor at slot 0 end (3), tab jumps to slot 1 (now at 9)
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

    // enter inside slot mode is "done filling slots", not submit.
    // user gets a chance to add more text or hit enter again
    assert!(event.is_none(), "first enter must not submit");
    assert_eq!(app.input.text, "fix bug");
    assert_eq!(app.interaction.mode, AppMode::Normal);
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

    // esc bails out of slot mode but leaves whatever was typed.
    // the user can keep editing in normal mode without weirdness
    assert_eq!(app.input.text, "fix x");
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.slot_edit.is_none());
}

#[test]
fn slot_edit_shift_tab_cycles_backwards_with_wrap() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    push_test_template(&mut app, "review", "$1 / $2 / $3");
    app.input.text = "@review".into();
    app.input.cursor = app.input.text.len();
    handle_key(&mut app, key(KeyCode::Tab));

    // shift+tab from slot 0 wraps to last slot
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

    // tab → slot 1 → tab → wrap back to slot 0
    handle_key(&mut app, key(KeyCode::Tab));
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.interaction.slot_edit.as_ref().unwrap().current, 0);
}

#[test]
fn slot_edit_backspace_at_slot_start_is_a_noop() {
    // protect verbatim template content: backspace at the start of
    // the current slot must not delete the byte before it
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

    // ctrl+j moves down
    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    assert_eq!(app.completion.slash_menu.as_ref().unwrap().selected, 1);

    // ctrl+k moves up
    handle_key(&mut app, ctrl(KeyCode::Char('k')));
    assert_eq!(app.completion.slash_menu.as_ref().unwrap().selected, 0);

    // enter selects
    handle_key(&mut app, key(KeyCode::Enter));
    assert_eq!(app.interaction.mode, AppMode::Normal);
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

    // first tab opens the menu with selection 0
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.interaction.mode, AppMode::SlashComplete);
    assert_eq!(app.completion.slash_menu.as_ref().unwrap().selected, 0);

    // subsequent tab advances selection
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.completion.slash_menu.as_ref().unwrap().selected, 1);

    // tab at end wraps to 0
    handle_key(&mut app, key(KeyCode::Tab));
    assert_eq!(app.completion.slash_menu.as_ref().unwrap().selected, 0);

    // shift+backtab moves back, wrapping past 0 to last
    let shift_backtab = KeyEvent {
        code: KeyCode::BackTab,
        modifiers: KeyModifiers::SHIFT,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    };
    handle_key(&mut app, shift_backtab);
    assert_eq!(app.completion.slash_menu.as_ref().unwrap().selected, 1);

    // tab no longer accepts, enter does. menu is still open
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
    // advance to /login-complete
    handle_key(&mut app, key(KeyCode::Tab));

    handle_key(&mut app, key(KeyCode::Enter));
    assert_eq!(app.interaction.mode, AppMode::Normal);
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
            .as_deref()
            .is_some_and(|s| s.contains("favourite")),
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

    // only /h* commands match
    let menu = app.completion.slash_menu.as_ref().unwrap();
    assert_eq!(menu.matches.len(), 2);

    // type 'e' to narrow to /he*
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
    // use a blocked slash command so the input gets reinserted.
    // `/new` is in the blocked-during-stream list and ignores trailing
    // args, so it parses successfully into a non-safe action
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
    assert_eq!(app.interaction.mode, AppMode::Normal);
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
fn alt_number_focuses_pane() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let event = handle_key(&mut app, alt(KeyCode::Char('1')));
    assert!(matches!(event, Some(AppEvent::FocusPaneByIndex(0))));

    let event = handle_key(&mut app, alt(KeyCode::Char('3')));
    assert!(matches!(event, Some(AppEvent::FocusPaneByIndex(2))));

    let event = handle_key(&mut app, alt(KeyCode::Char('9')));
    assert!(matches!(event, Some(AppEvent::FocusPaneByIndex(8))));
}

#[test]
fn ctrl_shift_enter_splits_pane() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let event = handle_key(
        &mut app,
        KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        },
    );
    assert!(matches!(event, Some(AppEvent::SplitPane)));
}

#[test]
fn ctrl_tab_focuses_next_pane() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let event = handle_key(
        &mut app,
        KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        },
    );
    assert!(matches!(event, Some(AppEvent::FocusNextPane)));
}

#[test]
fn ctrl_shift_tab_focuses_prev_pane() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    let event = handle_key(
        &mut app,
        KeyEvent {
            code: KeyCode::BackTab,
            modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        },
    );
    assert!(matches!(event, Some(AppEvent::FocusPrevPane)));
}

#[test]
fn pane_keys_work_while_streaming() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;

    // alt+number should still focus panes
    let event = handle_key(&mut app, alt(KeyCode::Char('2')));
    assert!(matches!(event, Some(AppEvent::FocusPaneByIndex(1))));

    // ctrl+shift+enter should still split
    let event = handle_key(
        &mut app,
        KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        },
    );
    assert!(matches!(event, Some(AppEvent::SplitPane)));

    // ctrl+tab should still cycle panes
    let event = handle_key(
        &mut app,
        KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        },
    );
    assert!(matches!(event, Some(AppEvent::FocusNextPane)));

    // ctrl+shift+left should resize
    let event = handle_key(
        &mut app,
        KeyEvent {
            code: KeyCode::Left,
            modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        },
    );
    assert!(matches!(event, Some(AppEvent::ResizePane(-4))));
}

#[test]
fn scroll_mode_v_toggles_visual_selection() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("one");
    app.push_user_message("two");
    app.push_user_message("three");

    // enter scroll mode then switch to message mode
    handle_key(&mut app, ctrl(KeyCode::Char('s')));
    handle_key(&mut app, key(KeyCode::Char('b')));
    assert_eq!(app.interaction.mode, AppMode::Scroll);
    assert_eq!(app.navigation.selected_message, Some(2));
    assert!(app.navigation.selection_anchor.is_none());

    // press v to start visual selection
    handle_key(&mut app, key(KeyCode::Char('v')));
    assert_eq!(app.navigation.selection_anchor, Some(2));

    // press v again to toggle off
    handle_key(&mut app, key(KeyCode::Char('v')));
    assert!(app.navigation.selection_anchor.is_none());
}

#[test]
fn scroll_mode_visual_selection_range() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("one");
    app.push_user_message("two");
    app.push_user_message("three");

    handle_key(&mut app, ctrl(KeyCode::Char('s')));
    handle_key(&mut app, key(KeyCode::Char('b'))); // switch to message mode
    assert_eq!(app.navigation.selected_message, Some(2));

    // start visual selection at message 2
    handle_key(&mut app, key(KeyCode::Char('v')));
    assert_eq!(app.navigation.selection_anchor, Some(2));

    // move up twice
    handle_key(&mut app, key(KeyCode::Char('k')));
    handle_key(&mut app, key(KeyCode::Char('k')));
    assert_eq!(app.navigation.selected_message, Some(0));

    // range should be 0..=2
    assert_eq!(app.selection_range(), Some((0, 2)));
}

#[test]
fn scroll_mode_esc_clears_selection_first() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("one");
    app.push_user_message("two");

    handle_key(&mut app, ctrl(KeyCode::Char('s')));
    handle_key(&mut app, key(KeyCode::Char('b'))); // switch to message mode
    handle_key(&mut app, key(KeyCode::Char('v')));
    assert!(app.navigation.selection_anchor.is_some());

    // first esc clears selection, stays in scroll mode
    handle_key(&mut app, key(KeyCode::Esc));
    assert!(app.navigation.selection_anchor.is_none());
    assert_eq!(app.interaction.mode, AppMode::Scroll);

    // second esc exits scroll mode
    handle_key(&mut app, key(KeyCode::Esc));
    assert_eq!(app.interaction.mode, AppMode::Normal);
}

#[test]
fn scroll_mode_exit_clears_selection() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("one");

    handle_key(&mut app, ctrl(KeyCode::Char('s')));
    handle_key(&mut app, key(KeyCode::Char('b'))); // switch to message mode
    handle_key(&mut app, key(KeyCode::Char('v')));
    assert!(app.navigation.selection_anchor.is_some());

    // esc clears selection first
    handle_key(&mut app, key(KeyCode::Esc));
    assert!(app.navigation.selection_anchor.is_none());
    assert_eq!(app.interaction.mode, AppMode::Scroll);
}

#[test]
fn esc_cascade_scroll_select_to_abort() {
    // regression for L290 (todo.md): pressing esc repeatedly should
    // walk a 4-step cascade when streaming:
    //   1. clear visual selection (stay in scroll mode)
    //   2. exit scroll mode (stay scrolled up)
    //   3. scroll to bottom (stay in normal mode)
    //   4. abort the in-flight stream
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..5 {
        app.push_user_message(format!("msg {i}"));
    }

    // enter scroll mode, select a message, start visual selection,
    // then scroll up so the later scroll_offset > 0 branch fires.
    // ctrl+s is an idle-only binding, so enter scroll mode before
    // simulating the in-flight stream
    handle_key(&mut app, ctrl(KeyCode::Char('s')));
    handle_key(&mut app, key(KeyCode::Char('b')));
    handle_key(&mut app, key(KeyCode::Char('v')));
    assert!(app.navigation.selection_anchor.is_some());
    app.scroll_offset = 20;
    app.stream.active = true;

    // 1. first esc: clears selection, scroll mode retained
    assert!(handle_key(&mut app, key(KeyCode::Esc)).is_none());
    assert_eq!(app.interaction.mode, AppMode::Scroll);
    assert!(app.navigation.selection_anchor.is_none());
    assert_eq!(app.scroll_offset, 20);

    // 2. second esc: exits scroll mode, stays scrolled up
    assert!(handle_key(&mut app, key(KeyCode::Esc)).is_none());
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert_eq!(app.scroll_offset, 20);

    // 3. third esc: anchors at bottom of message log
    assert!(handle_key(&mut app, key(KeyCode::Esc)).is_none());
    assert_eq!(app.scroll_offset, 0);

    // 4. fourth esc: now at bottom, cancels the stream
    let event = handle_key(&mut app, key(KeyCode::Esc));
    assert!(matches!(event, Some(AppEvent::Abort)));
}

#[test]
fn scroll_mode_b_toggles_scroll_unit() {
    use crate::app::ScrollUnit;
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("hello");

    handle_key(&mut app, ctrl(KeyCode::Char('s')));
    assert_eq!(app.interaction.mode, AppMode::Scroll);

    // default is Block
    assert_eq!(app.navigation.scroll_unit, ScrollUnit::Block);

    // b toggles to Message
    handle_key(&mut app, key(KeyCode::Char('b')));
    assert_eq!(app.navigation.scroll_unit, ScrollUnit::Message);

    // b again toggles back to Block
    handle_key(&mut app, key(KeyCode::Char('b')));
    assert_eq!(app.navigation.scroll_unit, ScrollUnit::Block);
}

#[test]
fn scroll_mode_block_jk_navigates_paragraphs_and_code_blocks() {
    use crate::app::ScrollUnit;
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.start_streaming();
    app.push_text_delta("here:\n```bash\nrm -rf mything\n```\nand:\n```python\nprint(42)\n```");
    app.finish_streaming(None, None);

    // four selectable blocks now: "here:" para, bash fence, "and:"
    // para, python fence
    let blocks = app.blocks();
    assert_eq!(blocks.len(), 4, "expected 4 blocks, got {blocks:#?}");

    // enter scroll mode (default: Block)
    handle_key(&mut app, ctrl(KeyCode::Char('s')));
    assert_eq!(app.navigation.scroll_unit, ScrollUnit::Block);

    // should start on the last block
    assert_eq!(app.navigation.selected_block, Some(3));

    // k walks back through every block in document order
    handle_key(&mut app, key(KeyCode::Char('k')));
    assert_eq!(app.navigation.selected_block, Some(2));
    handle_key(&mut app, key(KeyCode::Char('k')));
    assert_eq!(app.navigation.selected_block, Some(1));
    handle_key(&mut app, key(KeyCode::Char('k')));
    assert_eq!(app.navigation.selected_block, Some(0));

    // k at start stays at 0
    handle_key(&mut app, key(KeyCode::Char('k')));
    assert_eq!(app.navigation.selected_block, Some(0));

    // j walks forward through every block
    handle_key(&mut app, key(KeyCode::Char('j')));
    assert_eq!(app.navigation.selected_block, Some(1));
    handle_key(&mut app, key(KeyCode::Char('j')));
    assert_eq!(app.navigation.selected_block, Some(2));
    handle_key(&mut app, key(KeyCode::Char('j')));
    assert_eq!(app.navigation.selected_block, Some(3));

    // j at end stays at last
    handle_key(&mut app, key(KeyCode::Char('j')));
    assert_eq!(app.navigation.selected_block, Some(3));
}

/// install cached heights for tall messages so viewport intersection
/// checks have something to consume. no real render runs in unit tests
fn install_tall_message_heights(app: &mut App, height_each: usize, visible: u16) {
    let count = app.messages.len();
    let mut cache = app.render_state.height_cache.borrow_mut();
    cache.resize_with(count, || None);
    for slot in cache.iter_mut() {
        *slot = Some(crate::app_state::CachedHeight {
            content_len: 0,
            thinking_len: 0,
            tool_output_len: 0,
            completed_tool_count: 0,
            thinking_expanded: false,
            has_usage: false,
            width: 80,
            height: height_each,
        });
    }
    drop(cache);
    app.render_state.visible_area_height.set(visible);
}

#[test]
fn scroll_mode_k_only_advances_selection_when_target_visible() {
    use crate::app::ScrollUnit;
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..5 {
        app.push_user_message(format!("msg {i}"));
    }
    // 5 msgs, 10 lines each, total 50. visible 10. viewport holds one msg.
    install_tall_message_heights(&mut app, 10, 10);

    handle_key(&mut app, ctrl(KeyCode::Char('s')));
    handle_key(&mut app, key(KeyCode::Char('b')));
    assert_eq!(app.navigation.scroll_unit, ScrollUnit::Message);
    assert_eq!(app.navigation.selected_message, Some(4));
    // start at bottom: scroll_offset=0, viewport [40,50), only msg 4 visible

    // press k: scroll_offset → 3, viewport [37,47). msg 3 [30,40) overlaps → visible, select it
    handle_key(&mut app, key(KeyCode::Char('k')));
    assert_eq!(app.scroll_offset, 3);
    assert_eq!(app.navigation.selected_message, Some(3));

    // press k again: scroll_offset → 6, viewport [34,44). msg 2 [20,30):
    // 20<44 but 30>34 is false → not visible. selection stays on msg 3
    handle_key(&mut app, key(KeyCode::Char('k')));
    assert_eq!(app.scroll_offset, 6);
    assert_eq!(
        app.navigation.selected_message,
        Some(3),
        "selection must not move to an off-screen message"
    );

    // press k until msg 2 becomes visible. at scroll 11: viewport [29,39).
    // msg 2 [20,30): 20<39 && 30>29 → visible
    for _ in 0..2 {
        handle_key(&mut app, key(KeyCode::Char('k')));
    }
    assert_eq!(app.scroll_offset, 12);
    assert_eq!(app.navigation.selected_message, Some(2));
}

#[test]
fn scroll_mode_j_only_advances_selection_when_target_visible() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    for i in 0..5 {
        app.push_user_message(format!("msg {i}"));
    }
    install_tall_message_heights(&mut app, 10, 10);

    handle_key(&mut app, ctrl(KeyCode::Char('s')));
    handle_key(&mut app, key(KeyCode::Char('b')));
    // start at bottom then scroll all the way up
    app.scroll_offset = 40;
    app.navigation.selected_message = Some(0);
    // viewport [0,10): only msg 0 visible

    // press j: scroll_offset → 37, viewport [3,13). msg 1 [10,20) overlaps → select
    handle_key(&mut app, key(KeyCode::Char('j')));
    assert_eq!(app.scroll_offset, 37);
    assert_eq!(app.navigation.selected_message, Some(1));

    // press j again: scroll_offset → 34, viewport [6,16). msg 2 [20,30): 20<16 false → not visible
    handle_key(&mut app, key(KeyCode::Char('j')));
    assert_eq!(app.scroll_offset, 34);
    assert_eq!(
        app.navigation.selected_message,
        Some(1),
        "selection must not move to an off-screen message"
    );
}

#[test]
fn scroll_mode_respects_custom_scroll_lines() {
    // custom scroll_lines=5 should scroll 5 lines per keystroke, not 3
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.scroll_lines = 5;
    for i in 0..5 {
        app.push_user_message(format!("msg {i}"));
    }
    install_tall_message_heights(&mut app, 10, 10);

    handle_key(&mut app, ctrl(KeyCode::Char('s')));
    handle_key(&mut app, key(KeyCode::Char('b')));
    assert_eq!(app.scroll_offset, 0);

    handle_key(&mut app, key(KeyCode::Char('k')));
    assert_eq!(app.scroll_offset, 5);

    handle_key(&mut app, key(KeyCode::Char('j')));
    assert_eq!(app.scroll_offset, 0);
}

// ctrl+[ is the same byte as ESC in legacy terminal mode, but terminals
// with kitty keyboard enhancement disambiguate them. these tests pin
// that ctrl+[ cancels every modal the same way ESC does.

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
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.completion.slash_menu.is_none());
}

#[test]
fn ctrl_bracket_exits_scroll_mode() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("one");
    handle_key(&mut app, ctrl(KeyCode::Char('s')));
    assert_eq!(app.interaction.mode, AppMode::Scroll);

    handle_key(&mut app, ctrl(KeyCode::Char('[')));
    assert_eq!(app.interaction.mode, AppMode::Normal);
}

#[test]
fn ctrl_bracket_exits_search_mode() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.push_user_message("one");
    app.interaction.mode = AppMode::Search;

    handle_key(&mut app, ctrl(KeyCode::Char('[')));
    assert_eq!(app.interaction.mode, AppMode::Normal);
}

#[test]
fn ctrl_bracket_closes_session_picker() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.open_session_picker(test_sessions(), "/tmp".into());
    assert_eq!(app.interaction.mode, AppMode::SessionPicker);

    handle_key(&mut app, ctrl(KeyCode::Char('[')));
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.session_picker.is_none());
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
            id: SessionId::from("one"),
            title: Some("hello".into()),
            model_id: ModelId::from("m"),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 1,
            cwd: "/tmp".into(),
        },
        mush_session::SessionMeta {
            id: SessionId::from("two"),
            title: Some("jkl session".into()),
            model_id: ModelId::from("m"),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 1,
            cwd: "/tmp".into(),
        },
    ]
}

/// seed an app with two anthropic models and one openai model so
/// the model picker tests have a deterministic catalogue
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
    assert_eq!(app.interaction.mode, AppMode::Normal);
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
    // can't go past the last row
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
    // favourites present, so the picker opens on the favourites tab
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
    // navigate to the second row then press enter
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
    assert_eq!(app.interaction.mode, AppMode::Normal);
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

    // toggling again removes it
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

/// build an app whose model catalogue contains one fresh row and
/// one stale row in that order, so tests can exercise ctrl+d and
/// ctrl+shift+d
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
    // ctrl+d on a fresh row toasts a hint instead of arming a
    // delete; on a stale row it arms a Single confirmation that
    // n then cancels without mutating state
    let mut app = app_with_one_stale_row();
    app.open_model_picker_overlay();

    // selected is 0 (fresh), ctrl+d should toast and not arm
    handle_key(&mut app, ctrl(KeyCode::Char('d')));
    let picker = app.interaction.model_picker.as_ref().unwrap();
    assert!(picker.confirm_delete.is_none());
    assert!(picker.toast.as_deref().unwrap().contains("only stale"));

    // move selection to the stale row
    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    handle_key(&mut app, ctrl(KeyCode::Char('d')));
    let picker = app.interaction.model_picker.as_ref().unwrap();
    assert!(matches!(
        picker.confirm_delete,
        Some(crate::slash_menu::DeleteConfirm::Single { .. })
    ));

    // n cancels the confirm without mutating state
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
fn ctrl_bracket_aborts_streaming() {
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;

    let event = handle_key(&mut app, ctrl(KeyCode::Char('[')));
    assert!(matches!(event, Some(AppEvent::Abort)));
}

#[test]
fn up_arrow_lifts_queued_when_input_empty_streaming() {
    // user is streaming with a queued steering message. pressing Up
    // on an empty input line should lift the queued message back
    // into the input for editing, matching the behaviour of Alt+K
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
    // lift also works when the agent isn't streaming (queue may
    // still have pending entries between turns)
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
    // classic emacs kill-line behaviour must still work when the
    // user has typed something or when no queued message exists
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
    // without a queued message there's nothing to lift, so Up falls
    // through to existing behaviour (no-op in editing mode)
    let mut app = App::new("test".into(), TokenCount::new(200_000));
    app.stream.active = true;
    // no queued messages pushed

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
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.login_picker.is_none());
}

#[test]
fn login_picker_ctrl_j_k_navigate_visible_rows() {
    let mut app = app_with_login_picker();
    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    assert_eq!(app.interaction.login_picker.as_ref().unwrap().selected, 1);
    // can't navigate past the last row
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

    // typing reorders the list and pushes selection back to the top
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
    // pressing enter on a logged-out oauth row should kick the
    // existing login flow by emitting SlashAction::Login with the
    // oauth provider id (so the existing handler can begin_login)
    let mut app = app_with_login_picker();
    // navigate to logged-out row
    handle_key(&mut app, ctrl(KeyCode::Char('j')));
    let event = handle_key(&mut app, key(KeyCode::Enter));
    match event {
        Some(AppEvent::SlashCommand {
            action: crate::slash::SlashAction::Login { provider: Some(id) },
        }) => assert_eq!(id, "openai-codex"),
        other => panic!("expected SlashAction::Login, got {other:?}"),
    }
    assert_eq!(app.interaction.mode, AppMode::Normal);
    assert!(app.interaction.login_picker.is_none());
}

#[test]
fn login_picker_enter_on_logged_out_api_key_arms_entry_prompt() {
    // api-key rows don't kick a slash command - they arm the
    // in-picker key entry prompt so the user can paste their key
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
    // arm entry mode
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
    // picker itself stays open
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
    // press enter without typing anything - must not write an
    // empty key and must surface a helpful toast
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
    // logged-in row gets a logout confirmation instead of a login
    // action. picker stays open while the user confirms
    let mut app = app_with_login_picker();
    // logged-in row is at index 0
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
    // saving an api key must go through provider validation first.
    // hitting enter with a typed buffer should hand the key off to
    // the runner via an event AND immediately switch the picker to
    // its validating state so the user gets feedback (no silent
    // gap, no chance to retype while a request is in flight)
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
    // entry stays alive (with empty buffer) so the popover keeps
    // rendering across the in-flight roundtrip
    let prompt = picker.entry.as_ref().expect("popover prompt should remain");
    assert!(prompt.buffer.is_empty());
}

#[test]
fn login_picker_keys_swallowed_while_validating() {
    // input must not mutate the picker while a validation is in
    // flight. otherwise typing during the (typically sub-second)
    // network roundtrip could spuriously re-arm the entry prompt
    // or filter the row list, and the runner's state restore
    // would race with user input
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
    // entry prompt stays alive across the in-flight roundtrip so
    // the popover renders continuously, but its buffer must stay
    // empty (no typed key should be captured during validation)
    assert!(
        picker.entry.as_ref().is_some_and(|p| p.buffer.is_empty()),
        "entry buffer must not capture keystrokes while validating"
    );
}
