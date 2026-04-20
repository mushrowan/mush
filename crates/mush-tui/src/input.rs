//! keyboard input handling
//!
//! maps crossterm key events to app mutations and events

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, AppEvent, AppMode};
use crate::session_picker::{SessionScope, filtered_sessions};

// A trailing '\' escapes plain Enter into a literal newline, but only when the
// cursor is at the end of the current line.
fn should_escape_enter_to_newline(app: &App, key: KeyEvent) -> bool {
    if key.code != KeyCode::Enter || !key.modifiers.is_empty() || app.input.cursor == 0 {
        return false;
    }

    let current_line = &app.input.text[..app.input.cursor];
    let Some(last_char) = current_line.chars().next_back() else {
        return false;
    };
    if last_char != '\\' {
        return false;
    }

    app.input.text[app.input.cursor..].starts_with('\n') || app.input.cursor == app.input.text.len()
}
/// handle a key event, mutating the app and optionally producing an event
///
/// dispatch layers (checked in order):
/// 1. mode-specific handlers (picker, slash menu, scroll, search)
/// 2. global keys (quit, abort, page scroll)
/// 3. pane management (focus, split, resize) - works in all states
/// 4. multiline enter (alt/shift+enter, ctrl+j)
/// 5. streaming-only or idle-only keys (submit behaviour, mode switches)
/// 6. shared editing (cursor, deletion, character input)
pub fn handle_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    tracing::trace!(
        code = ?key.code,
        modifiers = ?key.modifiers,
        kind = ?key.kind,
        mode = ?app.interaction.mode,
        is_streaming = app.stream.active,
        input_len = app.input.text.len(),
        cursor = app.input.cursor,
        "key event"
    );

    // 1. mode-specific dispatches
    if app.interaction.mode == AppMode::SessionPicker {
        return handle_picker_key(app, key);
    }
    if app.interaction.mode == AppMode::SlashComplete {
        return handle_slash_menu_key(app, key);
    }
    if app.interaction.mode == AppMode::Scroll {
        return handle_scroll_mode(app, key);
    }
    if app.interaction.mode == AppMode::Search {
        return handle_search_mode(app, key);
    }
    if app.interaction.mode == AppMode::Settings {
        return handle_settings_menu_key(app, key);
    }

    // 2. global bindings
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Some(AppEvent::Quit),
        // ctrl+v works in both idle and streaming so the user can stage
        // images for mid-turn steering without aborting the stream
        (KeyModifiers::CONTROL, KeyCode::Char('v')) => return Some(AppEvent::PasteImage),
        _ => {}
    }
    // scroll / cancel cascade: esc (or ctrl+[) first jumps to bottom if
    // scrolled up, only aborts once already anchored there. this applies
    // even while streaming so users can re-anchor without cancelling a
    // long turn. single check + internal branches avoids duplicate guards
    if is_cancel_key(key) {
        if app.scroll_offset > 0 {
            app.scroll_to_bottom();
            return None;
        }
        if app.stream.active {
            return Some(AppEvent::Abort);
        }
    }
    // dedicated "go to bottom" hotkey: configurable, default shift+End.
    // fires regardless of stream state and never aborts
    if app
        .keymap
        .matches(crate::keybinds::Action::JumpToBottom, key)
    {
        app.scroll_to_bottom();
        return None;
    }
    match key.code {
        KeyCode::PageUp => {
            app.scroll_offset = app.scroll_offset.saturating_add(10);
            return Some(AppEvent::ScrollUp(10));
        }
        KeyCode::PageDown => {
            app.scroll_offset = app.scroll_offset.saturating_sub(10);
            if app.scroll_offset == 0 {
                app.navigation.has_unread = false;
            }
            return Some(AppEvent::ScrollDown(10));
        }
        _ => {}
    }

    // 3. pane management (works regardless of streaming state)
    if let Some(event) = handle_pane_keys(key) {
        return Some(event);
    }

    // alt+m / alt+shift+m cycle through favourite models. works while
    // streaming so the user can hot-swap mid-turn without aborting.
    // bindings come from the [keys] config; see keybinds.rs for
    // cycle_favourite / cycle_favourite_backward
    if app
        .keymap
        .matches(crate::keybinds::Action::CycleFavouriteBackward, key)
    {
        if let Some(event) = cycle_favourite_model(app, -1) {
            return Some(event);
        }
        return None;
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::CycleFavourite, key)
    {
        if let Some(event) = cycle_favourite_model(app, 1) {
            return Some(event);
        }
        return None;
    }

    // 4. multiline enter (before mode-specific enter handling)
    match (key.modifiers, key.code) {
        (KeyModifiers::ALT | KeyModifiers::SHIFT, KeyCode::Enter)
        | (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
            app.input_char('\n');
            return None;
        }
        _ => {}
    }

    // 5. edit_steering action: lift the last queued steering message
    // back into the input for editing. the binding set is configurable
    // via [keys] in config.toml. to avoid clobbering real typing,
    // bindings without an Alt modifier require the input to be empty;
    // Alt-prefixed bindings fire unconditionally because alt combos
    // aren't produced by normal typing
    if app.has_queued_messages()
        && app
            .keymap
            .matches(crate::keybinds::Action::EditSteering, key)
    {
        let is_alt_combo = key.modifiers.contains(KeyModifiers::ALT);
        if is_alt_combo || app.input.text.is_empty() {
            return Some(AppEvent::EditSteering);
        }
    }

    // 6. streaming vs idle dispatch
    if app.stream.active {
        handle_streaming_keys(app, key)
    } else {
        handle_idle_keys(app, key)
    }
}

/// pane management bindings, independent of streaming state
fn handle_pane_keys(key: KeyEvent) -> Option<AppEvent> {
    match (key.modifiers, key.code) {
        (KeyModifiers::ALT, KeyCode::Char(c @ '1'..='9')) => {
            Some(AppEvent::FocusPaneByIndex((c as usize) - ('1' as usize)))
        }
        (m, KeyCode::Enter)
            if m.contains(KeyModifiers::CONTROL) && m.contains(KeyModifiers::SHIFT) =>
        {
            Some(AppEvent::SplitPane)
        }
        (m, KeyCode::Left)
            if m.contains(KeyModifiers::CONTROL) && m.contains(KeyModifiers::SHIFT) =>
        {
            Some(AppEvent::ResizePane(-4))
        }
        (m, KeyCode::Right)
            if m.contains(KeyModifiers::CONTROL) && m.contains(KeyModifiers::SHIFT) =>
        {
            Some(AppEvent::ResizePane(4))
        }
        (KeyModifiers::CONTROL, KeyCode::Tab) => Some(AppEvent::FocusNextPane),
        (m, KeyCode::BackTab) if m.contains(KeyModifiers::CONTROL) => Some(AppEvent::FocusPrevPane),
        _ => None,
    }
}

/// keys specific to streaming: enter submits to steering queue
fn handle_streaming_keys(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    if key.code == KeyCode::Enter {
        if should_escape_enter_to_newline(app, key) {
            app.input.backspace();
            app.input_char('\n');
            return None;
        }
        if app.input.text.trim().is_empty() {
            return None;
        }
        let text = app.input.take_text();
        if text.starts_with('/') {
            // try to parse. safe commands run immediately; unsafe ones get
            // rejected with input restored so the user can resubmit later
            match crate::slash::parse(&text) {
                Ok(action) if action.is_safe_during_stream() => {
                    return Some(AppEvent::SlashCommand { action });
                }
                Ok(_) => {
                    app.input.text = text;
                    app.input.cursor = app.input.text.len();
                    app.input.ensure_cursor_visible();
                    app.status = Some(
                        "this slash command is blocked while streaming; press esc to abort first"
                            .into(),
                    );
                    return None;
                }
                Err(error) => {
                    app.push_system_message(error.to_string());
                    return None;
                }
            }
        }
        return Some(AppEvent::UserSubmit { text });
    }

    handle_editing(app, key)
}

/// keys specific to idle: mode switches, toggles, submit with slash parsing
fn handle_idle_keys(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    // mode switches: bindings come from the [keys] config
    if app
        .keymap
        .matches(crate::keybinds::Action::EnterSearch, key)
    {
        app.interaction.mode = AppMode::Search;
        app.interaction.search.query.clear();
        app.interaction.search.matches.clear();
        app.interaction.search.selected = 0;
        return None;
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::EnterScroll, key)
    {
        app.interaction.mode = AppMode::Scroll;
        if !app.messages.is_empty() {
            app.navigation.selected_message = Some(app.messages.len() - 1);
        }
        // initialise block selection when entering in block mode
        if app.navigation.scroll_unit == crate::app::ScrollUnit::Block {
            let blocks = app.code_blocks();
            app.navigation.selected_block = if blocks.is_empty() {
                None
            } else {
                Some(blocks.len() - 1)
            };
        }
        return None;
    }

    match (key.modifiers, key.code) {
        // tab completion / slash menu
        (_, KeyCode::Tab) | (KeyModifiers::SHIFT, KeyCode::BackTab) => {
            // @-template expansion takes priority over generic tab completion
            // so users can type `@review<tab>` and get the template content.
            // bare `@` with no matching template falls through to normal
            // tab behaviour so typing `@ ` in code/paths still works
            if try_expand_at_template(app) {
                return None;
            }
            if app.input.text.starts_with('/')
                && app.completion.slash_menu.is_none()
                && !app.completion.slash_commands.is_empty()
            {
                app.open_slash_menu();
            } else {
                app.tab_complete();
            }
            None
        }

        // submit (accept ghost text, parse slash commands)
        (_, KeyCode::Enter) => {
            if should_escape_enter_to_newline(app, key) {
                app.input.backspace();
                app.input_char('\n');
                return None;
            }
            if let Some(suffix) = app.ghost_text().map(|s| s.to_string()) {
                app.input.text.push_str(&suffix);
                app.input.cursor = app.input.text.len();
                app.input.ensure_cursor_visible();
            }
            if app.input.text.trim().is_empty() {
                return None;
            }
            let text = app.input.take_text();
            if text.starts_with('/') {
                match crate::slash::parse(&text) {
                    Ok(action) => return Some(AppEvent::SlashCommand { action }),
                    Err(error) => {
                        app.push_system_message(error.to_string());
                        return None;
                    }
                }
            }
            Some(AppEvent::UserSubmit { text })
        }

        // ctrl+d: quit on empty, delete char otherwise
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
            if app.input.text.is_empty() {
                return Some(AppEvent::Quit);
            }
            app.input.delete();
            None
        }

        // toggles
        (KeyModifiers::CONTROL, KeyCode::Char('t')) => {
            app.cycle_thinking_level();
            Some(AppEvent::CycleThinkingLevel)
        }
        (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
            app.toggle_thinking_expanded();
            None
        }
        (KeyModifiers::CONTROL, KeyCode::Char('i')) => {
            app.interaction.show_prompt_injection = !app.interaction.show_prompt_injection;
            app.status = Some(if app.interaction.show_prompt_injection {
                "prompt injection visible".into()
            } else {
                "prompt injection hidden".into()
            });
            None
        }

        // everything else falls through to shared editing
        _ => handle_editing(app, key),
    }
}

/// try expanding an `@name` prompt template adjacent to the cursor.
///
/// returns `true` when the trigger was found and a matching template
/// got inserted (replacing the `@name` span in the input). returns
/// `false` when there's no trigger or no matching template, so the
/// caller can fall through to regular tab completion.
///
/// positional placeholders (`$1`, `$2`, `$@`) are inserted verbatim
/// for now. a follow-up cycle will add slot-editing with tab/shift+tab
fn try_expand_at_template(app: &mut App) -> bool {
    let Some(trigger) = crate::at_template::parse_at_trigger(&app.input.text, app.input.cursor)
    else {
        return false;
    };
    // disjoint-field borrow: &app.completion.templates and
    // &mut app.input.text don't alias, so no clone needed on content
    let Some(template) = crate::at_template::find_exact(&app.completion.templates, &trigger) else {
        return false;
    };
    let end = app.input.cursor;
    let new_cursor = trigger.start + template.content.len();
    app.input
        .text
        .replace_range(trigger.start..end, &template.content);
    app.input.cursor = new_cursor;
    app.input.ensure_cursor_visible();
    true
}

/// shared text editing bindings (cursor movement, deletion, character input)
fn handle_editing(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    match (key.modifiers, key.code) {
        // word deletion
        (KeyModifiers::CONTROL, KeyCode::Backspace)
        | (KeyModifiers::ALT, KeyCode::Backspace)
        | (KeyModifiers::CONTROL, KeyCode::Char('w')) => app.input.delete_word_backward(),
        (KeyModifiers::ALT, KeyCode::Char('d')) => app.input.delete_word_forward(),
        (_, KeyCode::Backspace) => app.input.backspace(),
        (_, KeyCode::Delete) => app.input.delete(),

        // cursor movement
        (KeyModifiers::ALT, KeyCode::Left)
        | (KeyModifiers::CONTROL, KeyCode::Left)
        | (KeyModifiers::ALT, KeyCode::Char('b')) => app.input.cursor_word_left(),
        (KeyModifiers::ALT, KeyCode::Right)
        | (KeyModifiers::CONTROL, KeyCode::Right)
        | (KeyModifiers::ALT, KeyCode::Char('f')) => app.input.cursor_word_right(),
        (KeyModifiers::CONTROL, KeyCode::Char('b')) => app.input.cursor_left(),
        (_, KeyCode::Left) => app.input.cursor_left(),
        (_, KeyCode::Right) => app.input.cursor_right(),
        (_, KeyCode::Home) | (KeyModifiers::CONTROL, KeyCode::Char('a')) => app.input.cursor_home(),
        (_, KeyCode::End) | (KeyModifiers::CONTROL, KeyCode::Char('e')) => app.input.cursor_end(),

        // line editing
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => app.input.delete_to_start(),
        (KeyModifiers::CONTROL, KeyCode::Char('k')) => app.input.delete_to_end(),

        // character input
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => app.input_char(c),

        _ => {}
    }
    None
}

/// handle keys in /settings overlay mode
fn handle_settings_menu_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    let Some(menu) = app.settings_menu.as_mut() else {
        app.interaction.mode = AppMode::Normal;
        return None;
    };
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) | (KeyModifiers::CONTROL, KeyCode::Char('c' | '[')) => {
            app.settings_menu = None;
            app.interaction.mode = AppMode::Normal;
        }
        (_, KeyCode::Down | KeyCode::Char('j')) => menu.move_down(),
        (_, KeyCode::Up | KeyCode::Char('k')) => menu.move_up(),
        (_, KeyCode::Home | KeyCode::Char('g')) => menu.top(),
        (_, KeyCode::End | KeyCode::Char('G')) => menu.bottom(),
        (_, KeyCode::Enter | KeyCode::Char(' ')) => {
            return Some(AppEvent::SettingsToggleSelected);
        }
        _ => {}
    }
    None
}

/// handle keys in session picker mode
fn handle_search_mode(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Some(AppEvent::Quit),
        (_, KeyCode::Esc) | (KeyModifiers::CONTROL, KeyCode::Char('f' | '[')) => {
            app.interaction.mode = AppMode::Normal;
            app.interaction.search.query.clear();
            app.interaction.search.matches.clear();
        }
        (_, KeyCode::Enter) => {
            // jump to selected match and enter scroll mode
            if let Some(&idx) = app
                .interaction
                .search
                .matches
                .get(app.interaction.search.selected)
            {
                app.interaction.mode = AppMode::Scroll;
                app.navigation.selected_message = Some(idx);
                app.interaction.search.query.clear();
                app.interaction.search.matches.clear();
            } else {
                app.interaction.mode = AppMode::Normal;
            }
        }
        (_, KeyCode::Up)
        | (KeyModifiers::CONTROL, KeyCode::Char('p'))
        | (KeyModifiers::CONTROL, KeyCode::Char('k'))
            if !app.interaction.search.matches.is_empty() =>
        {
            app.interaction.search.selected = app.interaction.search.selected.saturating_sub(1);
        }
        (_, KeyCode::Down)
        | (KeyModifiers::CONTROL, KeyCode::Char('n'))
        | (KeyModifiers::CONTROL, KeyCode::Char('j'))
            if !app.interaction.search.matches.is_empty()
                && app.interaction.search.selected + 1 < app.interaction.search.matches.len() =>
        {
            app.interaction.search.selected += 1;
        }
        (_, KeyCode::Backspace) => {
            app.interaction.search.query.pop();
            app.update_search();
        }
        (_, KeyCode::Char(c)) => {
            app.interaction.search.query.push(c);
            app.update_search();
        }
        _ => {}
    }
    None
}

fn handle_scroll_mode(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    use crate::app::ScrollUnit;

    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Some(AppEvent::Quit),
        (_, KeyCode::Esc) | (KeyModifiers::CONTROL, KeyCode::Char('s' | '[')) => {
            if app.navigation.selection_anchor.is_some() {
                // first esc clears selection, stays in scroll mode
                app.navigation.selection_anchor = None;
            } else {
                app.interaction.mode = AppMode::Normal;
                app.navigation.selected_message = None;
                app.navigation.selected_block = None;
            }
        }
        (_, KeyCode::Char('b')) => {
            app.navigation.scroll_unit = match app.navigation.scroll_unit {
                ScrollUnit::Message => {
                    // switching to block mode, initialise selection
                    let blocks = app.code_blocks();
                    app.navigation.selected_block = if blocks.is_empty() {
                        None
                    } else {
                        Some(blocks.len() - 1)
                    };
                    ScrollUnit::Block
                }
                ScrollUnit::Block => {
                    app.navigation.selected_block = None;
                    ScrollUnit::Message
                }
            };
        }
        (_, KeyCode::Char('v')) if app.navigation.scroll_unit == ScrollUnit::Message => {
            // toggle visual selection (message mode only)
            if app.navigation.selection_anchor.is_some() {
                app.navigation.selection_anchor = None;
            } else {
                app.navigation.selection_anchor = app.navigation.selected_message;
            }
        }
        (_, KeyCode::Char('j')) | (_, KeyCode::Down) => {
            let scroll_lines = app.scroll_lines;
            if app.navigation.scroll_unit == ScrollUnit::Block {
                // move to next code block
                let blocks = app.code_blocks();
                if let Some(sel) = app.navigation.selected_block
                    && sel + 1 < blocks.len()
                {
                    app.navigation.selected_block = Some(sel + 1);
                    app.navigation.selected_message = Some(blocks[sel + 1].msg_idx);
                    app.scroll_offset = app.scroll_offset.saturating_sub(scroll_lines);
                    if app.scroll_offset == 0 {
                        app.navigation.has_unread = false;
                    }
                }
            } else {
                // message mode: scroll down, advance selection only if the
                // next message is visible in the new viewport
                app.scroll_offset = app.scroll_offset.saturating_sub(scroll_lines);
                if app.scroll_offset == 0 {
                    app.navigation.has_unread = false;
                }
                if let Some(sel) = app.navigation.selected_message
                    && sel + 1 < app.messages.len()
                    && app.is_message_visible(sel + 1)
                {
                    app.navigation.selected_message = Some(sel + 1);
                }
            }
        }
        (_, KeyCode::Char('k')) | (_, KeyCode::Up) => {
            let scroll_lines = app.scroll_lines;
            if app.navigation.scroll_unit == ScrollUnit::Block {
                // move to previous code block
                let blocks = app.code_blocks();
                if let Some(sel) = app.navigation.selected_block {
                    let new_sel = sel.saturating_sub(1);
                    app.navigation.selected_block = Some(new_sel);
                    app.navigation.selected_message = Some(blocks[new_sel].msg_idx);
                    app.scroll_offset = app.scroll_offset.saturating_add(scroll_lines);
                }
            } else {
                // message mode: scroll up, move selection to previous message
                // only if it's visible in the new viewport
                app.scroll_offset = app.scroll_offset.saturating_add(scroll_lines);
                if let Some(sel) = app.navigation.selected_message {
                    let target = sel.saturating_sub(1);
                    if target != sel && app.is_message_visible(target) {
                        app.navigation.selected_message = Some(target);
                    }
                }
            }
        }
        (_, KeyCode::Char('G')) => {
            app.scroll_to_bottom();
            if app.navigation.scroll_unit == ScrollUnit::Block {
                let blocks = app.code_blocks();
                app.navigation.selected_block = if blocks.is_empty() {
                    None
                } else {
                    Some(blocks.len() - 1)
                };
            }
            if !app.messages.is_empty() {
                app.navigation.selected_message = Some(app.messages.len() - 1);
            }
        }
        (_, KeyCode::Char('g')) => {
            app.scroll_offset = u16::MAX;
            if app.navigation.scroll_unit == ScrollUnit::Block {
                let blocks = app.code_blocks();
                app.navigation.selected_block = if blocks.is_empty() { None } else { Some(0) };
            }
            app.navigation.selected_message = Some(0);
        }
        (_, KeyCode::Char('y')) => {
            if app.navigation.scroll_unit == ScrollUnit::Block {
                // copy selected code block
                let blocks = app.code_blocks();
                if let Some(sel) = app.navigation.selected_block
                    && let Some(block) = blocks.get(sel)
                {
                    if copy_to_clipboard(&block.content) {
                        let label = block
                            .lang
                            .as_deref()
                            .map(|l| format!("{l} block"))
                            .unwrap_or_else(|| "code block".into());
                        app.status = Some(format!(
                            "copied {label} ({} chars)",
                            block.content.chars().count()
                        ));
                    } else {
                        app.status = Some("clipboard copy failed".into());
                    }
                }
            } else if let Some((start, end)) = app.selection_range() {
                // copy visual selection range
                let text: String = app.messages[start..=end]
                    .iter()
                    .map(|m| m.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let count = end - start + 1;
                if copy_to_clipboard(&text) {
                    app.status = Some(format!(
                        "copied {count} messages ({} chars)",
                        text.chars().count()
                    ));
                } else {
                    app.status = Some("clipboard copy failed".into());
                }
                app.navigation.selection_anchor = None;
            } else if let Some(sel) = app.navigation.selected_message
                && let Some(msg) = app.messages.get(sel)
            {
                // copy single selected message
                if copy_to_clipboard(&msg.content) {
                    app.status = Some(format!(
                        "copied message ({} chars)",
                        msg.content.chars().count()
                    ));
                } else {
                    app.status = Some("clipboard copy failed".into());
                }
            }
        }
        _ => {}
    }
    None
}

/// copy text to system clipboard. uses OSC 52 primarily (works over SSH
/// and inside tmux with appropriate config), with shell-based fallbacks
/// for platforms or terminals that don't support OSC 52.
///
/// returns true if at least one method succeeded.
fn copy_to_clipboard(text: &str) -> bool {
    // OSC 52 emits an escape sequence the terminal emulator intercepts.
    // this works transparently over SSH because the escape reaches the
    // user's local terminal. kitty, iterm2, wezterm, ghostty, alacritty,
    // and tmux (with set-clipboard passthrough) all support it
    let osc52 = emit_osc52(text);
    // also try native clipboard tools for local sessions where the user
    // might prefer system clipboard integration (paste in non-terminal apps)
    let native = copy_via_shell(text);
    osc52 || native
}

fn emit_osc52(text: &str) -> bool {
    use crossterm::clipboard::CopyToClipboard;
    use crossterm::execute;

    let mut stdout = std::io::stdout();
    execute!(stdout, CopyToClipboard::to_clipboard_from(text)).is_ok()
}

fn copy_via_shell(text: &str) -> bool {
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
        (_, KeyCode::Esc) | (KeyModifiers::CONTROL, KeyCode::Char('c' | '[')) => {
            app.close_slash_menu();
            None
        }
        // enter accepts the highlighted entry and closes the menu
        (_, KeyCode::Enter) => {
            let event = if let Some(ref menu) = app.completion.slash_menu {
                if menu.model_mode {
                    menu.model_matches
                        .get(menu.selected)
                        .map(|model| AppEvent::ModelSelected {
                            model_id: model.id.clone(),
                        })
                } else if let Some(cmd) = menu.matches.get(menu.selected) {
                    app.input.text = format!("/{}", cmd.name);
                    app.input.cursor = app.input.text.len();
                    app.input.ensure_cursor_visible();
                    None
                } else {
                    None
                }
            } else {
                None
            };
            app.close_slash_menu();
            event
        }
        // tab: in command mode cycle selection forward, in model mode
        // toggle between all / favourites-only
        (_, KeyCode::Tab) => {
            let favourites_only = app
                .completion
                .slash_menu
                .as_ref()
                .is_some_and(|m| m.model_mode);
            if favourites_only {
                toggle_model_favourites_view(app);
            } else if let Some(ref mut menu) = app.completion.slash_menu {
                let len = menu_len(menu);
                if len > 0 {
                    menu.selected = (menu.selected + 1) % len;
                }
            }
            None
        }
        // shift+backtab cycles backward with wrap
        (KeyModifiers::SHIFT, KeyCode::BackTab) | (_, KeyCode::BackTab) => {
            if let Some(ref mut menu) = app.completion.slash_menu {
                let len = menu_len(menu);
                if len > 0 {
                    menu.selected = (menu.selected + len - 1) % len;
                }
            }
            None
        }
        // ctrl+f toggles favourite on the selected model (model mode only).
        // rejected with a toast when favourites are locked by config
        (KeyModifiers::CONTROL, KeyCode::Char('f')) => toggle_selected_favourite(app),
        // navigate
        (_, KeyCode::Up) | (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
            if let Some(ref mut menu) = app.completion.slash_menu {
                menu.selected = menu.selected.saturating_sub(1);
            }
            None
        }
        (_, KeyCode::Down) | (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
            if let Some(ref mut menu) = app.completion.slash_menu {
                let len = menu_len(menu);
                if menu.selected + 1 < len {
                    menu.selected += 1;
                }
            }
            None
        }
        // backspace: edit input and update filter
        (_, KeyCode::Backspace) => {
            app.input.backspace();
            if app.input.text.is_empty() || !app.input.text.starts_with('/') {
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

fn menu_len(menu: &crate::slash_menu::SlashMenuState) -> usize {
    if menu.model_mode {
        menu.model_matches.len()
    } else {
        menu.matches.len()
    }
}

/// cycle to the next (direction=+1) or previous (direction=-1) favourite
/// model relative to the currently active model. empty favourites → status
/// message nudge + no event
fn cycle_favourite_model(app: &mut App, direction: isize) -> Option<AppEvent> {
    let favs = &app.completion.favourite_models;
    if favs.is_empty() {
        app.status =
            Some("no favourite models yet, open /model and ★ some with ctrl+f".to_string());
        return None;
    }
    // find current position. if current model isn't a favourite, start from
    // -1 so the first step lands on index 0 (for +1) or the last (for -1)
    let current = favs.iter().position(|id| id == app.model_id.as_str());
    let len = favs.len();
    let next_idx = match (current, direction) {
        (Some(i), d) => (i as isize + d).rem_euclid(len as isize) as usize,
        (None, d) if d > 0 => 0,
        (None, _) => len - 1,
    };
    let next_id = favs[next_idx].clone();
    if next_id == app.model_id.as_str() {
        // single-favourite list and we're already on it; nothing to do
        return None;
    }
    Some(AppEvent::ModelSelected { model_id: next_id })
}

/// toggle favourite status on the currently-selected model row. returns
/// `AppEvent::PersistFavourites` when the list actually changed so the
/// runner can save to disk. no-op + toast when locked
fn toggle_selected_favourite(app: &mut App) -> Option<AppEvent> {
    let menu = app.completion.slash_menu.as_mut()?;
    if !menu.model_mode {
        return None;
    }
    if menu.favourites_locked {
        menu.toast = Some("favourites are locked by config.toml".to_string());
        return None;
    }
    let model = menu.model_matches.get(menu.selected)?;
    let id = model.id.clone();
    if let Some(pos) = app
        .completion
        .favourite_models
        .iter()
        .position(|f| f == &id)
    {
        app.completion.favourite_models.remove(pos);
    } else {
        app.completion.favourite_models.push(id);
    }
    // keep the menu's copy in sync so the star marker updates immediately
    if let Some(ref mut menu) = app.completion.slash_menu {
        menu.favourite_models = app.completion.favourite_models.clone();
    }
    Some(AppEvent::PersistFavourites)
}

/// toggle the model picker between all-models and favourites-only views.
/// swaps `menu.model_matches` to the subset of `model_completions` that are
/// favourited (or back to all). leaves the picker open. when favourites are
/// empty, sets a "no favourites yet" toast and stays in the all view
fn toggle_model_favourites_view(app: &mut App) {
    let Some(ref mut menu) = app.completion.slash_menu else {
        return;
    };
    if !menu.model_mode {
        return;
    }
    // a favourites-only view is one where every visible row is a favourite
    // AND the count is smaller than the all-models set. flip by comparing
    // current matches to the full list: if any non-favourite is visible, we
    // were in "all" → switch to favourites
    let has_non_favourite = menu
        .model_matches
        .iter()
        .any(|m| !menu.favourite_models.contains(&m.id));
    if has_non_favourite {
        // currently showing all. switch to favourites-only
        if menu.favourite_models.is_empty() {
            menu.toast = Some("no favourites yet, ★ some with ctrl+f".to_string());
            return;
        }
        let favs: Vec<_> = app
            .completion
            .model_completions
            .iter()
            .filter(|m| menu.favourite_models.contains(&m.id))
            .cloned()
            .collect();
        menu.model_matches = favs;
        menu.selected = 0;
        menu.toast = Some("showing favourites only".to_string());
    } else {
        // currently favourites-only (or empty). switch back to all
        menu.model_matches = app.completion.model_completions.clone();
        menu.selected = 0;
        menu.toast = None;
    }
}

/// ctrl+[ is byte 0x1B, indistinguishable from ESC on terminals without
/// kitty's DISAMBIGUATE_ESCAPE_CODES. on terminals with it, crossterm reports
/// ctrl+[ as (CONTROL, Char('[')). this helper treats them equivalently so
/// users can cancel modals with either on any terminal
fn is_cancel_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc)
        || (key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('['))
}

fn handle_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) | (KeyModifiers::CONTROL, KeyCode::Char('c' | '[')) => {
            app.close_session_picker();
            None
        }
        (_, KeyCode::Tab) | (_, KeyCode::BackTab) => {
            // toggle scope between this dir and all dirs
            if let Some(ref mut picker) = app.interaction.session_picker {
                picker.scope = match picker.scope {
                    SessionScope::ThisDir => SessionScope::AllDirs,
                    SessionScope::AllDirs => SessionScope::ThisDir,
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
                    action: crate::slash::SlashAction::Resume {
                        session_id: id.into(),
                    },
                })
            } else {
                None
            }
        }
        (_, KeyCode::Up) | (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
            if let Some(ref mut picker) = app.interaction.session_picker {
                picker.selected = picker.selected.saturating_sub(1);
            }
            None
        }
        (_, KeyCode::Down) | (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
            if let Some(ref mut picker) = app.interaction.session_picker {
                let filtered_len = filtered_sessions(picker).len();
                if picker.selected + 1 < filtered_len {
                    picker.selected += 1;
                }
            }
            None
        }
        (_, KeyCode::Backspace) => {
            if let Some(ref mut picker) = app.interaction.session_picker {
                picker.filter.pop();
                picker.selected = 0;
            }
            None
        }
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
            if let Some(ref mut picker) = app.interaction.session_picker {
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
    fn ctrl_c_quits() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        let event = handle_key(&mut app, ctrl(KeyCode::Char('c')));
        assert!(matches!(event, Some(AppEvent::Quit)));
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
            Some(AppEvent::UserSubmit { text }) => assert_eq!(text, "hello"),
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
    fn ctrl_o_toggles_thinking_expanded() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
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
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        assert!(!app.interaction.show_prompt_injection);
        handle_key(&mut app, ctrl(KeyCode::Char('i')));
        assert!(app.interaction.show_prompt_injection);
        assert_eq!(app.status.as_deref(), Some("prompt injection visible"));

        handle_key(&mut app, ctrl(KeyCode::Char('i')));
        assert!(!app.interaction.show_prompt_injection);
        assert_eq!(app.status.as_deref(), Some("prompt injection hidden"));
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
            Some(AppEvent::UserSubmit { text }) => assert_eq!(text, "steer this"),
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
            description: String::new(),
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
            Some(AppEvent::UserSubmit { text }) => {
                assert_eq!(text, "@asdf", "enter must send the raw @word");
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
        assert!(!menu.model_mode);
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

    #[test]
    fn model_picker_enter_emits_model_selected() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.completion.model_completions = vec![
            crate::app::ModelCompletion {
                id: "claude-opus".into(),
                name: "Claude Opus".into(),
            },
            crate::app::ModelCompletion {
                id: "gpt-5".into(),
                name: "GPT-5".into(),
            },
        ];
        app.open_model_picker();

        // select the second model
        handle_key(&mut app, ctrl(KeyCode::Char('j')));

        let event = handle_key(&mut app, key(KeyCode::Enter));
        assert!(
            matches!(event, Some(AppEvent::ModelSelected { ref model_id }) if model_id == "gpt-5"),
            "expected ModelSelected{{ gpt-5 }}, got {event:?}"
        );
        assert!(app.completion.slash_menu.is_none());
        assert_eq!(app.interaction.mode, AppMode::Normal);
    }

    #[test]
    fn model_picker_tab_toggles_favourites_only_view() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.completion.model_completions = vec![
            crate::app::ModelCompletion {
                id: "claude-opus".into(),
                name: "Claude Opus".into(),
            },
            crate::app::ModelCompletion {
                id: "gpt-5".into(),
                name: "GPT-5".into(),
            },
            crate::app::ModelCompletion {
                id: "gemini".into(),
                name: "Gemini".into(),
            },
        ];
        app.completion.favourite_models = vec!["claude-opus".into()];
        app.open_model_picker();
        let menu = app.completion.slash_menu.as_ref().unwrap();
        assert_eq!(menu.model_matches.len(), 3, "all 3 visible by default");

        // tab toggles to favourites-only
        handle_key(&mut app, key(KeyCode::Tab));
        let menu = app.completion.slash_menu.as_ref().unwrap();
        assert_eq!(
            menu.model_matches.len(),
            1,
            "favourites-only view should restrict to 1 model"
        );
        assert_eq!(menu.model_matches[0].id, "claude-opus");

        // tab again toggles back to all
        handle_key(&mut app, key(KeyCode::Tab));
        let menu = app.completion.slash_menu.as_ref().unwrap();
        assert_eq!(menu.model_matches.len(), 3, "all models visible again");
    }

    #[test]
    fn model_picker_tab_toasts_when_no_favourites() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.completion.model_completions = vec![crate::app::ModelCompletion {
            id: "claude-opus".into(),
            name: "Claude Opus".into(),
        }];
        assert!(app.completion.favourite_models.is_empty());
        app.open_model_picker();

        handle_key(&mut app, key(KeyCode::Tab));
        let menu = app.completion.slash_menu.as_ref().unwrap();
        assert_eq!(
            menu.model_matches.len(),
            1,
            "no favourites means the all-view must stay"
        );
        assert!(
            menu.toast
                .as_deref()
                .is_some_and(|t| t.contains("no favourites")),
            "expected a 'no favourites' toast, got {:?}",
            menu.toast
        );
    }

    #[test]
    fn model_picker_ctrl_f_toggles_favourite_when_unlocked() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.completion.model_completions = vec![crate::app::ModelCompletion {
            id: "claude-opus".into(),
            name: "Claude Opus".into(),
        }];
        app.completion.favourite_models = Vec::new();
        app.completion.favourites_locked = false;
        app.open_model_picker();

        // add to favourites
        let event = handle_key(&mut app, ctrl(KeyCode::Char('f')));
        assert!(
            matches!(event, Some(AppEvent::PersistFavourites)),
            "expected PersistFavourites event, got {event:?}"
        );
        assert_eq!(app.completion.favourite_models, vec!["claude-opus"]);
        let menu = app.completion.slash_menu.as_ref().unwrap();
        assert!(menu.is_favourite("claude-opus"));

        // toggle off
        let event = handle_key(&mut app, ctrl(KeyCode::Char('f')));
        assert!(matches!(event, Some(AppEvent::PersistFavourites)));
        assert!(app.completion.favourite_models.is_empty());
    }

    #[test]
    fn model_picker_ctrl_f_toasts_when_locked() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.completion.model_completions = vec![crate::app::ModelCompletion {
            id: "claude-opus".into(),
            name: "Claude Opus".into(),
        }];
        app.completion.favourite_models = vec!["claude-opus".into()];
        app.completion.favourites_locked = true;
        app.open_model_picker();

        let event = handle_key(&mut app, ctrl(KeyCode::Char('f')));
        assert!(
            event.is_none(),
            "locked ctrl+f must not emit a persist event, got {event:?}"
        );
        // list unchanged
        assert_eq!(app.completion.favourite_models, vec!["claude-opus"]);
        let menu = app.completion.slash_menu.as_ref().unwrap();
        assert!(
            menu.toast.as_deref().is_some_and(|t| t.contains("locked")),
            "expected locked toast, got {:?}",
            menu.toast
        );
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
    fn slash_menu_opens_for_model_subcommand() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.completion.slash_commands = vec![crate::app::SlashCommand {
            name: "model".into(),
            description: "show or switch model".into(),
        }];
        app.completion.model_completions = vec![
            crate::app::ModelCompletion {
                id: "claude-opus-4-7".into(),
                name: "Claude Opus 4.7".into(),
            },
            crate::app::ModelCompletion {
                id: "claude-sonnet-4-20250514".into(),
                name: "Claude Sonnet 4".into(),
            },
        ];
        app.input.text = "/model claude".into();
        app.input.cursor = app.input.text.len();

        handle_key(&mut app, key(KeyCode::Tab));

        let menu = app.completion.slash_menu.as_ref().unwrap();
        assert!(menu.model_mode);
        assert_eq!(menu.model_matches.len(), 2);
    }

    #[test]
    fn slash_menu_selects_model_completion() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.completion.slash_commands = vec![crate::app::SlashCommand {
            name: "model".into(),
            description: "show or switch model".into(),
        }];
        app.completion.model_completions = vec![
            crate::app::ModelCompletion {
                id: "claude-opus-4-7".into(),
                name: "Claude Opus 4.7".into(),
            },
            crate::app::ModelCompletion {
                id: "claude-sonnet-4-20250514".into(),
                name: "Claude Sonnet 4".into(),
            },
        ];
        app.input.text = "/model claude".into();
        app.input.cursor = app.input.text.len();

        handle_key(&mut app, key(KeyCode::Tab));
        handle_key(&mut app, ctrl(KeyCode::Char('j')));
        let event = handle_key(&mut app, key(KeyCode::Enter));

        assert!(
            matches!(event, Some(AppEvent::ModelSelected { ref model_id }) if model_id == "claude-sonnet-4-20250514"),
            "enter in model picker must emit ModelSelected directly, not just rewrite input text; got {event:?}"
        );
        assert!(app.completion.slash_menu.is_none());
    }

    #[test]
    fn slash_menu_typing_filters_model_matches() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.completion.slash_commands = vec![crate::app::SlashCommand {
            name: "model".into(),
            description: "show or switch model".into(),
        }];
        app.completion.model_completions = vec![
            crate::app::ModelCompletion {
                id: "claude-opus-4-7".into(),
                name: "Claude Opus 4.7".into(),
            },
            crate::app::ModelCompletion {
                id: "claude-sonnet-4-20250514".into(),
                name: "Claude Sonnet 4".into(),
            },
        ];
        app.input.text = "/model claude-".into();
        app.input.cursor = app.input.text.len();

        handle_key(&mut app, key(KeyCode::Tab));
        // type 'opu' - fuzzy-distinct between opus and sonnet
        handle_key(&mut app, key(KeyCode::Char('o')));
        handle_key(&mut app, key(KeyCode::Char('p')));
        handle_key(&mut app, key(KeyCode::Char('u')));

        let menu = app.completion.slash_menu.as_ref().unwrap();
        assert!(menu.model_mode);
        // opus ranks first; sonnet may or may not match with a low score
        // depending on fuzzy tolerance, so just assert the top hit
        assert_eq!(menu.model_matches[0].id, "claude-opus-4-7");
    }

    #[test]
    fn slash_menu_model_filter_is_fuzzy_subsequence() {
        // demonstrate the fuzzy upgrade: "clop" isn't a substring of any
        // model id but subsequence-matches "claude-opus"
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.completion.slash_commands = vec![crate::app::SlashCommand {
            name: "model".into(),
            description: "show or switch model".into(),
        }];
        app.completion.model_completions = vec![
            crate::app::ModelCompletion {
                id: "claude-opus-4-7".into(),
                name: "Claude Opus 4.7".into(),
            },
            crate::app::ModelCompletion {
                id: "gpt-5".into(),
                name: "GPT 5".into(),
            },
        ];
        app.input.text = "/model ".into();
        app.input.cursor = app.input.text.len();

        handle_key(&mut app, key(KeyCode::Tab));
        for c in "clop".chars() {
            handle_key(&mut app, key(KeyCode::Char(c)));
        }

        let menu = app.completion.slash_menu.as_ref().unwrap();
        assert_eq!(menu.model_matches.len(), 1);
        assert_eq!(menu.model_matches[0].id, "claude-opus-4-7");
    }

    #[test]
    fn input_reinsert_while_streaming_keeps_cursor_visible() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.stream.active = true;
        app.input.area.set(ratatui::layout::Rect::new(0, 0, 20, 12));
        app.input.visible_lines.set(2);
        app.input.total_lines.set(8);
        app.input.scroll.set(0);
        // use a blocked slash command so the input gets reinserted
        app.input.text = "/compact a\nb\nc\nd".into();
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
    fn scroll_mode_block_jk_navigates_code_blocks() {
        use crate::app::ScrollUnit;
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.start_streaming();
        app.push_text_delta("here:\n```bash\nrm -rf mything\n```\nand:\n```python\nprint(42)\n```");
        app.finish_streaming(None, None);

        // enter scroll mode (default: Block)
        handle_key(&mut app, ctrl(KeyCode::Char('s')));
        assert_eq!(app.navigation.scroll_unit, ScrollUnit::Block);

        // should start on the last block
        assert_eq!(app.navigation.selected_block, Some(1));

        // k moves to previous block
        handle_key(&mut app, key(KeyCode::Char('k')));
        assert_eq!(app.navigation.selected_block, Some(0));

        // k at start stays at 0
        handle_key(&mut app, key(KeyCode::Char('k')));
        assert_eq!(app.navigation.selected_block, Some(0));

        // j moves forward
        handle_key(&mut app, key(KeyCode::Char('j')));
        assert_eq!(app.navigation.selected_block, Some(1));

        // j at end stays at last
        handle_key(&mut app, key(KeyCode::Char('j')));
        assert_eq!(app.navigation.selected_block, Some(1));
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
        use mush_ai::types::{ModelId, SessionId, Timestamp};
        use mush_session::SessionMeta;
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.open_session_picker(
            vec![SessionMeta {
                id: SessionId::new(),
                title: Some("hello".into()),
                model_id: ModelId::from("m"),
                created_at: Timestamp::now(),
                updated_at: Timestamp::now(),
                message_count: 1,
                cwd: "/tmp".into(),
            }],
            "/tmp".into(),
        );
        assert_eq!(app.interaction.mode, AppMode::SessionPicker);

        handle_key(&mut app, ctrl(KeyCode::Char('[')));
        assert_eq!(app.interaction.mode, AppMode::Normal);
        assert!(app.interaction.session_picker.is_none());
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
}
