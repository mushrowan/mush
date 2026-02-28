//! keyboard input handling
//!
//! maps crossterm key events to app mutations and events

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, AppEvent};

/// handle a key event, mutating the app and optionally producing an event
pub fn handle_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
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

        // editing
        (_, KeyCode::Backspace) => app.input_backspace(),
        (_, KeyCode::Delete) => app.input_delete(),
        (_, KeyCode::Left) => app.cursor_left(),
        (_, KeyCode::Right) => app.cursor_right(),
        (_, KeyCode::Home) | (KeyModifiers::CONTROL, KeyCode::Char('a')) => app.cursor_home(),
        (_, KeyCode::End) | (KeyModifiers::CONTROL, KeyCode::Char('e')) => app.cursor_end(),

        // toggle thinking
        (KeyModifiers::CONTROL, KeyCode::Char('t')) => {
            app.toggle_thinking();
        }

        // clear line
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
            app.input.clear();
            app.cursor = 0;
        }

        // regular character
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
            app.input_char(c);
        }

        _ => {}
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

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
    fn ctrl_t_toggles_thinking() {
        let mut app = App::new("test".into());
        app.start_streaming();
        app.push_thinking_delta("thoughts");
        app.push_text_delta("text");
        app.finish_streaming(None, None);

        assert!(!app.messages[0].thinking_expanded);
        handle_key(&mut app, ctrl(KeyCode::Char('t')));
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
    fn input_blocked_while_streaming() {
        let mut app = App::new("test".into());
        app.is_streaming = true;
        handle_key(&mut app, key(KeyCode::Char('x')));
        assert!(app.input.is_empty()); // typing ignored
    }
}
