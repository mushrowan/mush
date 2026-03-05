//! keyboard input handling
//!
//! maps crossterm key events to app mutations and events

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, AppEvent, AppMode};

// A trailing '\' escapes plain Enter into a literal newline, but only when the
// cursor is at the end of the current line.
fn should_escape_enter_to_newline(app: &App, key: KeyEvent) -> bool {
    if key.code != KeyCode::Enter || !key.modifiers.is_empty() || app.cursor == 0 {
        return false;
    }

    let current_line = &app.input[..app.cursor];
    let Some(last_char) = current_line.chars().next_back() else {
        return false;
    };
    if last_char != '\\' {
        return false;
    }

    app.input[app.cursor..].starts_with('\n') || app.cursor == app.input.len()
}
/// handle a key event, mutating the app and optionally producing an event
pub fn handle_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    tracing::trace!(
        code = ?key.code,
        modifiers = ?key.modifiers,
        kind = ?key.kind,
        mode = ?app.mode,
        is_streaming = app.is_streaming,
        input_len = app.input.len(),
        cursor = app.cursor,
        "key event"
    );

    // session picker mode has its own key handling
    if app.mode == AppMode::SessionPicker {
        return handle_picker_key(app, key);
    }

    // slash command menu
    if app.mode == AppMode::SlashComplete {
        return handle_slash_menu_key(app, key);
    }

    // scroll mode: j/k scroll, y copies selected message, esc exits
    if app.mode == AppMode::Scroll {
        return handle_scroll_mode(app, key);
    }

    // search mode
    if app.mode == AppMode::Search {
        return handle_search_mode(app, key);
    }

    // global bindings (work even while streaming)
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Some(AppEvent::Quit),
        (_, KeyCode::Esc) if app.is_streaming => return Some(AppEvent::Abort),
        (_, KeyCode::Esc) if app.scroll_offset > 0 => {
            app.scroll_to_bottom();
            return None;
        }
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
            if app.scroll_offset == 0 {
                app.has_unread = false;
            }
            return Some(AppEvent::ScrollDown(10));
        }
        _ => {}
    }

    // while streaming, allow typing + submission (queued as steering messages
    // by the runner) but block mode switches and slash commands
    if app.is_streaming {
        match (key.modifiers, key.code) {
            // multi-line
            (KeyModifiers::ALT | KeyModifiers::SHIFT, KeyCode::Enter)
            | (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
                app.input_char('\n');
            }
            // submit to steering queue (slash commands blocked during streaming)
            (_, KeyCode::Enter) => {
                if should_escape_enter_to_newline(app, key) {
                    app.input_backspace();
                    app.input_char('\n');
                    return None;
                }
                if app.input.trim().is_empty() {
                    return None;
                }
                let text = app.take_input();
                if text.starts_with('/') {
                    app.input = text;
                    app.cursor = app.input.len();
                    app.ensure_cursor_visible();
                    app.status = Some("slash commands unavailable while streaming".into());
                    return None;
                }
                return Some(AppEvent::UserSubmit { text });
            }
            // editing
            (KeyModifiers::CONTROL, KeyCode::Backspace)
            | (KeyModifiers::ALT, KeyCode::Backspace)
            | (KeyModifiers::CONTROL, KeyCode::Char('w')) => app.delete_word_backward(),
            (KeyModifiers::ALT, KeyCode::Char('d')) => app.delete_word_forward(),
            (_, KeyCode::Backspace) => app.input_backspace(),
            (_, KeyCode::Delete) => app.input_delete(),
            // cursor movement
            (KeyModifiers::ALT, KeyCode::Left)
            | (KeyModifiers::CONTROL, KeyCode::Left)
            | (KeyModifiers::ALT, KeyCode::Char('b')) => app.cursor_word_left(),
            (KeyModifiers::ALT, KeyCode::Right)
            | (KeyModifiers::CONTROL, KeyCode::Right)
            | (KeyModifiers::ALT, KeyCode::Char('f')) => app.cursor_word_right(),
            (KeyModifiers::CONTROL, KeyCode::Char('b')) => app.cursor_left(),
            (_, KeyCode::Left) => app.cursor_left(),
            (_, KeyCode::Right) => app.cursor_right(),
            (_, KeyCode::Home) | (KeyModifiers::CONTROL, KeyCode::Char('a')) => app.cursor_home(),
            (_, KeyCode::End) | (KeyModifiers::CONTROL, KeyCode::Char('e')) => app.cursor_end(),
            // line editing
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => app.delete_to_start(),
            (KeyModifiers::CONTROL, KeyCode::Char('k')) => app.delete_to_end(),
            // regular character
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                app.input_char(c);
            }
            _ => {}
        }
        return None;
    }

    match (key.modifiers, key.code) {
        // search
        (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
            app.mode = AppMode::Search;
            app.search.query.clear();
            app.search.matches.clear();
            app.search.selected = 0;
            return None;
        }

        // scroll/copy mode
        (KeyModifiers::CONTROL, KeyCode::Char('s')) => {
            app.mode = AppMode::Scroll;
            // select the last message by default
            if !app.messages.is_empty() {
                app.selected_message = Some(app.messages.len() - 1);
            }
            return None;
        }

        // tab completion / slash menu
        (_, KeyCode::Tab) | (KeyModifiers::SHIFT, KeyCode::BackTab) => {
            // open slash menu if typing a command and we have descriptions
            if app.input.starts_with('/')
                && app.slash_menu.is_none()
                && !app.slash_commands.is_empty()
            {
                app.open_slash_menu();
            } else {
                app.tab_complete();
            }
            return None;
        }

        // multi-line: alt+enter, shift+enter, or ctrl+j inserts newline
        (KeyModifiers::ALT | KeyModifiers::SHIFT, KeyCode::Enter)
        | (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
            app.input_char('\n');
            return None;
        }

        // submit (accept ghost text if present)
        (_, KeyCode::Enter) => {
            if should_escape_enter_to_newline(app, key) {
                app.input_backspace();
                app.input_char('\n');
                return None;
            }
            // accept ghost completion before submitting
            if let Some(suffix) = app.ghost_text().map(|s| s.to_string()) {
                app.input.push_str(&suffix);
                app.cursor = app.input.len();
                app.ensure_cursor_visible();
            }
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

        // cursor movement (specific modifiers before wildcards)
        (KeyModifiers::ALT, KeyCode::Left)
        | (KeyModifiers::CONTROL, KeyCode::Left)
        | (KeyModifiers::ALT, KeyCode::Char('b')) => app.cursor_word_left(),
        (KeyModifiers::ALT, KeyCode::Right)
        | (KeyModifiers::CONTROL, KeyCode::Right)
        | (KeyModifiers::ALT, KeyCode::Char('f')) => app.cursor_word_right(),
        (KeyModifiers::CONTROL, KeyCode::Char('b')) => app.cursor_left(),
        (_, KeyCode::Left) => app.cursor_left(),
        (_, KeyCode::Right) => app.cursor_right(),
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

        // toggle prompt injection visibility
        (KeyModifiers::CONTROL, KeyCode::Char('i')) => {
            app.show_prompt_injection = !app.show_prompt_injection;
            app.status = Some(if app.show_prompt_injection {
                "prompt injection visible".into()
            } else {
                "prompt injection hidden".into()
            });
        }

        // paste image from clipboard
        (KeyModifiers::CONTROL, KeyCode::Char('v')) => {
            return Some(AppEvent::PasteImage);
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
fn handle_search_mode(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Some(AppEvent::Quit),
        (_, KeyCode::Esc) | (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
            app.mode = AppMode::Normal;
            app.search.query.clear();
            app.search.matches.clear();
        }
        (_, KeyCode::Enter) => {
            // jump to selected match and enter scroll mode
            if let Some(&idx) = app.search.matches.get(app.search.selected) {
                app.mode = AppMode::Scroll;
                app.selected_message = Some(idx);
                app.search.query.clear();
                app.search.matches.clear();
            } else {
                app.mode = AppMode::Normal;
            }
        }
        (_, KeyCode::Up)
        | (KeyModifiers::CONTROL, KeyCode::Char('p'))
        | (KeyModifiers::CONTROL, KeyCode::Char('k'))
            if !app.search.matches.is_empty() =>
        {
            app.search.selected = app.search.selected.saturating_sub(1);
        }
        (_, KeyCode::Down)
        | (KeyModifiers::CONTROL, KeyCode::Char('n'))
        | (KeyModifiers::CONTROL, KeyCode::Char('j'))
            if !app.search.matches.is_empty()
                && app.search.selected + 1 < app.search.matches.len() =>
        {
            app.search.selected += 1;
        }
        (_, KeyCode::Backspace) => {
            app.search.query.pop();
            app.update_search();
        }
        (_, KeyCode::Char(c)) => {
            app.search.query.push(c);
            app.update_search();
        }
        _ => {}
    }
    None
}

fn handle_scroll_mode(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Some(AppEvent::Quit),
        (_, KeyCode::Esc) | (KeyModifiers::CONTROL, KeyCode::Char('s')) => {
            app.mode = AppMode::Normal;
            app.selected_message = None;
        }
        (_, KeyCode::Char('j')) | (_, KeyCode::Down) => {
            app.scroll_offset = app.scroll_offset.saturating_sub(3);
            if app.scroll_offset == 0 {
                app.has_unread = false;
            }
            // move selection down
            if let Some(sel) = app.selected_message
                && sel + 1 < app.messages.len()
            {
                app.selected_message = Some(sel + 1);
            }
        }
        (_, KeyCode::Char('k')) | (_, KeyCode::Up) => {
            app.scroll_offset = app.scroll_offset.saturating_add(3);
            // move selection up
            if let Some(sel) = app.selected_message {
                app.selected_message = Some(sel.saturating_sub(1));
            }
        }
        (_, KeyCode::Char('G')) => {
            app.scroll_to_bottom();
            if !app.messages.is_empty() {
                app.selected_message = Some(app.messages.len() - 1);
            }
        }
        (_, KeyCode::Char('g')) => {
            // scroll to top
            app.scroll_offset = u16::MAX;
            app.selected_message = Some(0);
        }
        (_, KeyCode::Char('y')) => {
            // copy selected message content to clipboard
            if let Some(sel) = app.selected_message
                && let Some(msg) = app.messages.get(sel)
            {
                let text = &msg.content;
                if copy_to_clipboard(text) {
                    app.status = Some("copied to clipboard".into());
                } else {
                    app.status = Some("clipboard copy failed".into());
                }
            }
        }
        _ => {}
    }
    None
}

/// copy text to system clipboard using platform tools
fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};

    // try each clipboard tool in order
    let tools: &[&[&str]] = &[
        &["pbcopy"],                           // macOS
        &["wl-copy"],                          // wayland
        &["xclip", "-selection", "clipboard"], // x11
        &["xsel", "--clipboard", "--input"],   // x11 alt
    ];

    for tool in tools {
        if let Ok(mut child) = Command::new(tool[0])
            .args(&tool[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            && let Some(mut stdin) = child.stdin.take()
            && stdin.write_all(text.as_bytes()).is_ok()
        {
            drop(stdin);
            return child.wait().map(|s| s.success()).unwrap_or(false);
        }
    }
    false
}

fn handle_slash_menu_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) | (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            app.close_slash_menu();
            None
        }
        // select the highlighted command
        (_, KeyCode::Enter) | (_, KeyCode::Tab) => {
            if let Some(ref menu) = app.slash_menu {
                if menu.model_mode {
                    if let Some(model) = menu.model_matches.get(menu.selected) {
                        app.input = format!("/model {}", model.id);
                        app.cursor = app.input.len();
                        app.ensure_cursor_visible();
                    }
                } else if let Some(cmd) = menu.matches.get(menu.selected) {
                    app.input = format!("/{}", cmd.name);
                    app.cursor = app.input.len();
                    app.ensure_cursor_visible();
                }
            }
            app.close_slash_menu();
            None
        }
        // navigate
        (_, KeyCode::Up) | (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
            if let Some(ref mut menu) = app.slash_menu {
                menu.selected = menu.selected.saturating_sub(1);
            }
            None
        }
        (_, KeyCode::Down) | (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
            if let Some(ref mut menu) = app.slash_menu {
                let len = if menu.model_mode {
                    menu.model_matches.len()
                } else {
                    menu.matches.len()
                };
                if menu.selected + 1 < len {
                    menu.selected += 1;
                }
            }
            None
        }
        // backspace: edit input and update filter
        (_, KeyCode::Backspace) => {
            app.input_backspace();
            if app.input.is_empty() || !app.input.starts_with('/') {
                app.close_slash_menu();
            } else {
                app.update_slash_menu();
            }
            None
        }
        // typing narrows the filter
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
            app.input_char(c);
            app.update_slash_menu();
            None
        }
        _ => None,
    }
}

fn handle_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) | (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            app.close_session_picker();
            None
        }
        (_, KeyCode::Tab) | (_, KeyCode::BackTab) => {
            // toggle scope between this dir and all dirs
            if let Some(ref mut picker) = app.session_picker {
                picker.scope = match picker.scope {
                    crate::app::SessionScope::ThisDir => crate::app::SessionScope::AllDirs,
                    crate::app::SessionScope::AllDirs => crate::app::SessionScope::ThisDir,
                };
                picker.selected = 0;
            }
            None
        }
        (_, KeyCode::Enter) => {
            // select session — return a slash command that the runner handles
            if let Some(meta) = app.selected_session() {
                let id = meta.id.to_string();
                app.close_session_picker();
                Some(AppEvent::SlashCommand {
                    name: "resume".into(),
                    args: id,
                })
            } else {
                None
            }
        }
        (_, KeyCode::Up) | (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
            if let Some(ref mut picker) = app.session_picker {
                picker.selected = picker.selected.saturating_sub(1);
            }
            None
        }
        (_, KeyCode::Down) | (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
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
        let mut app = App::new("test".into(), 200_000);
        let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
        assert!(matches!(event, Some(AppEvent::Quit)));
    }

    #[test]
    fn escape_aborts_when_streaming() {
        let mut app = App::new("test".into(), 200_000);
        app.is_streaming = true;
        let event = handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(event, Some(AppEvent::Abort)));
    }

    #[test]
    fn escape_does_nothing_when_idle() {
        let mut app = App::new("test".into(), 200_000);
        let event = handle_key(&mut app, key(KeyCode::Esc));
        assert!(event.is_none());
    }

    #[test]
    fn typing_characters() {
        let mut app = App::new("test".into(), 200_000);
        handle_key(&mut app, key(KeyCode::Char('h')));
        handle_key(&mut app, key(KeyCode::Char('i')));
        assert_eq!(app.input, "hi");
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn enter_submits_input() {
        let mut app = App::new("test".into(), 200_000);
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
        let mut app = App::new("test".into(), 200_000);
        let event = handle_key(&mut app, key(KeyCode::Enter));
        assert!(event.is_none());
    }

    #[test]
    fn backspace_deletes() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "abc".into();
        app.cursor = 3;
        handle_key(&mut app, key(KeyCode::Backspace));
        assert_eq!(app.input, "ab");
    }

    #[test]
    fn ctrl_u_clears_line() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "long text here".into();
        app.cursor = 14;
        handle_key(&mut app, ctrl(KeyCode::Char('u')));
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn home_end_navigation() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello".into();
        app.cursor = 3;

        handle_key(&mut app, key(KeyCode::Home));
        assert_eq!(app.cursor, 0);

        handle_key(&mut app, key(KeyCode::End));
        assert_eq!(app.cursor, 5);
    }

    #[test]
    fn page_up_down_scrolls() {
        let mut app = App::new("test".into(), 200_000);
        let event = handle_key(&mut app, key(KeyCode::PageUp));
        assert!(matches!(event, Some(AppEvent::ScrollUp(10))));
        assert_eq!(app.scroll_offset, 10);

        let event = handle_key(&mut app, key(KeyCode::PageDown));
        assert!(matches!(event, Some(AppEvent::ScrollDown(10))));
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn ctrl_t_cycles_thinking_level() {
        let mut app = App::new("test".into(), 200_000);
        assert_eq!(app.thinking_level, ThinkingLevel::Off);

        let event = handle_key(&mut app, ctrl(KeyCode::Char('t')));
        assert!(matches!(event, Some(AppEvent::CycleThinkingLevel)));
        assert_eq!(app.thinking_level, ThinkingLevel::Minimal);

        handle_key(&mut app, ctrl(KeyCode::Char('t')));
        assert_eq!(app.thinking_level, ThinkingLevel::Low);
    }

    #[test]
    fn ctrl_o_toggles_thinking_expanded() {
        let mut app = App::new("test".into(), 200_000);
        app.start_streaming();
        app.push_thinking_delta("thoughts");
        app.push_text_delta("text");
        app.finish_streaming(None, None);

        // starts expanded (default ThinkingDisplay::Expanded)
        assert!(app.messages[0].thinking_expanded);
        handle_key(&mut app, ctrl(KeyCode::Char('o')));
        assert!(!app.messages[0].thinking_expanded);
    }

    #[test]
    fn ctrl_i_toggles_prompt_injection_preview() {
        let mut app = App::new("test".into(), 200_000);
        assert!(!app.show_prompt_injection);
        handle_key(&mut app, ctrl(KeyCode::Char('i')));
        assert!(app.show_prompt_injection);
        assert_eq!(app.status.as_deref(), Some("prompt injection visible"));

        handle_key(&mut app, ctrl(KeyCode::Char('i')));
        assert!(!app.show_prompt_injection);
        assert_eq!(app.status.as_deref(), Some("prompt injection hidden"));
    }

    #[test]
    fn alt_enter_inserts_newline() {
        let mut app = App::new("test".into(), 200_000);
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
    fn ctrl_j_inserts_newline() {
        let mut app = App::new("test".into(), 200_000);
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
        assert_eq!(app.input, "a\n");
    }

    #[test]
    fn slash_command_parsed() {
        let mut app = App::new("test".into(), 200_000);
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
        let mut app = App::new("test".into(), 200_000);
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
    fn enter_after_trailing_backslash_inserts_newline() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello\\".into();
        app.cursor = app.input.len();

        let event = handle_key(&mut app, key(KeyCode::Enter));

        assert!(event.is_none());
        assert_eq!(app.input, "hello\n");
        assert_eq!(app.cursor, app.input.len());
    }

    #[test]
    fn enter_after_backslash_mid_input_does_not_submit() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello\\\nworld".into();
        app.cursor = "hello\\".len();

        let event = handle_key(&mut app, key(KeyCode::Enter));

        assert!(event.is_none());
        assert_eq!(app.input, "hello\n\nworld");
        assert_eq!(app.cursor, "hello\n".len());
    }

    #[test]
    fn ctrl_w_deletes_word_backward() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello world".into();
        app.cursor = 11;
        handle_key(&mut app, ctrl(KeyCode::Char('w')));
        assert_eq!(app.input, "hello ");
    }

    #[test]
    fn ctrl_k_deletes_to_end() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "hello world".into();
        app.cursor = 5;
        handle_key(&mut app, ctrl(KeyCode::Char('k')));
        assert_eq!(app.input, "hello");
    }

    #[test]
    fn ctrl_d_on_empty_quits() {
        let mut app = App::new("test".into(), 200_000);
        let event = handle_key(&mut app, ctrl(KeyCode::Char('d')));
        assert!(matches!(event, Some(AppEvent::Quit)));
    }

    #[test]
    fn ctrl_d_with_input_deletes_char() {
        let mut app = App::new("test".into(), 200_000);
        app.input = "abc".into();
        app.cursor = 1;
        handle_key(&mut app, ctrl(KeyCode::Char('d')));
        assert_eq!(app.input, "ac");
    }

    #[test]
    fn typing_allowed_while_streaming() {
        let mut app = App::new("test".into(), 200_000);
        app.is_streaming = true;
        handle_key(&mut app, key(KeyCode::Char('h')));
        handle_key(&mut app, key(KeyCode::Char('i')));
        assert_eq!(app.input, "hi");
    }

    #[test]
    fn submit_allowed_while_streaming() {
        let mut app = App::new("test".into(), 200_000);
        app.is_streaming = true;
        app.input = "steer this".into();
        app.cursor = 10;
        let event = handle_key(&mut app, key(KeyCode::Enter));
        match event {
            Some(AppEvent::UserSubmit { text }) => assert_eq!(text, "steer this"),
            other => panic!("expected UserSubmit, got {other:?}"),
        }
    }

    #[test]
    fn slash_commands_blocked_while_streaming() {
        let mut app = App::new("test".into(), 200_000);
        app.is_streaming = true;
        app.input = "/clear".into();
        app.cursor = 6;
        let event = handle_key(&mut app, key(KeyCode::Enter));
        assert!(event.is_none());
        // input preserved so user can submit after streaming ends
        assert_eq!(app.input, "/clear");
        assert_eq!(
            app.status.as_deref(),
            Some("slash commands unavailable while streaming")
        );
    }

    #[test]
    fn tab_completes_slash_commands() {
        let mut app = App::new("test".into(), 200_000);
        app.completions = vec!["/help".into(), "/history".into(), "/clear".into()];
        app.input = "/h".into();
        app.cursor = 2;
        handle_key(&mut app, key(KeyCode::Tab));
        assert_eq!(app.input, "/help");

        // second tab cycles to next match
        handle_key(&mut app, key(KeyCode::Tab));
        assert_eq!(app.input, "/history");

        // wraps around
        handle_key(&mut app, key(KeyCode::Tab));
        assert_eq!(app.input, "/help");
    }

    #[test]
    fn tab_completes_model_ids() {
        let mut app = App::new("test".into(), 200_000);
        app.completions = vec![
            "/model".into(),
            "claude-opus-4-6".into(),
            "claude-sonnet-4-20250514".into(),
        ];
        app.input = "/model claude-o".into();
        app.cursor = 15;
        handle_key(&mut app, key(KeyCode::Tab));
        assert_eq!(app.input, "/model claude-opus-4-6");
    }

    #[test]
    fn tab_no_match_does_nothing() {
        let mut app = App::new("test".into(), 200_000);
        app.completions = vec!["/help".into()];
        app.input = "/zzz".into();
        app.cursor = 4;
        handle_key(&mut app, key(KeyCode::Tab));
        assert_eq!(app.input, "/zzz");
    }

    #[test]
    fn tab_resets_on_typing() {
        let mut app = App::new("test".into(), 200_000);
        app.completions = vec!["/help".into(), "/history".into()];
        app.input = "/h".into();
        app.cursor = 2;
        handle_key(&mut app, key(KeyCode::Tab));
        assert_eq!(app.input, "/help");

        // typing resets completion state
        handle_key(&mut app, key(KeyCode::Char('x')));
        assert_eq!(app.input, "/helpx");
    }

    #[test]
    fn tab_ignored_for_non_slash_input() {
        let mut app = App::new("test".into(), 200_000);
        app.completions = vec!["/help".into()];
        app.input = "hello".into();
        app.cursor = 5;
        handle_key(&mut app, key(KeyCode::Tab));
        assert_eq!(app.input, "hello"); // unchanged
    }

    #[test]
    fn slash_menu_opens_on_tab_with_commands() {
        let mut app = App::new("test".into(), 200_000);
        app.slash_commands = vec![
            crate::app::SlashCommand {
                name: "help".into(),
                description: "show help".into(),
            },
            crate::app::SlashCommand {
                name: "clear".into(),
                description: "clear chat".into(),
            },
        ];
        app.input = "/".into();
        app.cursor = 1;
        handle_key(&mut app, key(KeyCode::Tab));
        assert_eq!(app.mode, AppMode::SlashComplete);
        assert!(app.slash_menu.is_some());
        let menu = app.slash_menu.as_ref().unwrap();
        assert!(!menu.model_mode);
        assert_eq!(menu.matches.len(), 2);
        assert_eq!(menu.selected, 0);
    }

    #[test]
    fn slash_menu_navigate_and_select() {
        let mut app = App::new("test".into(), 200_000);
        app.slash_commands = vec![
            crate::app::SlashCommand {
                name: "help".into(),
                description: "show help".into(),
            },
            crate::app::SlashCommand {
                name: "clear".into(),
                description: "clear chat".into(),
            },
        ];
        app.input = "/".into();
        app.cursor = 1;
        handle_key(&mut app, key(KeyCode::Tab));

        // ctrl+j moves down
        handle_key(&mut app, ctrl(KeyCode::Char('j')));
        assert_eq!(app.slash_menu.as_ref().unwrap().selected, 1);

        // ctrl+k moves up
        handle_key(&mut app, ctrl(KeyCode::Char('k')));
        assert_eq!(app.slash_menu.as_ref().unwrap().selected, 0);

        // enter selects
        handle_key(&mut app, key(KeyCode::Enter));
        assert_eq!(app.mode, AppMode::Normal);
        assert_eq!(app.input, "/help");
    }

    #[test]
    fn slash_menu_typing_filters() {
        let mut app = App::new("test".into(), 200_000);
        app.slash_commands = vec![
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
        app.input = "/h".into();
        app.cursor = 2;
        handle_key(&mut app, key(KeyCode::Tab));

        // only /h* commands match
        let menu = app.slash_menu.as_ref().unwrap();
        assert_eq!(menu.matches.len(), 2);

        // type 'e' to narrow to /he*
        handle_key(&mut app, key(KeyCode::Char('e')));
        let menu = app.slash_menu.as_ref().unwrap();
        assert_eq!(menu.matches.len(), 1);
        assert_eq!(menu.matches[0].name, "help");
    }

    #[test]
    fn slash_menu_opens_for_model_subcommand() {
        let mut app = App::new("test".into(), 200_000);
        app.slash_commands = vec![crate::app::SlashCommand {
            name: "model".into(),
            description: "show or switch model".into(),
        }];
        app.model_completions = vec![
            crate::app::ModelCompletion {
                id: "claude-opus-4-6".into(),
                name: "Claude Opus 4.6".into(),
            },
            crate::app::ModelCompletion {
                id: "claude-sonnet-4-20250514".into(),
                name: "Claude Sonnet 4".into(),
            },
        ];
        app.input = "/model claude".into();
        app.cursor = app.input.len();

        handle_key(&mut app, key(KeyCode::Tab));

        let menu = app.slash_menu.as_ref().unwrap();
        assert!(menu.model_mode);
        assert_eq!(menu.model_matches.len(), 2);
    }

    #[test]
    fn slash_menu_selects_model_completion() {
        let mut app = App::new("test".into(), 200_000);
        app.slash_commands = vec![crate::app::SlashCommand {
            name: "model".into(),
            description: "show or switch model".into(),
        }];
        app.model_completions = vec![
            crate::app::ModelCompletion {
                id: "claude-opus-4-6".into(),
                name: "Claude Opus 4.6".into(),
            },
            crate::app::ModelCompletion {
                id: "claude-sonnet-4-20250514".into(),
                name: "Claude Sonnet 4".into(),
            },
        ];
        app.input = "/model claude".into();
        app.cursor = app.input.len();

        handle_key(&mut app, key(KeyCode::Tab));
        handle_key(&mut app, ctrl(KeyCode::Char('j')));
        handle_key(&mut app, key(KeyCode::Enter));

        assert_eq!(app.input, "/model claude-sonnet-4-20250514");
    }

    #[test]
    fn slash_menu_typing_filters_model_matches() {
        let mut app = App::new("test".into(), 200_000);
        app.slash_commands = vec![crate::app::SlashCommand {
            name: "model".into(),
            description: "show or switch model".into(),
        }];
        app.model_completions = vec![
            crate::app::ModelCompletion {
                id: "claude-opus-4-6".into(),
                name: "Claude Opus 4.6".into(),
            },
            crate::app::ModelCompletion {
                id: "claude-sonnet-4-20250514".into(),
                name: "Claude Sonnet 4".into(),
            },
        ];
        app.input = "/model claude-".into();
        app.cursor = app.input.len();

        handle_key(&mut app, key(KeyCode::Tab));
        handle_key(&mut app, key(KeyCode::Char('o')));

        let menu = app.slash_menu.as_ref().unwrap();
        assert!(menu.model_mode);
        assert_eq!(menu.model_matches.len(), 1);
        assert_eq!(menu.model_matches[0].id, "claude-opus-4-6");
    }

    #[test]
    fn input_reinsert_while_streaming_keeps_cursor_visible() {
        let mut app = App::new("test".into(), 200_000);
        app.is_streaming = true;
        app.input_area.set(ratatui::layout::Rect::new(0, 0, 20, 12));
        app.input_visible_lines.set(2);
        app.input_total_lines.set(8);
        app.input_scroll.set(0);
        app.input = "/model a\nb\nc\nd".into();
        app.cursor = app.input.len();

        handle_key(&mut app, key(KeyCode::Enter));

        assert!(app.input_scroll.get() > 0);
    }

    #[test]
    fn slash_menu_esc_closes() {
        let mut app = App::new("test".into(), 200_000);
        app.slash_commands = vec![crate::app::SlashCommand {
            name: "help".into(),
            description: "show help".into(),
        }];
        app.input = "/".into();
        app.cursor = 1;
        handle_key(&mut app, key(KeyCode::Tab));
        assert_eq!(app.mode, AppMode::SlashComplete);

        handle_key(&mut app, key(KeyCode::Esc));
        assert_eq!(app.mode, AppMode::Normal);
        assert!(app.slash_menu.is_none());
    }
}
