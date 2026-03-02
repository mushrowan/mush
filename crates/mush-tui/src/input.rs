//! keyboard input handling
//!
//! maps crossterm key events to app mutations and events

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, AppEvent, AppMode};

/// handle a key event, mutating the app and optionally producing an event
pub fn handle_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    // session picker mode has its own key handling
    if app.mode == AppMode::SessionPicker {
        return handle_picker_key(app, key);
    }

    // global bindings (work even while streaming)
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Some(AppEvent::Quit),
        (_, KeyCode::Esc) if app.is_streaming => return Some(AppEvent::Abort),
        _ => {}
    }

    // scroll bindings
    match key.code {
        KeyCode::PageUp => {
            app.scroll_offset = app.scroll_offset.saturating_add(10);
            return Some(AppEvent::ScrollUp(10));
        }
        KeyCode::PageDown => {
            app.scroll_offset = app.scroll_offset.saturating_sub(10);
            return Some(AppEvent::ScrollDown(10));
        }
        _ => {}
    }

    // don't accept input while streaming
    if app.is_streaming {
        return None;
    }

    match (key.modifiers, key.code) {
        // multi-line: alt+enter or shift+enter inserts newline
        (KeyModifiers::ALT | KeyModifiers::SHIFT, KeyCode::Enter) => {
            app.input_char('\n');
            return None;
        }

        // submit
        (_, KeyCode::Enter) => {
            if app.input.trim().is_empty() {
                return None;
            }
            let text = app.take_input();

            // check for slash commands
            if let Some(rest) = text.strip_prefix('/') {
                let parts: Vec<&str> = rest.splitn(2, ' ').collect();
                let name = parts[0].to_string();
                let args = parts.get(1).unwrap_or(&"").to_string();
                return Some(AppEvent::SlashCommand { name, args });
            }

            return Some(AppEvent::UserSubmit { text });
        }

        // word deletion
        (KeyModifiers::CONTROL, KeyCode::Backspace)
        | (KeyModifiers::ALT, KeyCode::Backspace)
        | (KeyModifiers::CONTROL, KeyCode::Char('w')) => app.delete_word_backward(),
        (KeyModifiers::ALT, KeyCode::Char('d')) => app.delete_word_forward(),

        // editing
        (_, KeyCode::Backspace) => app.input_backspace(),
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
            if app.input.is_empty() {
                return Some(AppEvent::Quit);
            }
            app.input_delete();
        }
        (_, KeyCode::Delete) => app.input_delete(),

        // cursor movement
        (KeyModifiers::CONTROL, KeyCode::Char('b')) => app.cursor_left(),
        (_, KeyCode::Left) => app.cursor_left(),
        (KeyModifiers::CONTROL, KeyCode::Char('f')) => app.cursor_right(),
        (_, KeyCode::Right) => app.cursor_right(),
        (KeyModifiers::ALT, KeyCode::Left)
        | (KeyModifiers::CONTROL, KeyCode::Left)
        | (KeyModifiers::ALT, KeyCode::Char('b')) => app.cursor_word_left(),
        (KeyModifiers::ALT, KeyCode::Right)
        | (KeyModifiers::CONTROL, KeyCode::Right)
        | (KeyModifiers::ALT, KeyCode::Char('f')) => app.cursor_word_right(),
        (_, KeyCode::Home) | (KeyModifiers::CONTROL, KeyCode::Char('a')) => app.cursor_home(),
        (_, KeyCode::End) | (KeyModifiers::CONTROL, KeyCode::Char('e')) => app.cursor_end(),

        // line editing
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => app.delete_to_start(),
        (KeyModifiers::CONTROL, KeyCode::Char('k')) => app.delete_to_end(),

        // cycle thinking level
        (KeyModifiers::CONTROL, KeyCode::Char('t')) => {
            app.cycle_thinking_level();
            return Some(AppEvent::CycleThinkingLevel);
        }

        // toggle thinking text visibility
        (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
            app.toggle_thinking_expanded();
        }

        // regular character
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
            app.input_char(c);
        }

        _ => {}
    }

    None
}

/// handle keys in session picker mode
fn handle_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) | (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            app.close_session_picker();
            None
        }
        (_, KeyCode::Enter) => {
            // select session — return a slash command that the runner handles
            if let Some(meta) = app.selected_session() {
                let id = meta.id.0.clone();
                app.close_session_picker();
                Some(AppEvent::SlashCommand {
                    name: "resume".into(),
                    args: id,
                })
            } else {
                None
            }
        }
        (_, KeyCode::Up) => {
            if let Some(ref mut picker) = app.session_picker {
                picker.selected = picker.selected.saturating_sub(1);
            }
            None
        }
        (_, KeyCode::Down) => {
            if let Some(ref mut picker) = app.session_picker {
                let filtered_len = crate::app::filtered_sessions(picker).len();
                if picker.selected + 1 < filtered_len {
                    picker.selected += 1;
                }
            }
            None
        }
        (_, KeyCode::Backspace) => {
            if let Some(ref mut picker) = app.session_picker {
                picker.filter.pop();
                picker.selected = 0;
            }
            None
        }
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
            if let Some(ref mut picker) = app.session_picker {
                picker.filter.push(c);
                picker.selected = 0;
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use mush_ai::types::ThinkingLevel;

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
    fn ctrl_c_quits() {
        let mut app = App::new("test".into());
        let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
        assert!(matches!(event, Some(AppEvent::Quit)));
    }

    #[test]
    fn escape_aborts_when_streaming() {
        let mut app = App::new("test".into());
        app.is_streaming = true;
        let event = handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(event, Some(AppEvent::Abort)));
    }

    #[test]
    fn escape_does_nothing_when_idle() {
        let mut app = App::new("test".into());
        let event = handle_key(&mut app, key(KeyCode::Esc));
        assert!(event.is_none());
    }

    #[test]
    fn typing_characters() {
        let mut app = App::new("test".into());
        handle_key(&mut app, key(KeyCode::Char('h')));
        handle_key(&mut app, key(KeyCode::Char('i')));
        assert_eq!(app.input, "hi");
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn enter_submits_input() {
        let mut app = App::new("test".into());
        app.input = "hello".into();
        app.cursor = 5;
        let event = handle_key(&mut app, key(KeyCode::Enter));
        match event {
            Some(AppEvent::UserSubmit { text }) => assert_eq!(text, "hello"),
            other => panic!("expected UserSubmit, got {other:?}"),
        }
        assert!(app.input.is_empty());
    }

    #[test]
    fn enter_on_empty_does_nothing() {
        let mut app = App::new("test".into());
        let event = handle_key(&mut app, key(KeyCode::Enter));
        assert!(event.is_none());
    }

    #[test]
    fn backspace_deletes() {
        let mut app = App::new("test".into());
        app.input = "abc".into();
        app.cursor = 3;
        handle_key(&mut app, key(KeyCode::Backspace));
        assert_eq!(app.input, "ab");
    }

    #[test]
    fn ctrl_u_clears_line() {
        let mut app = App::new("test".into());
        app.input = "long text here".into();
        app.cursor = 14;
        handle_key(&mut app, ctrl(KeyCode::Char('u')));
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn home_end_navigation() {
        let mut app = App::new("test".into());
        app.input = "hello".into();
        app.cursor = 3;

        handle_key(&mut app, key(KeyCode::Home));
        assert_eq!(app.cursor, 0);

        handle_key(&mut app, key(KeyCode::End));
        assert_eq!(app.cursor, 5);
    }

    #[test]
    fn page_up_down_scrolls() {
        let mut app = App::new("test".into());
        let event = handle_key(&mut app, key(KeyCode::PageUp));
        assert!(matches!(event, Some(AppEvent::ScrollUp(10))));
        assert_eq!(app.scroll_offset, 10);

        let event = handle_key(&mut app, key(KeyCode::PageDown));
        assert!(matches!(event, Some(AppEvent::ScrollDown(10))));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn ctrl_t_cycles_thinking_level() {
        let mut app = App::new("test".into());
        assert_eq!(app.thinking_level, ThinkingLevel::Off);

        let event = handle_key(&mut app, ctrl(KeyCode::Char('t')));
        assert!(matches!(event, Some(AppEvent::CycleThinkingLevel)));
        assert_eq!(app.thinking_level, ThinkingLevel::Minimal);

        handle_key(&mut app, ctrl(KeyCode::Char('t')));
        assert_eq!(app.thinking_level, ThinkingLevel::Low);
    }

    #[test]
    fn ctrl_o_toggles_thinking_expanded() {
        let mut app = App::new("test".into());
        app.start_streaming();
        app.push_thinking_delta("thoughts");
        app.push_text_delta("text");
        app.finish_streaming(None, None);

        assert!(!app.messages[0].thinking_expanded);
        handle_key(&mut app, ctrl(KeyCode::Char('o')));
        assert!(app.messages[0].thinking_expanded);
    }

    #[test]
    fn alt_enter_inserts_newline() {
        let mut app = App::new("test".into());
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
        assert_eq!(app.input, "a\n");
    }

    #[test]
    fn slash_command_parsed() {
        let mut app = App::new("test".into());
        app.input = "/help".into();
        app.cursor = 5;
        let event = handle_key(&mut app, key(KeyCode::Enter));
        match event {
            Some(AppEvent::SlashCommand { name, args }) => {
                assert_eq!(name, "help");
                assert!(args.is_empty());
            }
            other => panic!("expected SlashCommand, got {other:?}"),
        }
    }

    #[test]
    fn slash_command_with_args() {
        let mut app = App::new("test".into());
        app.input = "/review src/main.rs".into();
        app.cursor = 19;
        let event = handle_key(&mut app, key(KeyCode::Enter));
        match event {
            Some(AppEvent::SlashCommand { name, args }) => {
                assert_eq!(name, "review");
                assert_eq!(args, "src/main.rs");
            }
            other => panic!("expected SlashCommand, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_w_deletes_word_backward() {
        let mut app = App::new("test".into());
        app.input = "hello world".into();
        app.cursor = 11;
        handle_key(&mut app, ctrl(KeyCode::Char('w')));
        assert_eq!(app.input, "hello ");
    }

    #[test]
    fn ctrl_k_deletes_to_end() {
        let mut app = App::new("test".into());
        app.input = "hello world".into();
        app.cursor = 5;
        handle_key(&mut app, ctrl(KeyCode::Char('k')));
        assert_eq!(app.input, "hello");
    }

    #[test]
    fn ctrl_d_on_empty_quits() {
        let mut app = App::new("test".into());
        let event = handle_key(&mut app, ctrl(KeyCode::Char('d')));
        assert!(matches!(event, Some(AppEvent::Quit)));
    }

    #[test]
    fn ctrl_d_with_input_deletes_char() {
        let mut app = App::new("test".into());
        app.input = "abc".into();
        app.cursor = 1;
        handle_key(&mut app, ctrl(KeyCode::Char('d')));
        assert_eq!(app.input, "ac");
    }

    #[test]
    fn input_blocked_while_streaming() {
        let mut app = App::new("test".into());
        app.is_streaming = true;
        handle_key(&mut app, key(KeyCode::Char('x')));
        assert!(app.input.is_empty()); // typing ignored
    }
}
