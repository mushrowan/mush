//! keyboard input handling
//!
//! maps crossterm key events to app mutations and events

use crossterm::event::KeyEvent;

use crate::app::{App, AppEvent, AppMode};
use crate::session_picker::{SessionScope, filtered_sessions};

// A trailing '\' escapes plain Enter into a literal newline, but only when the
// cursor is at the end of the current line.
fn should_escape_enter_to_newline(app: &App, key: KeyEvent) -> bool {
    if !crate::keybinds::is_plain_enter(key) || app.input.cursor == 0 {
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
    if app.interaction.mode == AppMode::ModelPicker {
        return handle_model_picker_key(app, key);
    }
    if app.interaction.mode == AppMode::LoginPicker {
        return handle_login_picker_key(app, key);
    }
    if app.interaction.mode == AppMode::SlashComplete {
        return handle_slash_menu_key(app, key);
    }
    if app.interaction.mode == AppMode::AtPicker {
        return handle_at_picker_key(app, key);
    }
    if app.interaction.mode == AppMode::SlotEdit {
        return handle_slot_edit_key(app, key);
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
    if app.interaction.mode == AppMode::CompactionRecovery {
        return handle_compaction_recovery_key(app, key);
    }

    // 2. global bindings
    //
    // ctrl+c semantics:
    //   non-empty input → clear input + images
    //   empty input + busy → abort the turn (no confirm)
    //   empty input + idle, not armed → arm + status hint
    //   empty input + idle, armed → quit
    // any other key resets the armed flag so a stray earlier ctrl+c
    // can't quit later
    if app
        .keymap
        .matches(crate::keybinds::Action::QuitOrAbort, key)
    {
        if !app.input.text.is_empty() || !app.input.images.is_empty() {
            app.input.take_text();
            app.input.take_images();
            app.confirm_quit_pending = false;
            return None;
        }
        if app.is_busy() {
            app.confirm_quit_pending = false;
            return Some(AppEvent::Abort);
        }
        if app.confirm_quit_pending {
            return Some(AppEvent::Quit);
        }
        app.confirm_quit_pending = true;
        app.status = Some("press ctrl+c again to exit".into());
        return None;
    }
    app.confirm_quit_pending = false;

    // ctrl+v works in both idle and streaming so the user can stage
    // images for mid-turn steering without aborting the stream
    if app.keymap.matches(crate::keybinds::Action::PasteImage, key) {
        return Some(AppEvent::PasteImage);
    }
    // scroll / cancel cascade: esc (or ctrl+[) first jumps to bottom if
    // scrolled up, only aborts once already anchored there. this applies
    // even while streaming so users can re-anchor without cancelling a
    // long turn. single check + internal branches avoids duplicate guards.
    // we key off `is_busy()` rather than just `stream.active` so esc still
    // works during the tool-execution window between an assistant
    // `MessageEnd` and the next `TurnStart`, when text is no longer
    // streaming but the agent is still cranking through tool calls
    if is_cancel_key(app, key) {
        if app.is_scrolled_up() {
            app.scroll_to_bottom();
            return None;
        }
        if app.is_busy() {
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
    if app
        .keymap
        .matches(crate::keybinds::Action::ScrollPageUp, key)
    {
        app.scroll_up(10);
        return Some(AppEvent::ScrollUp(10));
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::ScrollPageDown, key)
    {
        app.scroll_down(10);
        return Some(AppEvent::ScrollDown(10));
    }

    // 3. pane management (works regardless of streaming state)
    if let Some(event) = handle_pane_keys(app, key) {
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
    if app
        .keymap
        .matches(crate::keybinds::Action::InsertNewline, key)
    {
        app.input_char('\n');
        return None;
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
        let is_alt_combo = crate::keybinds::has_alt(key);
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
fn handle_pane_keys(app: &App, key: KeyEvent) -> Option<AppEvent> {
    if app.keymap.matches(crate::keybinds::Action::SplitPane, key) {
        return Some(AppEvent::SplitPane);
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::FocusNextPane, key)
    {
        return Some(AppEvent::FocusNextPane);
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::FocusPrevPane, key)
    {
        return Some(AppEvent::FocusPrevPane);
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::ResizePaneLeft, key)
    {
        return Some(AppEvent::ResizePane(-4));
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::ResizePaneRight, key)
    {
        return Some(AppEvent::ResizePane(4));
    }

    focus_pane_index(app, key).map(AppEvent::FocusPaneByIndex)
}

fn focus_pane_index(app: &App, key: KeyEvent) -> Option<usize> {
    use crate::keybinds::Action;

    [
        Action::FocusPane1,
        Action::FocusPane2,
        Action::FocusPane3,
        Action::FocusPane4,
        Action::FocusPane5,
        Action::FocusPane6,
        Action::FocusPane7,
        Action::FocusPane8,
        Action::FocusPane9,
    ]
    .into_iter()
    .position(|action| app.keymap.matches(action, key))
}

/// keys specific to streaming: enter submits to steering queue
fn handle_streaming_keys(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    if crate::keybinds::is_plain_enter(key) {
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
        return Some(AppEvent::UserSubmit {
            text,
            images: app.input.take_images(),
        });
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
            let blocks = app.blocks();
            app.navigation.selected_block = if blocks.is_empty() {
                None
            } else {
                Some(blocks.len() - 1)
            };
        }
        return None;
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::CycleThinkingLevel, key)
    {
        app.cycle_thinking_level();
        return Some(AppEvent::CycleThinkingLevel);
    }

    match () {
        // tab completion / slash menu
        _ if crate::keybinds::is_tab(key) || crate::keybinds::is_backtab(key) => {
            // @-template expansion takes priority over generic tab completion
            // so users can type `@review<tab>` and get the template content.
            // bare `@` with no matching template falls through to normal
            // tab behaviour so typing `@ ` in code/paths still works
            if try_expand_at_template(app) {
                return None;
            }
            if try_open_at_picker(app) {
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
        _ if app.keymap.matches(crate::keybinds::Action::Confirm, key) => {
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
            Some(AppEvent::UserSubmit {
                text,
                images: app.input.take_images(),
            })
        }

        // delete-char-or-quit: quit on empty, delete char otherwise
        _ if app
            .keymap
            .matches(crate::keybinds::Action::DeleteCharOrQuit, key) =>
        {
            if app.input.text.is_empty() {
                return Some(AppEvent::Quit);
            }
            app.input.delete();
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
    let content = template.content.clone();
    let end = app.input.cursor;
    insert_template(app, trigger.start, end, &content);
    true
}

/// replace the byte range `[start, end)` of `app.input.text` with
/// `content`, then if the content has placeholders enter slot-edit
/// mode parking the cursor at the first slot. shared by the exact-match
/// path and the picker `enter` path so both routes get slot-editing for
/// free
fn insert_template(app: &mut App, start: usize, end: usize, content: &str) {
    let (clean, slot_offsets) = crate::at_template::strip_placeholders(content);
    app.input.text.replace_range(start..end, &clean);

    if slot_offsets.is_empty() {
        // no placeholders: simple expansion, cursor at end of insertion
        app.input.cursor = start + clean.len();
        app.input.ensure_cursor_visible();
        return;
    }

    // shift slot offsets into absolute byte positions in `input.text`
    let slots: Vec<usize> = slot_offsets.iter().map(|&o| start + o).collect();
    app.input.cursor = slots[0];
    app.input.ensure_cursor_visible();
    let slot_count = slots.len();
    app.interaction.slot_edit = Some(crate::app_state::SlotEditState { slots, current: 0 });
    app.interaction.mode = AppMode::SlotEdit;
    app.status = Some(format!("slot 1/{slot_count} · tab to cycle, ⏎ to finish"));
}

/// open the @-template picker if the trigger word has prefix matches
/// but no exact match. returns whether the picker was opened. callers
/// should fall through to normal tab completion when this returns false
fn try_open_at_picker(app: &mut App) -> bool {
    let Some(trigger) = crate::at_template::parse_at_trigger(&app.input.text, app.input.cursor)
    else {
        return false;
    };
    let matches: Vec<mush_ext::PromptTemplate> =
        crate::at_template::find_prefix(&app.completion.templates, &trigger)
            .into_iter()
            .cloned()
            .collect();
    if matches.is_empty() {
        return false;
    }
    app.interaction.at_picker = Some(crate::app_state::AtPickerState {
        matches,
        selected: 0,
        trigger_start: trigger.start,
        trigger_end: app.input.cursor,
    });
    app.interaction.mode = AppMode::AtPicker;
    true
}

/// key handler while the `@`-template picker popup is open. tab and
/// shift+tab cycle, enter inserts the selected template and closes,
/// esc / ctrl+[ close without touching the input. typing characters
/// narrows the filter; if the user types past every match the picker
/// closes and the character still lands in the input
fn handle_at_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    let Some(picker) = app.interaction.at_picker.as_mut() else {
        // shouldn't happen, but recover gracefully
        app.interaction.mode = AppMode::Normal;
        return None;
    };
    let count = picker.matches.len();
    match () {
        _ if app.keymap.matches_in(
            crate::keybinds::KeyScope::AtPicker,
            crate::keybinds::Action::PreviousItem,
            key,
        ) =>
        {
            picker.selected = (picker.selected + count - 1) % count;
            None
        }
        _ if app.keymap.matches_in(
            crate::keybinds::KeyScope::AtPicker,
            crate::keybinds::Action::NextItem,
            key,
        ) =>
        {
            picker.selected = (picker.selected + 1) % count;
            None
        }
        _ if app.keymap.matches_in(
            crate::keybinds::KeyScope::AtPicker,
            crate::keybinds::Action::Confirm,
            key,
        ) =>
        {
            // copy out the fields we need before replacing the input,
            // since insertion mutates app.input via &mut
            let template = picker.matches[picker.selected].clone();
            let start = picker.trigger_start;
            let end = picker.trigger_end;
            app.interaction.at_picker = None;
            app.interaction.mode = AppMode::Normal;
            insert_template(app, start, end, &template.content);
            None
        }
        _ if app.keymap.matches_in(
            crate::keybinds::KeyScope::AtPicker,
            crate::keybinds::Action::Cancel,
            key,
        ) =>
        {
            app.interaction.at_picker = None;
            app.interaction.mode = AppMode::Normal;
            None
        }
        _ if crate::keybinds::is_backspace(key) => {
            // backspacing into the trigger narrows the filter. dropping
            // back past the `@` closes the picker entirely
            app.input.backspace();
            refresh_at_picker(app);
            None
        }
        _ if let Some(c) = crate::keybinds::typed_char(key) => {
            app.input_char(c);
            refresh_at_picker(app);
            None
        }
        _ => None,
    }
}

/// re-evaluate the picker after the input changed mid-mode. closes the
/// picker if the trigger has dissolved or no candidates match the new
/// word; otherwise updates `matches` and clamps `selected`
fn refresh_at_picker(app: &mut App) {
    let Some(trigger) = crate::at_template::parse_at_trigger(&app.input.text, app.input.cursor)
    else {
        app.interaction.at_picker = None;
        app.interaction.mode = AppMode::Normal;
        return;
    };
    let matches: Vec<mush_ext::PromptTemplate> =
        crate::at_template::find_prefix(&app.completion.templates, &trigger)
            .into_iter()
            .cloned()
            .collect();
    if matches.is_empty() {
        app.interaction.at_picker = None;
        app.interaction.mode = AppMode::Normal;
        return;
    }
    if let Some(picker) = app.interaction.at_picker.as_mut() {
        picker.trigger_end = app.input.cursor;
        picker.selected = picker.selected.min(matches.len().saturating_sub(1));
        picker.matches = matches;
    }
}

/// key handler while the slot editor is active. tab cycles forward
/// through slots, shift+tab cycles back, enter / esc exit the mode and
/// keep the typed text in place. typing inserts at the cursor and
/// shifts every later slot offset by the same byte count so they keep
/// pointing at the right place
fn handle_slot_edit_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    let Some(state) = app.interaction.slot_edit.as_ref() else {
        app.interaction.mode = AppMode::Normal;
        return None;
    };
    let count = state.slots.len();
    match () {
        _ if app.keymap.matches_in(
            crate::keybinds::KeyScope::SlotEdit,
            crate::keybinds::Action::PreviousItem,
            key,
        ) =>
        {
            let next = (state.current + count - 1) % count;
            jump_to_slot(app, next);
            None
        }
        _ if app.keymap.matches_in(
            crate::keybinds::KeyScope::SlotEdit,
            crate::keybinds::Action::NextItem,
            key,
        ) =>
        {
            let next = (state.current + 1) % count;
            jump_to_slot(app, next);
            None
        }
        _ if app.keymap.matches_in(
            crate::keybinds::KeyScope::SlotEdit,
            crate::keybinds::Action::Confirm,
            key,
        ) || app.keymap.matches_in(
            crate::keybinds::KeyScope::SlotEdit,
            crate::keybinds::Action::Cancel,
            key,
        ) =>
        {
            app.interaction.slot_edit = None;
            app.interaction.mode = AppMode::Normal;
            None
        }
        _ if crate::keybinds::is_backspace(key) => {
            // shift later slots back by 1 if a byte gets removed. avoid
            // deleting past the current slot's start so users can't
            // accidentally chew through earlier verbatim template content
            let state = app.interaction.slot_edit.as_ref()?;
            let slot_start = state.slots[state.current];
            if app.input.cursor <= slot_start {
                return None;
            }
            let cursor = app.input.cursor;
            app.input.backspace();
            shift_later_slots(app, cursor, -1);
            None
        }
        _ if let Some(c) = crate::keybinds::typed_char(key) => {
            let cursor = app.input.cursor;
            app.input_char(c);
            let inserted = app.input.cursor - cursor;
            shift_later_slots(app, cursor, inserted as isize);
            None
        }
        // arrow keys, home/end, etc fall through silently so cursor
        // movement keeps working without breaking slot-tracking
        _ => None,
    }
}

/// move the cursor to slot index `target` and update `current`
fn jump_to_slot(app: &mut App, target: usize) {
    if let Some(state) = app.interaction.slot_edit.as_mut() {
        state.current = target;
        let position = state.slots[target];
        let total = state.slots.len();
        app.input.cursor = position;
        app.input.ensure_cursor_visible();
        app.status = Some(format!(
            "slot {}/{total} · tab to cycle, ⏎ to finish",
            target + 1
        ));
    }
}

/// shift every slot strictly after `cutoff` by `delta` bytes. used
/// after inserts and deletes inside the current slot so later slot
/// offsets keep pointing at their actual position in the live input.
/// strictly-greater so the current slot's own start byte stays put
fn shift_later_slots(app: &mut App, cutoff: usize, delta: isize) {
    if let Some(state) = app.interaction.slot_edit.as_mut() {
        for slot in &mut state.slots {
            if *slot > cutoff {
                *slot = (*slot as isize + delta) as usize;
            }
        }
    }
}

/// shared text editing bindings (cursor movement, deletion, character input)
fn handle_editing(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    if app
        .keymap
        .matches(crate::keybinds::Action::DeleteWordBackward, key)
    {
        app.input.delete_word_backward();
        return None;
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::DeleteWordForward, key)
    {
        app.input.delete_word_forward();
        return None;
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::DeleteToStart, key)
    {
        app.input.delete_to_start();
        return None;
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::DeleteToEnd, key)
    {
        app.input.delete_to_end();
        return None;
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::CursorWordLeft, key)
    {
        app.input.cursor_word_left();
        return None;
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::CursorWordRight, key)
    {
        app.input.cursor_word_right();
        return None;
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::CursorLineStart, key)
    {
        app.input.cursor_home();
        return None;
    }
    if app
        .keymap
        .matches(crate::keybinds::Action::CursorLineEnd, key)
    {
        app.input.cursor_end();
        return None;
    }

    match (key.modifiers, key.code) {
        _ if crate::keybinds::is_backspace(key) => app.input.backspace(),
        _ if crate::keybinds::is_delete(key) => app.input.delete(),

        // cursor movement
        _ if app.keymap.matches_in(
            crate::keybinds::KeyScope::Edit,
            crate::keybinds::Action::CursorLeft,
            key,
        ) =>
        {
            app.input.cursor_left()
        }
        _ if app.keymap.matches_in(
            crate::keybinds::KeyScope::Edit,
            crate::keybinds::Action::CursorLeft,
            key,
        ) =>
        {
            app.input.cursor_left()
        }
        _ if app.keymap.matches_in(
            crate::keybinds::KeyScope::Edit,
            crate::keybinds::Action::CursorRight,
            key,
        ) =>
        {
            app.input.cursor_right()
        }

        // character input
        _ if let Some(c) = crate::keybinds::typed_char(key) => app.input_char(c),

        _ => {}
    }
    None
}

/// handle keys in /settings overlay mode
fn handle_settings_menu_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    use crate::keybinds::{Action, KeyScope};

    let Some(menu) = app.settings_menu.as_mut() else {
        app.interaction.mode = AppMode::Normal;
        return None;
    };
    if app.keymap.matches(Action::QuitOrAbort, key)
        || app
            .keymap
            .matches_in(KeyScope::Settings, Action::Cancel, key)
    {
        app.settings_menu = None;
        app.interaction.mode = AppMode::Normal;
    } else if app
        .keymap
        .matches_in(KeyScope::Settings, Action::NextItem, key)
    {
        menu.move_down();
    } else if app
        .keymap
        .matches_in(KeyScope::Settings, Action::PreviousItem, key)
    {
        menu.move_up();
    } else if app
        .keymap
        .matches_in(KeyScope::Settings, Action::NavigateTop, key)
    {
        menu.top();
    } else if app
        .keymap
        .matches_in(KeyScope::Settings, Action::NavigateBottom, key)
    {
        menu.bottom();
    } else if app
        .keymap
        .matches_in(KeyScope::Settings, Action::SelectOrSpace, key)
    {
        return Some(AppEvent::SettingsToggleSelected);
    }
    None
}

fn handle_compaction_recovery_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    use crate::keybinds::Action;

    let Some(recovery) = app.interaction.compaction_recovery.as_mut() else {
        app.interaction.mode = AppMode::Normal;
        return None;
    };
    if app.keymap.matches(Action::QuitOrAbort, key) || app.keymap.matches(Action::Cancel, key) {
        return Some(AppEvent::CompactionRecoverySelected {
            action: crate::app_state::CompactionRecoveryAction::Cancel,
        });
    }
    if app.keymap.matches(Action::NextItem, key) {
        recovery.move_down();
        return None;
    }
    if app.keymap.matches(Action::PreviousItem, key) {
        recovery.move_up();
        return None;
    }
    if app.keymap.matches(Action::Confirm, key) || app.keymap.matches(Action::SelectOrSpace, key) {
        let action = recovery.selected_action()?;
        return Some(AppEvent::CompactionRecoverySelected { action });
    }
    None
}

/// handle keys in session picker mode
fn handle_search_mode(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    use crate::keybinds::{Action, KeyScope};

    match () {
        _ if app.keymap.matches(Action::QuitOrAbort, key) => return Some(AppEvent::Quit),
        _ if app.keymap.matches_in(KeyScope::Search, Action::Cancel, key)
            || app.keymap.matches(Action::EnterSearch, key) =>
        {
            app.interaction.mode = AppMode::Normal;
            app.interaction.search.query.clear();
            app.interaction.search.matches.clear();
        }
        _ if app
            .keymap
            .matches_in(KeyScope::Search, Action::Confirm, key) =>
        {
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
        _ if app
            .keymap
            .matches_in(KeyScope::Search, Action::PreviousItem, key)
            && !app.interaction.search.matches.is_empty() =>
        {
            app.interaction.search.selected = app.interaction.search.selected.saturating_sub(1);
        }
        _ if app
            .keymap
            .matches_in(KeyScope::Search, Action::NextItem, key)
            && !app.interaction.search.matches.is_empty()
            && app.interaction.search.selected + 1 < app.interaction.search.matches.len() =>
        {
            app.interaction.search.selected += 1;
        }
        _ if crate::keybinds::is_backspace(key) => {
            app.interaction.search.query.pop();
            app.update_search();
        }
        _ if let Some(c) = crate::keybinds::any_char(key) => {
            app.interaction.search.query.push(c);
            app.update_search();
        }
        _ => {}
    }
    None
}

fn handle_scroll_mode(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    use crate::app::ScrollUnit;
    use crate::keybinds::{Action, KeyScope};

    if app.keymap.matches(Action::QuitOrAbort, key) {
        return Some(AppEvent::Quit);
    }
    if app.keymap.matches_in(KeyScope::Scroll, Action::Cancel, key) {
        if app.navigation.selection_anchor.is_some() {
            app.navigation.selection_anchor = None;
        } else {
            app.interaction.mode = AppMode::Normal;
            app.navigation.selected_message = None;
            app.navigation.selected_block = None;
        }
        return None;
    }
    if app
        .keymap
        .matches_in(KeyScope::Scroll, Action::ToggleBlockMode, key)
    {
        app.navigation.scroll_unit = match app.navigation.scroll_unit {
            ScrollUnit::Message => {
                app.navigation.selection_anchor = None;
                let blocks = app.blocks();
                app.navigation.selected_block = if blocks.is_empty() {
                    None
                } else {
                    Some(blocks.len() - 1)
                };
                ScrollUnit::Block
            }
            ScrollUnit::Block => {
                app.navigation.selected_block = None;
                app.navigation.selection_anchor = None;
                ScrollUnit::Message
            }
        };
        return None;
    }
    if app
        .keymap
        .matches_in(KeyScope::Scroll, Action::ToggleVisualSelection, key)
        && app.navigation.scroll_unit == ScrollUnit::Message
    {
        if app.navigation.selection_anchor.is_some() {
            app.navigation.selection_anchor = None;
        } else {
            app.navigation.selection_anchor = app.navigation.selected_message;
        }
        return None;
    }
    if app
        .keymap
        .matches_in(KeyScope::Scroll, Action::NextItem, key)
    {
        let scroll_lines = app.scroll_lines;
        if app.navigation.scroll_unit == ScrollUnit::Block {
            let blocks = app.blocks();
            if let Some(sel) = app.navigation.selected_block
                && sel + 1 < blocks.len()
            {
                app.navigation.selected_block = Some(sel + 1);
                app.navigation.selected_message = Some(blocks[sel + 1].msg_idx);
                app.scroll_down(scroll_lines);
            }
        } else {
            app.scroll_down(scroll_lines);
            if let Some(sel) = app.navigation.selected_message
                && sel + 1 < app.messages.len()
                && app.is_message_visible(sel + 1)
            {
                app.navigation.selected_message = Some(sel + 1);
            }
        }
        tracing::debug!(
            target: "mush_tui::scroll",
            key = "next_item",
            scroll_lines,
            position = ?app.scroll,
            "scroll keypress"
        );
        return None;
    }
    if app
        .keymap
        .matches_in(KeyScope::Scroll, Action::PreviousItem, key)
    {
        let scroll_lines = app.scroll_lines;
        if app.navigation.scroll_unit == ScrollUnit::Block {
            let blocks = app.blocks();
            if let Some(sel) = app.navigation.selected_block {
                let new_sel = sel.saturating_sub(1);
                app.navigation.selected_block = Some(new_sel);
                app.navigation.selected_message = Some(blocks[new_sel].msg_idx);
                app.scroll_up(scroll_lines);
            }
        } else {
            app.scroll_up(scroll_lines);
            if let Some(sel) = app.navigation.selected_message {
                let target = sel.saturating_sub(1);
                if target != sel && app.is_message_visible(target) {
                    app.navigation.selected_message = Some(target);
                }
            }
        }
        tracing::debug!(
            target: "mush_tui::scroll",
            key = "previous_item",
            scroll_lines,
            position = ?app.scroll,
            "scroll keypress"
        );
        return None;
    }
    if app
        .keymap
        .matches_in(KeyScope::Scroll, Action::NavigateBottom, key)
    {
        app.scroll_to_bottom();
        if app.navigation.scroll_unit == ScrollUnit::Block {
            let blocks = app.blocks();
            app.navigation.selected_block = if blocks.is_empty() {
                None
            } else {
                Some(blocks.len() - 1)
            };
        }
        if !app.messages.is_empty() {
            app.navigation.selected_message = Some(app.messages.len() - 1);
        }
        return None;
    }
    if app
        .keymap
        .matches_in(KeyScope::Scroll, Action::NavigateTop, key)
    {
        app.scroll_to_top();
        if app.navigation.scroll_unit == ScrollUnit::Block {
            let blocks = app.blocks();
            app.navigation.selected_block = if blocks.is_empty() { None } else { Some(0) };
        }
        app.navigation.selected_message = Some(0);
        return None;
    }
    if app
        .keymap
        .matches_in(KeyScope::Scroll, Action::CopySelection, key)
        && let Some(selection) = app.copy_selection()
    {
        if copy_to_clipboard(&selection.text) {
            app.status = Some(selection.status);
        } else {
            app.status = Some("clipboard copy failed".into());
        }
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
    use crate::keybinds::{Action, KeyScope};

    match () {
        _ if app.keymap.matches(Action::QuitOrAbort, key)
            || app.keymap.matches_in(KeyScope::Normal, Action::Cancel, key) =>
        {
            app.close_slash_menu();
            None
        }
        // enter accepts the highlighted command and closes the menu
        _ if app
            .keymap
            .matches_in(KeyScope::Normal, Action::Confirm, key) =>
        {
            if let Some(ref menu) = app.completion.slash_menu
                && let Some(cmd) = menu.matches.get(menu.selected)
            {
                app.input.text = format!("/{}", cmd.name);
                app.input.cursor = app.input.text.len();
                app.input.ensure_cursor_visible();
            }
            app.close_slash_menu();
            None
        }
        // tab cycles selection forward
        _ if app
            .keymap
            .matches_in(KeyScope::Normal, Action::NextItem, key) =>
        {
            if let Some(ref mut menu) = app.completion.slash_menu {
                let len = menu.matches.len();
                if len > 0 {
                    menu.selected = (menu.selected + 1) % len;
                }
            }
            None
        }
        // shift+backtab cycles backward with wrap
        _ if app
            .keymap
            .matches_in(KeyScope::Normal, Action::PreviousItem, key) =>
        {
            if let Some(ref mut menu) = app.completion.slash_menu {
                let len = menu.matches.len();
                if len > 0 {
                    menu.selected = (menu.selected + len - 1) % len;
                }
            }
            None
        }
        // navigate
        // backspace: edit input and update filter
        _ if crate::keybinds::is_backspace(key) => {
            app.input.backspace();
            if app.input.text.is_empty() || !app.input.text.starts_with('/') {
                app.close_slash_menu();
            } else {
                app.update_slash_menu();
            }
            None
        }
        // typing narrows the filter
        _ if let Some(c) = crate::keybinds::typed_char(key) => {
            app.input_char(c);
            app.update_slash_menu();
            None
        }
        _ => None,
    }
}

/// load the on-disk discovery cache, apply the confirmed deletion, save.
/// returns the number of entries removed (0 when the target is missing).
fn apply_delete_confirm(confirm: &crate::slash_menu::DeleteConfirm) -> usize {
    use mush_ai::discovery::{DiscoveryCache, cache_path};
    let path = cache_path();
    let mut cache = DiscoveryCache::load(&path).unwrap_or_default();
    let removed = match confirm {
        crate::slash_menu::DeleteConfirm::Single { provider, id } => {
            usize::from(cache.remove_model(provider, id))
        }
        crate::slash_menu::DeleteConfirm::AllStale => cache.remove_all_stale(),
    };
    if removed > 0
        && let Err(e) = cache.save(&path)
    {
        tracing::warn!(error = %e, "failed to persist discovery cache after delete");
    }
    removed
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
/// ctrl+[ is byte 0x1B, indistinguishable from ESC on terminals without
/// kitty's DISAMBIGUATE_ESCAPE_CODES. on terminals with it, crossterm reports
/// ctrl+[ as (CONTROL, Char('[')). this helper treats them equivalently so
/// users can cancel modals with either on any terminal
fn is_cancel_key(app: &App, key: KeyEvent) -> bool {
    app.keymap.matches(crate::keybinds::Action::Cancel, key)
}

fn handle_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    match (key.modifiers, key.code) {
        _ if app
            .keymap
            .matches(crate::keybinds::Action::QuitOrAbort, key)
            || app.keymap.matches(crate::keybinds::Action::Cancel, key) =>
        {
            app.close_session_picker();
            None
        }
        _ if app
            .keymap
            .matches(crate::keybinds::Action::SessionPickerToggleScope, key) =>
        {
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
        _ if app.keymap.matches(crate::keybinds::Action::Confirm, key) => {
            // select session, return a slash command that the runner handles
            if let Some(meta) = app.selected_session() {
                let id = meta.id.to_string();
                app.close_session_picker();
                Some(AppEvent::SlashCommand {
                    action: crate::slash::SlashAction::Resume {
                        session_id: Some(id.into()),
                    },
                })
            } else {
                None
            }
        }
        _ if app.keymap.matches_in(
            crate::keybinds::KeyScope::SessionPicker,
            crate::keybinds::Action::PreviousItem,
            key,
        ) =>
        {
            if let Some(ref mut picker) = app.interaction.session_picker {
                picker.selected = picker.selected.saturating_sub(1);
            }
            None
        }
        _ if app.keymap.matches_in(
            crate::keybinds::KeyScope::SessionPicker,
            crate::keybinds::Action::NextItem,
            key,
        ) =>
        {
            if let Some(ref mut picker) = app.interaction.session_picker {
                let filtered_len = filtered_sessions(picker).len();
                if picker.selected + 1 < filtered_len {
                    picker.selected += 1;
                }
            }
            None
        }
        _ if crate::keybinds::is_backspace(key) => {
            if let Some(ref mut picker) = app.interaction.session_picker {
                picker.filter.pop();
                picker.selected = 0;
            }
            None
        }
        _ if let Some(c) = crate::keybinds::typed_char(key) => {
            if let Some(ref mut picker) = app.interaction.session_picker {
                picker.filter.push(c);
                picker.selected = 0;
            }
            None
        }
        _ => None,
    }
}

/// drive the centred `/model` picker overlay. mirrors
/// [`handle_picker_key`] for sessions but adds ctrl+f (toggle
/// favourite), ctrl+d (arm delete-stale prompt), and a confirmation
/// flow that intercepts y/n while a delete is armed
fn handle_model_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    use crate::keybinds::{Action, KeyScope};

    // intercept y/n when a delete prompt is armed
    if app
        .interaction
        .model_picker
        .as_ref()
        .is_some_and(|p| p.confirm_delete.is_some())
    {
        return handle_model_picker_delete_confirm_key(app, key);
    }

    match () {
        _ if app
            .keymap
            .matches_in(KeyScope::ModelPicker, Action::QuitOrAbort, key)
            || app
                .keymap
                .matches_in(KeyScope::ModelPicker, Action::Cancel, key) =>
        {
            app.close_model_picker();
            None
        }
        _ if crate::keybinds::is_tab(key) => {
            if let Some(ref mut picker) = app.interaction.model_picker {
                picker.toggle_tab();
                if matches!(picker.tab, crate::model_picker::ModelPickerTab::Favourites)
                    && picker.favourite_models.is_empty()
                {
                    picker.toast = Some("no favourites yet · ctrl+f to star one".into());
                } else {
                    picker.toast = None;
                }
            }
            None
        }
        _ if app
            .keymap
            .matches_in(KeyScope::ModelPicker, Action::Confirm, key) =>
        {
            let id = app.selected_model_id()?;
            let show_all = app
                .interaction
                .model_picker
                .as_ref()
                .map(|p| p.show_all)
                .unwrap_or(false);
            app.close_model_picker();
            Some(AppEvent::SlashCommand {
                action: crate::slash::SlashAction::Model {
                    model_id: Some(id),
                    show_all,
                },
            })
        }
        _ if app
            .keymap
            .matches_in(KeyScope::ModelPicker, Action::PreviousItem, key) =>
        {
            if let Some(ref mut picker) = app.interaction.model_picker {
                picker.selected = picker.selected.saturating_sub(1);
            }
            None
        }
        _ if app
            .keymap
            .matches_in(KeyScope::ModelPicker, Action::NextItem, key) =>
        {
            if let Some(ref mut picker) = app.interaction.model_picker {
                let filtered_len = crate::model_picker::filtered_models(picker).len();
                if picker.selected + 1 < filtered_len {
                    picker.selected += 1;
                }
            }
            None
        }
        _ if app.keymap.matches_in(
            KeyScope::ModelPicker,
            Action::ModelPickerToggleFavourite,
            key,
        ) =>
        {
            toggle_overlay_favourite(app)
        }
        _ if app.keymap.matches_in(
            KeyScope::ModelPicker,
            Action::ModelPickerDeleteAllStale,
            key,
        ) =>
        {
            if let Some(ref mut picker) = app.interaction.model_picker {
                picker.arm_delete_all_stale();
            }
            None
        }
        _ if app.keymap.matches_in(
            KeyScope::ModelPicker,
            Action::ModelPickerDeleteSelectedStale,
            key,
        ) =>
        {
            if let Some(ref mut picker) = app.interaction.model_picker
                && !picker.arm_delete_selected()
            {
                picker.toast = Some("only stale rows can be deleted".into());
            }
            None
        }
        _ if crate::keybinds::is_backspace(key) => {
            if let Some(ref mut picker) = app.interaction.model_picker {
                picker.filter.pop();
                picker.selected = 0;
            }
            None
        }
        _ if let Some(c) = crate::keybinds::typed_char(key) => {
            if let Some(ref mut picker) = app.interaction.model_picker {
                picker.filter.push(c);
                picker.selected = 0;
            }
            None
        }
        _ => None,
    }
}

/// star or un-star the highlighted model when ctrl+f is pressed in
/// the overlay. respects `favourites_locked` (set when the user
/// pinned favourites in config.toml). returns `PersistFavourites`
/// when the list actually changed so the runner saves it
fn toggle_overlay_favourite(app: &mut App) -> Option<AppEvent> {
    let picker = app.interaction.model_picker.as_mut()?;
    if picker.favourites_locked {
        picker.toast = Some("favourites are locked by config.toml".into());
        return None;
    }
    let id = {
        let visible = crate::model_picker::filtered_models(picker);
        visible.get(picker.selected)?.id.clone()
    };
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
    if let Some(ref mut picker) = app.interaction.model_picker {
        picker.favourite_models = crate::model_picker::favourites_by_recency(
            &app.completion.favourite_models,
            &app.completion.recent_models,
        );
    }
    Some(AppEvent::PersistFavourites)
}

/// drive y/n/esc when a delete-stale confirmation is armed in the
/// overlay picker. y persists the deletion to the on-disk discovery
/// cache and refreshes the visible model list; anything else cancels
fn handle_model_picker_delete_confirm_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    let confirm = app
        .interaction
        .model_picker
        .as_ref()
        .and_then(|p| p.confirm_delete.clone())?;

    match (key.modifiers, key.code) {
        _ if matches!(crate::keybinds::any_char(key), Some('y' | 'Y')) => {
            let removed = apply_delete_confirm(&confirm);
            if let Some(picker) = app.interaction.model_picker.as_mut() {
                picker.confirm_delete = None;
                picker.toast = Some(match removed {
                    0 => "delete failed: model not found in cache".into(),
                    1 => "deleted 1 model".into(),
                    n => format!("deleted {n} models"),
                });
            }
            refresh_model_picker_overlay_after_delete(app);
            None
        }
        _ => {
            if let Some(picker) = app.interaction.model_picker.as_mut() {
                picker.cancel_delete();
            }
            None
        }
    }
}

/// replace `app.completion.completions` and
/// `app.completion.model_completions` from a freshly-merged catalogue
/// slice. shared by every code path that needs the slash menu and
/// model picker to reflect new discovery results (delete-stale,
/// `/refresh-models`, post-login refresh) without restarting the app
pub(crate) fn apply_model_completions(app: &mut App, merged: &[mush_ai::discovery::MergedModel]) {
    use crate::slash_menu::model_completion_from_merged;
    let new_completions: Vec<_> = merged.iter().map(model_completion_from_merged).collect();
    app.completion.completions = new_completions.iter().map(|m| m.id.clone()).collect();
    app.completion.model_completions = new_completions;
}

/// load the on-disk discovery cache and apply it to the app's
/// completion state. used after `/refresh-models` and after a login
/// completes so newly-discovered models become selectable immediately
pub(crate) fn reload_model_completions(app: &mut App) {
    let merged = mush_ai::discovery::merged_catalogue();
    apply_model_completions(app, &merged);
    if let Some(picker) = app.interaction.model_picker.as_mut() {
        picker.models = crate::slash_menu::prepare_picker_models(
            &app.completion.model_completions,
            picker.show_all,
        );
        let visible_len = crate::model_picker::filtered_models(picker).len();
        picker.selected = picker.selected.min(visible_len.saturating_sub(1));
    }
}

/// rebuild the model picker's catalogue from the freshly-mutated
/// discovery cache and clamp `selected` so it stays valid when the
/// deleted row vanishes
fn refresh_model_picker_overlay_after_delete(app: &mut App) {
    reload_model_completions(app);

    if let Some(picker) = app.interaction.model_picker.as_mut() {
        picker.models = crate::slash_menu::prepare_picker_models(
            &app.completion.model_completions,
            picker.show_all,
        );
        let visible_len = crate::model_picker::filtered_models(picker).len();
        picker.selected = picker.selected.min(visible_len.saturating_sub(1));
    }
}

/// drive the centred `/login` picker overlay. esc/ctrl+c closes,
/// ctrl+j/k or arrow keys navigate, type to filter, enter on a
/// logged-out row emits a `SlashAction::Login` event for the runner
/// to start the oauth flow, enter on a logged-in row arms a y/n
/// logout confirmation. y while armed deletes the saved credentials
/// in place
fn handle_login_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    // suppress all input while a validation request is in flight. the
    // runner will clear `validating` when it finishes
    if app
        .interaction
        .login_picker
        .as_ref()
        .is_some_and(|p| p.validating.is_some())
    {
        return None;
    }
    // intercept y/n while a logout confirm is armed
    if app
        .interaction
        .login_picker
        .as_ref()
        .is_some_and(|p| p.confirm_logout.is_some())
    {
        return handle_login_picker_logout_confirm_key(app, key);
    }
    // route keystrokes into the api-key buffer while entry mode is
    // active. esc cancels, enter saves to the credential store
    if app
        .interaction
        .login_picker
        .as_ref()
        .is_some_and(|p| p.entry.is_some())
    {
        return handle_login_picker_entry_key(app, key);
    }

    match (key.modifiers, key.code) {
        _ if app
            .keymap
            .matches(crate::keybinds::Action::QuitOrAbort, key)
            || app.keymap.matches(crate::keybinds::Action::Cancel, key) =>
        {
            app.close_login_picker();
            None
        }
        _ if app.keymap.matches(crate::keybinds::Action::Confirm, key) => {
            // resolve the selected row's id + method without holding
            // the immutable borrow into the mutating branches below
            let (id, method, logged_in) = {
                let picker = app.interaction.login_picker.as_ref()?;
                let visible = crate::login_picker::filtered_entries(picker);
                let entry = visible.get(picker.selected)?;
                (entry.id.clone(), entry.method.clone(), entry.logged_in())
            };
            if logged_in {
                if let Some(picker) = app.interaction.login_picker.as_mut() {
                    picker.arm_logout();
                }
                return None;
            }
            // logged-out: oauth rows kick the runner via a SlashAction
            // (so the existing oauth handshake fires). api-key rows arm
            // the in-picker entry prompt instead - no event, no close
            match method {
                mush_ai::login::LoginMethod::OAuth { oauth_provider_id } => {
                    app.close_login_picker();
                    Some(AppEvent::SlashCommand {
                        action: crate::slash::SlashAction::Login {
                            provider: Some(oauth_provider_id),
                        },
                    })
                }
                mush_ai::login::LoginMethod::ApiKey { .. } => {
                    if let Some(picker) = app.interaction.login_picker.as_mut() {
                        picker.arm_entry();
                        picker.toast =
                            Some(format!("paste {id} api key, enter saves, esc cancels"));
                    }
                    None
                }
            }
        }
        _ if app
            .keymap
            .matches(crate::keybinds::Action::PreviousItem, key) =>
        {
            if let Some(picker) = app.interaction.login_picker.as_mut() {
                picker.selected = picker.selected.saturating_sub(1);
            }
            None
        }
        _ if app.keymap.matches(crate::keybinds::Action::NextItem, key) => {
            if let Some(picker) = app.interaction.login_picker.as_mut() {
                let visible_len = crate::login_picker::filtered_entries(picker).len();
                if picker.selected + 1 < visible_len {
                    picker.selected += 1;
                }
            }
            None
        }
        _ if crate::keybinds::is_backspace(key) => {
            if let Some(picker) = app.interaction.login_picker.as_mut() {
                picker.filter.pop();
                picker.selected = 0;
            }
            None
        }
        _ if let Some(c) = crate::keybinds::typed_char(key) => {
            if let Some(picker) = app.interaction.login_picker.as_mut() {
                picker.filter.push(c);
                picker.selected = 0;
            }
            None
        }
        _ => None,
    }
}

/// drive y/n/esc when a logout confirmation is armed in the login
/// picker. y deletes the saved credentials (oauth tokens or stored
/// api key, depending on the row's method) and refreshes the row
/// state in place; anything else cancels the prompt
fn handle_login_picker_logout_confirm_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    let entry_id = app
        .interaction
        .login_picker
        .as_ref()
        .and_then(|p| p.confirm_logout.clone())?;

    match (key.modifiers, key.code) {
        _ if matches!(crate::keybinds::any_char(key), Some('y' | 'Y')) => {
            let toast = perform_logout(app, &entry_id);
            if let Some(picker) = app.interaction.login_picker.as_mut() {
                picker.confirm_logout = None;
                picker.toast = Some(toast);
                refresh_login_picker_logged_in(picker);
            }
            None
        }
        _ => {
            if let Some(picker) = app.interaction.login_picker.as_mut() {
                picker.cancel_logout();
            }
            None
        }
    }
}

/// resolve a picker row's method back to a credential delete and run
/// it. returns the user-facing toast describing the outcome. the row
/// is identified by `entry_id` (matching `LoginEntry::id`)
fn perform_logout(app: &App, entry_id: &str) -> String {
    use mush_ai::login::LoginMethod;

    let Some(method) = app
        .interaction
        .login_picker
        .as_ref()
        .and_then(|p| p.entries.iter().find(|e| e.id == entry_id))
        .map(|e| e.method.clone())
    else {
        return format!("row {entry_id} no longer in picker");
    };

    match method {
        LoginMethod::OAuth { oauth_provider_id } => {
            match mush_ai::oauth::remove_credentials(&oauth_provider_id) {
                Ok(true) => format!("logged out of {entry_id}"),
                Ok(false) => format!("no credentials stored for {entry_id}"),
                Err(e) => format!("logout failed: {e}"),
            }
        }
        LoginMethod::ApiKey { storage_key, .. } => {
            let store = mush_ai::credentials::default_store();
            match store.remove(&storage_key) {
                Ok(true) => format!("removed stored api key for {entry_id}"),
                Ok(false) => format!("no stored key for {entry_id}"),
                Err(e) => format!("logout failed: {e}"),
            }
        }
    }
}

/// drive an api-key entry prompt. typing builds the masked buffer,
/// backspace pops a char, enter hands the buffer to the runner for
/// async validation + save, esc cancels and returns to the picker
fn handle_login_picker_entry_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    match (key.modifiers, key.code) {
        _ if app
            .keymap
            .matches(crate::keybinds::Action::QuitOrAbort, key)
            || app.keymap.matches(crate::keybinds::Action::Cancel, key) =>
        {
            if let Some(picker) = app.interaction.login_picker.as_mut() {
                picker.cancel_entry();
            }
            None
        }
        _ if app.keymap.matches(crate::keybinds::Action::Confirm, key) => {
            submit_login_picker_entry(app)
        }
        _ if crate::keybinds::is_backspace(key) => {
            if let Some(prompt) = app
                .interaction
                .login_picker
                .as_mut()
                .and_then(|p| p.entry.as_mut())
            {
                prompt.buffer.pop();
            }
            None
        }
        _ if let Some(c) = crate::keybinds::typed_char(key) => {
            if let Some(prompt) = app
                .interaction
                .login_picker
                .as_mut()
                .and_then(|p| p.entry.as_mut())
            {
                prompt.buffer.push(c);
            }
            None
        }
        _ => None,
    }
}

/// snapshot the typed key, transition the picker to validating, and
/// emit a [`AppEvent::ValidateAndSaveApiKey`] so the runner can hit
/// the provider asynchronously. empty buffers short-circuit with a
/// toast and no event so we never round-trip a blank key
fn submit_login_picker_entry(app: &mut App) -> Option<AppEvent> {
    let picker = app.interaction.login_picker.as_mut()?;
    let prompt = picker.entry.as_ref()?;
    let key = prompt.buffer.trim().to_string();
    if key.is_empty() {
        picker.toast = Some("key was empty, not saved".into());
        return None;
    }
    let entry_id = prompt.entry_id.clone();
    let provider_name = prompt.provider_name.clone();
    picker.begin_validation(&entry_id, &provider_name);
    Some(AppEvent::ValidateAndSaveApiKey {
        entry_id,
        provider_name,
        key,
    })
}

/// re-read every credential source and update the picker rows so
/// they reflect the new state after a login or logout
fn refresh_login_picker_logged_in(picker: &mut crate::login_picker::LoginPickerState) {
    picker.rebuild_sources();
}

#[cfg(test)]
mod tests;
