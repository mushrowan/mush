use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use crate::app::{App, AppEvent, AppMode};
use crate::input_buffer::InputBuffer;
use crate::keybinds::{Action, KeyScope};
use crate::normal::NormalTarget;

fn should_escape_enter_to_newline(app: &App, key: KeyEvent) -> bool {
    let (before, after) = app.input.text.split_at(app.input.cursor);
    crate::keybinds::is_plain_enter(key)
        && before.ends_with('\\')
        && (after.is_empty() || after.starts_with('\n'))
}

pub fn handle_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    if app.subagents.handle_key(&app.keymap, key) {
        return None;
    }
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

    match app.interaction.mode {
        AppMode::SessionPicker => return handle_picker_key(app, key),
        AppMode::MessagePicker => return handle_message_picker_key(app, key),
        AppMode::ModelPicker => return handle_model_picker_key(app, key),
        AppMode::LoginPicker => return handle_login_picker_key(app, key),
        AppMode::SlashComplete => return handle_slash_menu_key(app, key),
        AppMode::AtPicker => return handle_at_picker_key(app, key),
        AppMode::SlotEdit => return handle_slot_edit_key(app, key),
        AppMode::Search => return handle_search_mode(app, key),
        AppMode::Settings => return handle_settings_menu_key(app, key),
        AppMode::CompactionRecovery => return handle_compaction_recovery_key(app, key),
        AppMode::Visual => return handle_visual_key(app, key),
        AppMode::Normal if app.interaction.focus.is_log() => {
            return handle_navigate_key(app, key);
        }
        AppMode::Normal => return handle_composer_normal_key(app, key),
        AppMode::Insert | AppMode::ToolConfirm => {}
    }

    if app.keymap.matches(Action::QuitOrAbort, key) {
        return handle_quit_or_abort(app);
    }
    app.quit_hint_until = None;

    if app.keymap.matches(Action::PasteImage, key) {
        return Some(AppEvent::PasteImage);
    }
    if app.keymap.matches(Action::ExternalEditor, key) {
        return Some(AppEvent::ExternalEditor);
    }
    if is_cancel_key(app, key) {
        app.interaction.mode = AppMode::Normal;
        app.input.step_back_one_char();
        return None;
    }
    if app.keymap.matches(Action::FocusLog, key) {
        return focus_log(app);
    }
    if app.keymap.matches(Action::CycleDetail, key) {
        app.cycle_detail();
        return None;
    }
    if app.keymap.matches(Action::SelectAll, key) {
        return select_all_composer(app);
    }

    if app.keymap.matches(Action::CycleFavouriteBackward, key) {
        return cycle_favourite_model(app, -1);
    }
    if app.keymap.matches(Action::CycleFavourite, key) {
        return cycle_favourite_model(app, 1);
    }

    if app.keymap.matches(Action::InsertNewline, key) {
        app.input_char('\n');
        return None;
    }

    if app.has_queued_messages() && app.keymap.matches(Action::EditSteering, key) {
        let is_alt_combo = crate::keybinds::has_alt(key);
        if is_alt_combo || app.input.text.is_empty() {
            return Some(AppEvent::EditSteering);
        }
    }

    if app.stream.active {
        handle_streaming_keys(app, key)
    } else {
        handle_idle_keys(app, key)
    }
}

pub fn handle_mouse(app: &mut App, event: MouseEvent) -> Option<AppEvent> {
    if app.subagents.open {
        return None;
    }
    if !app.interaction.mode.scrolls_the_log() {
        return None;
    }
    let over_log = app.viewport.area().contains(ratatui::layout::Position {
        x: event.column,
        y: event.row,
    });
    if !over_log {
        return None;
    }
    let lines = isize::try_from(app.interaction.mouse_scroll_lines).unwrap_or(1);
    let notch = match event.kind {
        MouseEventKind::ScrollUp => -lines,
        MouseEventKind::ScrollDown => lines,
        _ => return None,
    };
    app.viewport.scroll_by(notch);
    None
}

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
        app.completion.history = None;
        match classify_submission(app, &text) {
            Submission::Prompt => {}
            Submission::Command(action) if action.is_safe_during_stream() => {
                return Some(AppEvent::SlashCommand { action });
            }
            Submission::Command(_) => {
                restore_input(app, text);
                app.status = Some(
                    "this slash command is blocked while streaming; press esc to abort first"
                        .into(),
                );
                return None;
            }
            Submission::Refused => return None,
        }
        return Some(AppEvent::UserSubmit {
            text,
            images: app.input.take_images(),
        });
    }

    handle_editing(app, key)
}

enum Submission {
    Prompt,
    Command(crate::slash::SlashAction),
    Refused,
}

fn classify_submission(app: &mut App, text: &str) -> Submission {
    if !text.starts_with('/') {
        return Submission::Prompt;
    }
    match crate::slash::parse(text) {
        Ok(crate::slash::SlashAction::Other { name, .. }) if !app.knows_slash_command(&name) => {
            let Some(suggestion) = app.suggest_slash_command(&name) else {
                return Submission::Prompt;
            };
            restore_input(app, text.to_string());
            app.status =
                Some(format!("Unknown command: /{name}. Did you mean /{suggestion}?").into());
            Submission::Refused
        }
        Ok(action) => Submission::Command(action),
        Err(error) => {
            app.push_system_message(error.to_string());
            Submission::Refused
        }
    }
}

fn restore_input(app: &mut App, text: String) {
    app.input.text = text;
    app.input.cursor = app.input.text.len();
    app.input.ensure_cursor_visible();
}

fn handle_idle_keys(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    if app.keymap.matches(Action::EnterSearch, key) {
        app.interaction.mode = AppMode::Search;
        app.interaction.search.query.clear();
        app.interaction.search.matches.clear();
        app.interaction.search.selected = 0;
        return None;
    }
    if app.keymap.matches(Action::CycleThinkingLevel, key) {
        app.cycle_thinking_level();
        return Some(AppEvent::CycleThinkingLevel);
    }

    match () {
        _ if crate::keybinds::is_tab(key) || crate::keybinds::is_backtab(key) => {
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

        _ if app.keymap.matches(Action::Confirm, key) => {
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
            app.viewport.to_bottom();
            let text = app.input.take_text();
            app.completion.history = None;
            match classify_submission(app, &text) {
                Submission::Prompt => {}
                Submission::Command(action) => return Some(AppEvent::SlashCommand { action }),
                Submission::Refused => return None,
            }
            Some(AppEvent::UserSubmit {
                text,
                images: app.input.take_images(),
            })
        }

        _ if app.keymap.matches(Action::DeleteChar, key) => {
            app.input.delete();
            None
        }

        _ => handle_editing(app, key),
    }
}

fn try_expand_at_template(app: &mut App) -> bool {
    let Some(trigger) = crate::at_template::parse_at_trigger(&app.input.text, app.input.cursor)
    else {
        return false;
    };
    let Some(template) = crate::at_template::find_exact(&app.completion.templates, &trigger) else {
        return false;
    };
    let content = template.content.clone();
    let end = app.input.cursor;
    insert_template(app, trigger.start, end, &content);
    true
}

fn insert_template(app: &mut App, start: usize, end: usize, content: &str) {
    let (clean, slot_offsets) = crate::at_template::strip_placeholders(content);
    app.input.text.replace_range(start..end, &clean);

    if slot_offsets.is_empty() {
        app.input.cursor = start + clean.len();
        app.input.ensure_cursor_visible();
        return;
    }

    let slots: Vec<usize> = slot_offsets.iter().map(|&o| start + o).collect();
    app.input.cursor = slots[0];
    app.input.ensure_cursor_visible();
    let slot_count = slots.len();
    app.interaction.slot_edit = Some(crate::app_state::SlotEditState { slots, current: 0 });
    app.interaction.mode = AppMode::SlotEdit;
    app.status = Some(format!("slot 1/{slot_count} · tab to cycle, ⏎ to finish").into());
}

fn try_open_at_picker(app: &mut App) -> bool {
    let Some(trigger) = crate::at_template::parse_at_trigger(&app.input.text, app.input.cursor)
    else {
        return false;
    };
    let matches = at_template_matches(app, &trigger);
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

fn at_template_matches(
    app: &App,
    trigger: &crate::at_template::AtTrigger,
) -> Vec<mush_ext::PromptTemplate> {
    crate::at_template::find_prefix(&app.completion.templates, trigger)
        .into_iter()
        .cloned()
        .collect()
}

fn close_at_picker(app: &mut App) {
    app.interaction.at_picker = None;
    app.interaction.mode = AppMode::Insert;
}

fn handle_at_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    let Some(picker) = app.interaction.at_picker.as_mut() else {
        app.interaction.mode = AppMode::Insert;
        return None;
    };
    let count = picker.matches.len();
    match () {
        _ if app
            .keymap
            .matches_in(KeyScope::AtPicker, Action::PreviousItem, key) =>
        {
            picker.selected = (picker.selected + count - 1) % count;
            None
        }
        _ if app
            .keymap
            .matches_in(KeyScope::AtPicker, Action::NextItem, key) =>
        {
            picker.selected = (picker.selected + 1) % count;
            None
        }
        _ if app
            .keymap
            .matches_in(KeyScope::AtPicker, Action::Confirm, key) =>
        {
            let template = picker.matches[picker.selected].clone();
            let start = picker.trigger_start;
            let end = picker.trigger_end;
            close_at_picker(app);
            insert_template(app, start, end, &template.content);
            None
        }
        _ if app
            .keymap
            .matches_in(KeyScope::AtPicker, Action::Cancel, key) =>
        {
            close_at_picker(app);
            None
        }
        _ if crate::keybinds::is_backspace(key) => {
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

fn refresh_at_picker(app: &mut App) {
    let Some(trigger) = crate::at_template::parse_at_trigger(&app.input.text, app.input.cursor)
    else {
        close_at_picker(app);
        return;
    };
    let matches = at_template_matches(app, &trigger);
    if matches.is_empty() {
        close_at_picker(app);
        return;
    }
    if let Some(picker) = app.interaction.at_picker.as_mut() {
        picker.trigger_end = app.input.cursor;
        picker.selected = picker.selected.min(matches.len().saturating_sub(1));
        picker.matches = matches;
    }
}

fn handle_slot_edit_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    let Some(state) = app.interaction.slot_edit.as_ref() else {
        app.interaction.mode = AppMode::Insert;
        return None;
    };
    let count = state.slots.len();
    match () {
        _ if app
            .keymap
            .matches_in(KeyScope::SlotEdit, Action::PreviousItem, key) =>
        {
            let next = (state.current + count - 1) % count;
            jump_to_slot(app, next);
            None
        }
        _ if app
            .keymap
            .matches_in(KeyScope::SlotEdit, Action::NextItem, key) =>
        {
            let next = (state.current + 1) % count;
            jump_to_slot(app, next);
            None
        }
        _ if app
            .keymap
            .matches_in(KeyScope::SlotEdit, Action::Confirm, key)
            || app
                .keymap
                .matches_in(KeyScope::SlotEdit, Action::Cancel, key) =>
        {
            app.interaction.slot_edit = None;
            app.interaction.mode = AppMode::Insert;
            None
        }
        _ if crate::keybinds::is_backspace(key) => {
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
        _ => None,
    }
}

fn jump_to_slot(app: &mut App, target: usize) {
    if let Some(state) = app.interaction.slot_edit.as_mut() {
        state.current = target;
        let position = state.slots[target];
        let total = state.slots.len();
        app.input.cursor = position;
        app.input.ensure_cursor_visible();
        app.status =
            Some(format!("slot {}/{total} · tab to cycle, ⏎ to finish", target + 1).into());
    }
}

fn shift_later_slots(app: &mut App, cutoff: usize, delta: isize) {
    if let Some(state) = app.interaction.slot_edit.as_mut() {
        for slot in &mut state.slots {
            if *slot > cutoff {
                *slot = (*slot as isize + delta) as usize;
            }
        }
    }
}

type EditCommand = fn(&mut InputBuffer);

const EDIT_COMMANDS: &[(Action, EditCommand)] = &[
    (
        Action::DeleteWordBackward,
        InputBuffer::delete_word_backward,
    ),
    (Action::DeleteWordForward, InputBuffer::delete_word_forward),
    (Action::DeleteToStart, InputBuffer::delete_to_start),
    (Action::DeleteToEnd, InputBuffer::delete_to_end),
    (Action::CursorWordLeft, InputBuffer::cursor_word_left),
    (Action::CursorWordRight, InputBuffer::cursor_word_right),
    (Action::CursorLineStart, InputBuffer::cursor_home),
    (Action::CursorLineEnd, InputBuffer::cursor_end),
];

fn handle_editing(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    if let Some((_, edit)) = EDIT_COMMANDS
        .iter()
        .find(|(action, _)| app.keymap.matches(*action, key))
    {
        edit(&mut app.input);
        return None;
    }

    match (key.modifiers, key.code) {
        _ if crate::keybinds::is_backspace(key) => app.input.backspace(),
        _ if crate::keybinds::is_delete(key) => app.input.delete(),

        _ if app
            .keymap
            .matches_in(KeyScope::Edit, Action::CursorLeft, key) =>
        {
            app.input.cursor_left()
        }
        _ if app.keymap.matches_in(KeyScope::Edit, Action::CursorUp, key) => {
            if !app.input.cursor_up() {
                app.history_previous();
            }
        }
        _ if app
            .keymap
            .matches_in(KeyScope::Edit, Action::CursorDown, key) =>
        {
            if !app.input.cursor_down() {
                app.history_next();
            }
        }
        _ if app
            .keymap
            .matches_in(KeyScope::Edit, Action::CursorRight, key) =>
        {
            app.input.cursor_right()
        }

        _ if let Some(c) = crate::keybinds::typed_char(key) => app.input_char(c),

        _ => {}
    }
    None
}

fn handle_settings_menu_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    let Some(menu) = app.settings_menu.as_mut() else {
        app.interaction.mode = AppMode::Insert;
        return None;
    };
    if app.keymap.matches(Action::QuitOrAbort, key)
        || app
            .keymap
            .matches_in(KeyScope::Settings, Action::Cancel, key)
    {
        app.settings_menu = None;
        app.interaction.mode = AppMode::Insert;
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
    let Some(recovery) = app.interaction.compaction_recovery.as_mut() else {
        app.interaction.mode = AppMode::Insert;
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

fn handle_search_mode(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    match () {
        _ if app.keymap.matches(Action::QuitOrAbort, key) => return Some(AppEvent::Quit),
        _ if app.keymap.matches_in(KeyScope::Search, Action::Cancel, key)
            || app.keymap.matches(Action::EnterSearch, key)
            || app
                .keymap
                .matches_in(KeyScope::Search, Action::Confirm, key) =>
        {
            app.interaction.mode = AppMode::Insert;
            app.interaction.search.query.clear();
            app.interaction.search.matches.clear();
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

fn handle_slash_menu_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    match () {
        _ if app.keymap.matches(Action::QuitOrAbort, key)
            || app.keymap.matches_in(KeyScope::Insert, Action::Cancel, key) =>
        {
            app.close_slash_menu();
            None
        }
        _ if app
            .keymap
            .matches_in(KeyScope::Insert, Action::Confirm, key) =>
        {
            let chosen = app
                .completion
                .slash_menu
                .as_ref()
                .and_then(|menu| menu.matches.get(menu.selected))
                .map(|cmd| cmd.name.clone());
            app.close_slash_menu();
            let name = chosen?;
            app.input.text = format!("/{name}");
            app.input.cursor = app.input.text.len();
            app.input.ensure_cursor_visible();
            if !name.contains(' ') {
                return None;
            }
            let text = app.input.take_text();
            app.completion.history = None;
            match classify_submission(app, &text) {
                Submission::Command(action) => Some(AppEvent::SlashCommand { action }),
                Submission::Prompt | Submission::Refused => None,
            }
        }
        _ if app
            .keymap
            .matches_in(KeyScope::Insert, Action::NextItem, key) =>
        {
            if let Some(ref mut menu) = app.completion.slash_menu {
                let len = menu.matches.len();
                if len > 0 {
                    menu.selected = (menu.selected + 1) % len;
                }
            }
            None
        }
        _ if app
            .keymap
            .matches_in(KeyScope::Insert, Action::PreviousItem, key) =>
        {
            if let Some(ref mut menu) = app.completion.slash_menu {
                let len = menu.matches.len();
                if len > 0 {
                    menu.selected = (menu.selected + len - 1) % len;
                }
            }
            None
        }
        _ if crate::keybinds::is_backspace(key) => {
            app.input.backspace();
            if app.input.text.is_empty() || !app.input.text.starts_with('/') {
                app.close_slash_menu();
            } else {
                app.update_slash_menu();
            }
            None
        }
        _ if let Some(c) = crate::keybinds::typed_char(key) => {
            app.input_char(c);
            app.update_slash_menu();
            None
        }
        _ => None,
    }
}

fn cycle_favourite_model(app: &mut App, direction: isize) -> Option<AppEvent> {
    let favs = &app.completion.favourite_models;
    if favs.is_empty() {
        app.status = Some("no favourite models yet, open /model and ★ some with ctrl+f".into());
        return None;
    }
    let current = favs.iter().position(|id| id == app.model_id.as_str());
    let len = favs.len();
    let next_idx = match (current, direction) {
        (Some(i), d) => (i as isize + d).rem_euclid(len as isize) as usize,
        (None, d) if d > 0 => 0,
        (None, _) => len - 1,
    };
    let next_id = favs[next_idx].clone();
    if next_id == app.model_id.as_str() {
        return None;
    }
    Some(AppEvent::ModelSelected { model_id: next_id })
}

fn handle_quit_or_abort(app: &mut App) -> Option<AppEvent> {
    if app.quit_hint_armed() {
        return Some(AppEvent::Quit);
    }
    app.arm_quit_hint();
    if app.is_busy() {
        return Some(AppEvent::Abort);
    }
    app.status = Some("press ctrl+c again to exit".into());
    None
}

fn select_all_composer(app: &mut App) -> Option<AppEvent> {
    if app.input.text.is_empty() {
        return None;
    }
    crate::normal::ComposerTarget::new(&mut app.input).select_all();
    app.interaction.mode = AppMode::Visual;
    app.interaction.focus = crate::app_event::Focus::Composer;
    None
}

fn composer_selected_text(app: &App, strip_indent: bool) -> Option<String> {
    let selected = app.input.selected_bytes()?;
    let text = app.input.text.get(selected)?.to_string();
    Some(if strip_indent {
        crate::selection::strip_padding(&text)
    } else {
        text
    })
}

fn clear_selection(app: &mut App) {
    if app.interaction.focus.is_log() {
        app.viewport.clear_selection();
    } else {
        crate::normal::ComposerTarget::new(&mut app.input).clear_selection();
    }
}

fn focus_composer(app: &mut App, insert: bool) -> Option<AppEvent> {
    app.interaction.focus = crate::app_event::Focus::Composer;
    app.interaction.mode = if insert {
        AppMode::Insert
    } else {
        AppMode::Normal
    };
    app.viewport.resume();
    if insert {
        app.input.cursor = app.input.text.len();
    }
    None
}

fn focus_log(app: &mut App) -> Option<AppEvent> {
    app.interaction.focus = crate::app_event::Focus::Log;
    app.interaction.mode = AppMode::Normal;
    app.viewport.pause();
    None
}

fn handle_composer_normal_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    if app.keymap.matches(Action::QuitOrAbort, key) {
        return handle_quit_or_abort(app);
    }
    if is_cancel_key(app, key) || app.keymap.matches(Action::FocusLog, key) {
        return focus_log(app);
    }
    if app
        .keymap
        .matches_in(KeyScope::Normal, Action::EnterInsert, key)
    {
        app.interaction.mode = AppMode::Insert;
        return None;
    }
    if app.keymap.matches(Action::SelectAll, key) {
        return select_all_composer(app);
    }
    if let Some(kind) = selection_kind_for(app, key) {
        crate::normal::ComposerTarget::new(&mut app.input).start_selection(kind);
        app.interaction.mode = AppMode::Visual;
        return None;
    }
    with_count(app, key, run_composer_motion);
    None
}

fn resolve_motion(app: &App, key: KeyEvent) -> Option<Action> {
    [
        Action::LineUp,
        Action::LineDown,
        Action::HalfPageUp,
        Action::HalfPageDown,
        Action::PageUp,
        Action::PageDown,
        Action::CursorLineStart,
        Action::CursorFirstNonBlank,
        Action::CursorLineEnd,
        Action::CursorLeft,
        Action::CursorRight,
        Action::PreviousItem,
        Action::NextItem,
        Action::NavigateTop,
        Action::NavigateBottom,
        Action::CursorWordForward,
        Action::CursorChunkForward,
        Action::CursorWordBack,
        Action::CursorChunkBack,
        Action::CursorWordEnd,
        Action::CursorChunkEnd,
    ]
    .into_iter()
    .find(|action| app.keymap.matches_in(KeyScope::Normal, *action, key))
}

fn with_count(app: &mut App, key: KeyEvent, run: impl Fn(&mut App, Action)) {
    if take_count_digit(app, key) {
        return;
    }
    let count = app.interaction.pending_count.take().unwrap_or(1);
    let Some(action) = resolve_motion(app, key) else {
        return;
    };
    for _ in 0..count {
        run(app, action);
    }
}

fn handle_navigate_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    if app.keymap.matches(Action::QuitOrAbort, key) {
        return handle_quit_or_abort(app);
    }
    if is_cancel_key(app, key) {
        return None;
    }
    if app.keymap.matches(Action::FocusLog, key) {
        return focus_composer(app, false);
    }
    if app.keymap.matches(Action::CycleDetail, key) {
        app.cycle_detail();
        return None;
    }
    if app
        .keymap
        .matches_in(KeyScope::Normal, Action::EnterInsert, key)
    {
        return focus_composer(app, true);
    }
    if let Some(kind) = selection_kind_for(app, key) {
        app.viewport.start_selection(kind);
        app.interaction.mode = AppMode::Visual;
        return None;
    }
    handle_log_motion(app, key);
    None
}

fn handle_visual_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    if app.keymap.matches(Action::QuitOrAbort, key) {
        return handle_quit_or_abort(app);
    }
    if is_cancel_key(app, key) {
        clear_selection(app);
        app.interaction.mode = AppMode::Normal;
        return None;
    }
    if !app.interaction.focus.is_log() {
        if crate::keybinds::is_backspace(key)
            || (key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::NONE)
        {
            crate::normal::ComposerTarget::new(&mut app.input).delete_selection();
            app.interaction.mode = AppMode::Insert;
            app.input.ensure_cursor_visible();
            return None;
        }
        for (action, strip_indent) in [(Action::Yank, true), (Action::YankRaw, false)] {
            if app.keymap.matches_in(KeyScope::Normal, action, key) {
                let text = composer_selected_text(app, strip_indent)?;
                crate::normal::ComposerTarget::new(&mut app.input).clear_selection();
                app.interaction.mode = AppMode::Normal;
                return Some(AppEvent::Copy { text });
            }
        }
        with_count(app, key, run_composer_motion);
        return None;
    }
    for (action, strip_indent) in [(Action::Yank, true), (Action::YankRaw, false)] {
        if app.keymap.matches_in(KeyScope::Normal, action, key) {
            let text = crate::selection::selected_text(app, strip_indent)?;
            app.viewport.clear_selection();
            app.interaction.mode = AppMode::Normal;
            return Some(AppEvent::Copy { text });
        }
    }
    handle_log_motion(app, key);
    None
}

fn selection_kind_for(app: &App, key: KeyEvent) -> Option<crate::screen::SelectionKind> {
    use crate::screen::SelectionKind;

    [
        (Action::StartSelection, SelectionKind::Char),
        (Action::StartLineSelection, SelectionKind::Line),
        (Action::StartBlockSelection, SelectionKind::Block),
    ]
    .into_iter()
    .find(|(action, _)| app.keymap.matches_in(KeyScope::Normal, *action, key))
    .map(|(_, kind)| kind)
}

fn handle_log_motion(app: &mut App, key: KeyEvent) {
    with_count(app, key, run_log_motion);
}

fn take_count_digit(app: &mut App, key: KeyEvent) -> bool {
    let KeyCode::Char(c) = key.code else {
        return false;
    };
    if !key.modifiers.is_empty() {
        return false;
    }
    let Some(digit) = c.to_digit(10).map(|d| d as usize) else {
        return false;
    };
    let pending = app.interaction.pending_count;
    if digit == 0 && pending.is_none() {
        return false;
    }
    app.interaction.pending_count = Some(pending.unwrap_or(0) * 10 + digit);
    true
}

fn run_composer_motion(app: &mut App, action: Action) {
    crate::normal::apply_motion(
        &mut crate::normal::ComposerTarget::new(&mut app.input),
        action,
    );
}

fn run_log_motion(app: &mut App, action: Action) {
    let page = app.viewport.page() as isize;
    match action {
        Action::LineUp => app.viewport.scroll_by(-1),
        Action::LineDown => app.viewport.scroll_by(1),
        Action::HalfPageUp => app.viewport.page_by(-page / 2),
        Action::HalfPageDown => app.viewport.page_by(page / 2),
        Action::PageUp => app.viewport.page_by(-page),
        Action::PageDown => app.viewport.page_by(page),
        action => {
            crate::normal::apply_motion(&mut crate::normal::LogTarget::new(app), action);
        }
    }
}

fn is_cancel_key(app: &App, key: KeyEvent) -> bool {
    app.keymap.matches(Action::Cancel, key)
}

mod pickers;

pub(crate) use pickers::reload_model_completions;
use pickers::{
    handle_login_picker_key, handle_message_picker_key, handle_model_picker_key, handle_picker_key,
};

#[cfg(test)]
mod tests;
