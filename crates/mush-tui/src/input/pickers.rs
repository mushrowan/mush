use crossterm::event::KeyEvent;

use crate::app::{App, AppEvent};
use crate::filtered_list::FilteredList;
use crate::keybinds::{Action, KeyScope};
use crate::session_picker::SessionScope;

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

pub(super) fn handle_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    if let Some(meta) = app
        .interaction
        .session_picker
        .as_ref()
        .and_then(|picker| picker.confirm_delete.clone())
    {
        return handle_session_picker_delete_confirm_key(app, key, &meta);
    }
    match (key.modifiers, key.code) {
        _ if app.keymap.matches(Action::QuitOrAbort, key)
            || app.keymap.matches(Action::Cancel, key) =>
        {
            app.close_session_picker();
            None
        }
        _ if app.keymap.matches(Action::SessionPickerDelete, key) => {
            if let Some(ref mut picker) = app.interaction.session_picker {
                picker.toast = None;
                picker.arm_delete();
            }
            None
        }
        _ if app.keymap.matches(Action::SessionPickerToggleScope, key) => {
            if let Some(ref mut picker) = app.interaction.session_picker {
                picker.scope = match picker.scope {
                    SessionScope::ThisDir => SessionScope::AllDirs,
                    SessionScope::AllDirs => SessionScope::ThisDir,
                };
                picker.selected = 0;
            }
            None
        }
        _ if app.keymap.matches(Action::Confirm, key) => {
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
        _ if app
            .keymap
            .matches_in(KeyScope::SessionPicker, Action::PreviousItem, key) =>
        {
            if let Some(ref mut picker) = app.interaction.session_picker {
                picker.select_previous();
            }
            None
        }
        _ if app
            .keymap
            .matches_in(KeyScope::SessionPicker, Action::NextItem, key) =>
        {
            if let Some(ref mut picker) = app.interaction.session_picker {
                picker.select_next();
            }
            None
        }
        _ if crate::keybinds::is_backspace(key) => {
            if let Some(ref mut picker) = app.interaction.session_picker {
                picker.pop_filter();
            }
            None
        }
        _ if let Some(c) = crate::keybinds::typed_char(key) => {
            if let Some(ref mut picker) = app.interaction.session_picker {
                picker.push_filter(c);
            }
            None
        }
        _ => None,
    }
}

pub(super) fn handle_message_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
    let picker = app.interaction.message_picker.as_mut()?;
    if app.keymap.matches(Action::QuitOrAbort, key) || app.keymap.matches(Action::Cancel, key) {
        if picker.filter.is_empty() {
            app.close_message_picker();
        } else {
            picker.clear_filter();
        }
        return None;
    }
    if app.keymap.matches(Action::Confirm, key) {
        let chosen = picker
            .selected_item()
            .map(|item| (picker.purpose, item.index, item.entry_id.clone()));
        app.close_message_picker();
        let (purpose, index, entry_id) = chosen?;
        let action = match purpose {
            crate::message_picker::MessagePickerPurpose::Fork => {
                crate::slash::SlashAction::Fork { index: Some(index) }
            }
            crate::message_picker::MessagePickerPurpose::Branch => {
                crate::slash::SlashAction::Branch { index: Some(index) }
            }
            crate::message_picker::MessagePickerPurpose::Tree => {
                crate::slash::SlashAction::NavigateTree {
                    entry_id: entry_id?,
                }
            }
        };
        return Some(AppEvent::SlashCommand { action });
    }
    if app
        .keymap
        .matches_in(KeyScope::SessionPicker, Action::PreviousItem, key)
    {
        picker.select_previous();
    } else if app
        .keymap
        .matches_in(KeyScope::SessionPicker, Action::NextItem, key)
    {
        picker.select_next();
    } else if crate::keybinds::is_backspace(key) {
        picker.pop_filter();
    } else if let Some(c) = crate::keybinds::typed_char(key) {
        picker.push_filter(c);
    }
    None
}

fn handle_session_picker_delete_confirm_key(
    app: &mut App,
    key: KeyEvent,
    meta: &mush_session::SessionMeta,
) -> Option<AppEvent> {
    let picker = app.interaction.session_picker.as_mut()?;
    picker.cancel_delete();
    if !matches!(crate::keybinds::any_char(key), Some('y' | 'Y')) {
        return None;
    }
    let store = mush_session::SessionStore::new(mush_session::SessionStore::default_dir());
    if store.is_leased(&meta.id) {
        picker.toast = Some("that session is open elsewhere".into());
        return None;
    }
    match store.delete(&meta.id) {
        Ok(()) => {
            picker.remove(&meta.id);
            picker.toast = Some(format!(
                "deleted {}",
                meta.title.as_deref().unwrap_or("(untitled)")
            ));
        }
        Err(error) => picker.toast = Some(format!("delete failed: {error}")),
    }
    None
}

pub(super) fn handle_model_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
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
            let picker = app.interaction.model_picker.as_ref();
            let show_all = picker.map(|p| p.show_all).unwrap_or(false);
            let subagent = picker
                .is_some_and(|p| p.purpose == crate::model_picker::ModelPickerPurpose::Subagent);
            app.close_model_picker();
            if subagent {
                let unset = id == crate::model_picker::INHERIT_ID;
                return Some(AppEvent::SlashCommand {
                    action: crate::slash::SlashAction::SubagentModel {
                        model_id: (!unset).then_some(id),
                        unset,
                    },
                });
            }
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
                picker.select_previous();
            }
            None
        }
        _ if app
            .keymap
            .matches_in(KeyScope::ModelPicker, Action::NextItem, key) =>
        {
            if let Some(ref mut picker) = app.interaction.model_picker {
                picker.select_next();
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
                picker.pop_filter();
            }
            None
        }
        _ if let Some(c) = crate::keybinds::typed_char(key) => {
            if let Some(ref mut picker) = app.interaction.model_picker {
                picker.push_filter(c);
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
            reload_model_completions(app);
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

pub(super) fn handle_login_picker_key(app: &mut App, key: KeyEvent) -> Option<AppEvent> {
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
        _ if app.keymap.matches(Action::QuitOrAbort, key)
            || app.keymap.matches(Action::Cancel, key) =>
        {
            app.close_login_picker();
            None
        }
        _ if app.keymap.matches(Action::Confirm, key) => {
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
                            account: None,
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
        _ if app.keymap.matches(Action::PreviousItem, key) => {
            if let Some(picker) = app.interaction.login_picker.as_mut() {
                picker.select_previous();
            }
            None
        }
        _ if app.keymap.matches(Action::NextItem, key) => {
            if let Some(picker) = app.interaction.login_picker.as_mut() {
                picker.select_next();
            }
            None
        }
        _ if crate::keybinds::is_backspace(key) => {
            if let Some(picker) = app.interaction.login_picker.as_mut() {
                picker.pop_filter();
            }
            None
        }
        _ if let Some(c) = crate::keybinds::typed_char(key) => {
            if let Some(picker) = app.interaction.login_picker.as_mut() {
                picker.push_filter(c);
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
                picker.rebuild_sources();
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
            match mush_ai::oauth::remove_credentials(&oauth_provider_id, None) {
                Ok(Some(label)) => format!("logged out of {entry_id} ({label})"),
                Ok(None) => format!("no credentials stored for {entry_id}"),
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
        _ if app.keymap.matches(Action::QuitOrAbort, key)
            || app.keymap.matches(Action::Cancel, key) =>
        {
            if let Some(picker) = app.interaction.login_picker.as_mut() {
                picker.cancel_entry();
            }
            None
        }
        _ if app.keymap.matches(Action::Confirm, key) => submit_login_picker_entry(app),
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
