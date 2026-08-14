use crossterm::event::KeyEvent;

use crate::app::{App, AppEvent, AppMode};
use crate::session_picker::{SessionScope, filtered_sessions};

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
    if app.interaction.mode == AppMode::Search {
        return handle_search_mode(app, key);
    }
    if app.interaction.mode == AppMode::Settings {
        return handle_settings_menu_key(app, key);
    }
    if app.interaction.mode == AppMode::CompactionRecovery {
        return handle_compaction_recovery_key(app, key);
    }
    if app.interaction.mode == AppMode::Navigate {
        return handle_navigate_key(app, key);
    }

    if app
        .keymap
        .matches(crate::keybinds::Action::QuitOrAbort, key)
    {
        return handle_quit_or_abort(app);
    }
    app.confirm_quit_pending = false;

    if app.keymap.matches(crate::keybinds::Action::PasteImage, key) {
        return Some(AppEvent::PasteImage);
    }
    if is_cancel_key(app, key) {
        if app.is_busy() {
            return Some(AppEvent::Abort);
        }
        app.interaction.mode = AppMode::Navigate;
        app.viewport.pause();
        return None;
    }

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

    if app
        .keymap
        .matches(crate::keybinds::Action::InsertNewline, key)
    {
        app.input_char('\n');
        return None;
    }

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

    if app.stream.active {
        handle_streaming_keys(app, key)
    } else {
        handle_idle_keys(app, key)
    }
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
        if text.starts_with('/') {
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

fn handle_idle_keys(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
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
        .matches(crate::keybinds::Action::CycleThinkingLevel, key)
    {
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
            app.viewport.to_bottom();
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

        _ if app.keymap.matches(crate::keybinds::Action::DeleteChar, key) => {
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
    app.status = Some(format!("slot 1/{slot_count} · tab to cycle, ⏎ to finish"));
}

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

fn handle_at_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    let Some(picker) = app.interaction.at_picker.as_mut() else {
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
        app.status = Some(format!(
            "slot {}/{total} · tab to cycle, ⏎ to finish",
            target + 1
        ));
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

        _ if let Some(c) = crate::keybinds::typed_char(key) => app.input_char(c),

        _ => {}
    }
    None
}

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
            app.interaction.mode = AppMode::Normal;
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
    use crate::keybinds::{Action, KeyScope};

    match () {
        _ if app.keymap.matches(Action::QuitOrAbort, key)
            || app.keymap.matches_in(KeyScope::Normal, Action::Cancel, key) =>
        {
            app.close_slash_menu();
            None
        }
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

fn cycle_favourite_model(app: &mut App, direction: isize) -> Option<AppEvent> {
    let favs = &app.completion.favourite_models;
    if favs.is_empty() {
        app.status =
            Some("no favourite models yet, open /model and ★ some with ctrl+f".to_string());
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
    None
}

fn handle_navigate_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    use crate::keybinds::{Action, KeyScope};

    if app.keymap.matches(Action::QuitOrAbort, key) {
        return handle_quit_or_abort(app);
    }
    if is_cancel_key(app, key) {
        app.interaction.mode = AppMode::Normal;
        app.viewport.follow();
        return None;
    }

    let page = app.viewport.page() as isize;
    let matches = |action| app.keymap.matches_in(KeyScope::Navigate, action, key);
    if matches(Action::PreviousItem) {
        app.viewport.move_cursor(0, -1);
    } else if matches(Action::NextItem) {
        app.viewport.move_cursor(0, 1);
    } else if matches(Action::CursorLeft) {
        app.viewport.move_cursor(-1, 0);
    } else if matches(Action::CursorRight) {
        app.viewport.move_cursor(1, 0);
    } else if matches(Action::LineUp) {
        app.viewport.scroll_by(-1);
    } else if matches(Action::LineDown) {
        app.viewport.scroll_by(1);
    } else if matches(Action::HalfPageUp) {
        app.viewport.page_by(-page / 2);
    } else if matches(Action::HalfPageDown) {
        app.viewport.page_by(page / 2);
    } else if matches(Action::PageUp) {
        app.viewport.page_by(-page);
    } else if matches(Action::PageDown) {
        app.viewport.page_by(page);
    } else if matches(Action::NavigateTop) {
        app.viewport.to_top();
    } else if matches(Action::NavigateBottom) {
        app.viewport.to_bottom();
    }
    None
}

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

fn handle_model_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    use crate::keybinds::{Action, KeyScope};

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

pub(crate) fn apply_model_completions(app: &mut App, merged: &[mush_ai::discovery::MergedModel]) {
    use crate::slash_menu::model_completion_from_merged;
    let new_completions: Vec<_> = merged.iter().map(model_completion_from_merged).collect();
    app.completion.completions = new_completions.iter().map(|m| m.id.clone()).collect();
    app.completion.model_completions = new_completions;
}

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

fn handle_login_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    if app
        .interaction
        .login_picker
        .as_ref()
        .is_some_and(|p| p.validating.is_some())
    {
        return None;
    }
    if app
        .interaction
        .login_picker
        .as_ref()
        .is_some_and(|p| p.confirm_logout.is_some())
    {
        return handle_login_picker_logout_confirm_key(app, key);
    }
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

fn refresh_login_picker_logged_in(picker: &mut crate::login_picker::LoginPickerState) {
    picker.rebuild_sources();
}

#[cfg(test)]
mod tests;
