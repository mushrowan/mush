use std::io;
use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use mush_ai::registry::ApiRegistry;

use crate::app::{self, App, AppEvent};
use crate::input::handle_key;
use crate::pane::PaneManager;

use super::commands::{SlashEnv, handle_slash_action, save_thinking_pref};
use super::panes::{close_focused_pane, fork_pane};
use super::render::handle_mouse;
use super::streams::{
    StreamState, abort_focused_stream, answer_confirmation, edit_last_queued_steering,
    submit_streaming_input,
};
use super::{ThinkingPrefs, ThinkingPrefsSaver, TuiConfig};
use crate::slash;

pub(super) enum LoopAction {
    /// nothing changed, skip redraw
    Continue,
    /// state changed, redraw needed
    Redraw,
    Quit,
}

pub(super) struct InputDeps<'a> {
    pub tui_config: &'a mut TuiConfig,
    pub thinking_prefs: &'a mut ThinkingPrefs,
    pub thinking_saver: &'a Option<ThinkingPrefsSaver>,
    pub registry: &'a ApiRegistry,
    pub message_bus: &'a crate::messaging::MessageBus,
    pub file_tracker: &'a crate::file_tracker::FileTracker,
    pub lifecycle_hooks: &'a mush_agent::LifecycleHooks,
    pub cwd: &'a Path,
    pub pending_prompt: &'a mut Option<String>,
    pub pending_compactions:
        &'a mut std::collections::HashMap<crate::pane::PaneId, crate::slash::PendingCompaction>,
    pub delegation_queue: &'a crate::delegate::DelegationQueue,
    pub image_picker: &'a Option<ratatui_image::picker::Picker>,
    /// handle to the Terminal backend's size cache so resize events
    /// can invalidate it (ratatui's autoresize will then pick up the
    /// new dimensions on the next frame)
    pub size_cache: &'a std::rc::Rc<super::caching_backend::CachedSizeState>,
}

fn confirmation_answer(code: KeyCode) -> Option<bool> {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(true),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
        _ => None,
    }
}

fn trace_dropped_event(event: Event, phase: &str) {
    match event {
        Event::Key(key) => {
            // non-press key events are suspicious with our keyboard enhancement
            // setup (we only request DISAMBIGUATE_ESCAPE_CODES, not
            // REPORT_EVENT_TYPES) so log at debug to help diagnose misparsed
            // escape sequences
            tracing::debug!(
                %phase,
                code = ?key.code,
                modifiers = ?key.modifiers,
                kind = ?key.kind,
                "dropped non-press key event"
            );
        }
        Event::FocusGained => {
            tracing::debug!(%phase, "focus gained");
        }
        Event::FocusLost => {
            tracing::debug!(%phase, "focus lost");
        }
        Event::Resize(w, h) => {
            tracing::debug!(%phase, width = w, height = h, "terminal resized");
        }
        event => {
            tracing::debug!(%phase, ?event, "unhandled terminal event");
        }
    }
}

async fn handle_common_app_event(
    app_event: &AppEvent,
    pane_mgr: &mut PaneManager,
    stream_state: Option<&mut StreamState>,
    deps: &mut InputDeps<'_>,
) -> Option<LoopAction> {
    match app_event {
        AppEvent::Quit => Some(LoopAction::Quit),
        AppEvent::CycleThinkingLevel => {
            let app = &pane_mgr.focused().app;
            save_thinking_pref(
                deps.thinking_prefs,
                deps.thinking_saver,
                deps.cwd,
                &app.model_id,
                app.thinking_level,
            );
            Some(LoopAction::Redraw)
        }
        AppEvent::PasteImage => {
            paste_clipboard_image(&mut pane_mgr.focused_mut().app).await;
            Some(LoopAction::Redraw)
        }
        AppEvent::SplitPane => {
            fork_pane(
                pane_mgr,
                deps.tui_config,
                deps.message_bus,
                &deps.tui_config.tool_output_live,
            )
            .await;
            Some(LoopAction::Redraw)
        }
        AppEvent::ClosePane => {
            close_focused_pane(
                pane_mgr,
                stream_state,
                deps.message_bus,
                deps.file_tracker,
                deps.cwd,
            )
            .await;
            Some(LoopAction::Redraw)
        }
        AppEvent::FocusNextPane => {
            pane_mgr.focus_next();
            Some(LoopAction::Redraw)
        }
        AppEvent::FocusPrevPane => {
            pane_mgr.focus_prev();
            Some(LoopAction::Redraw)
        }
        AppEvent::FocusPaneByIndex(index) => {
            pane_mgr.focus_index(*index);
            Some(LoopAction::Redraw)
        }
        AppEvent::ResizePane(delta) => {
            pane_mgr.resize_focused(*delta);
            Some(LoopAction::Redraw)
        }
        AppEvent::ValidateAndSaveApiKey {
            entry_id,
            provider_name,
            key,
        } => {
            validate_and_save_api_key(
                &mut pane_mgr.focused_mut().app,
                entry_id,
                provider_name,
                key,
            )
            .await;
            Some(LoopAction::Redraw)
        }
        _ => None,
    }
}

/// async resolution of a [`AppEvent::ValidateAndSaveApiKey`]: hit the
/// upstream `/v1/models` (or `/auth/key` for openrouter) to confirm
/// the key is live, persist it on success, and update the picker
/// state so the row repaints with the new source. on failure the key
/// is dropped and the picker's entry prompt is re-armed for retry
async fn validate_and_save_api_key(app: &mut App, entry_id: &str, provider_name: &str, key: &str) {
    let outcome = run_validation_and_save(entry_id, key).await;
    let Some(picker) = app.interaction.login_picker.as_mut() else {
        return;
    };
    match outcome {
        SaveOutcome::Saved(toast) => {
            picker.finish_validation_success(toast);
            picker.rebuild_sources();
        }
        SaveOutcome::Failed(reason) => {
            picker.finish_validation_failure(entry_id, provider_name, reason);
        }
    }
}

/// outcome of validate-then-save. carries the user-facing message so
/// the caller doesn't need a second branch on what to display
enum SaveOutcome {
    Saved(String),
    Failed(String),
}

async fn run_validation_and_save(entry_id: &str, key: &str) -> SaveOutcome {
    use mush_ai::login::{LoginMethod, ValidationError, get_provider, validate_api_key};

    let Some(provider) = get_provider(entry_id) else {
        return SaveOutcome::Failed(format!("provider {entry_id} no longer in catalogue"));
    };
    let LoginMethod::ApiKey { storage_key, .. } = provider.method.clone() else {
        return SaveOutcome::Failed("row is not an api-key provider".into());
    };

    let client = reqwest::Client::new();
    let info = match validate_api_key(&client, &provider, key).await {
        Ok(info) => Some(info),
        // no live-validation endpoint registered for this provider -
        // fall through to a save without the sanity check rather
        // than blocking the user
        Err(ValidationError::Unsupported(_)) => None,
        Err(err) => return SaveOutcome::Failed(err.to_string()),
    };

    let store = mush_ai::credentials::default_store();
    if let Err(save_err) = store.set(&storage_key, key) {
        let prefix = if info.is_some() {
            "validated, but saving failed"
        } else {
            "failed to save api key"
        };
        return SaveOutcome::Failed(format!("{prefix}: {save_err}"));
    }

    SaveOutcome::Saved(format_save_toast(entry_id, store.backend(), info))
}

/// build the user-facing success toast. openrouter populates label,
/// usage/limit, and is_free_tier on `/auth/key`. surface whichever of
/// those is meaningful; suppress the "$0 used" line that fresh keys
/// would otherwise produce
fn format_save_toast(
    entry_id: &str,
    backend: &str,
    info: Option<mush_ai::login::KeyInfo>,
) -> String {
    let Some(info) = info else {
        return format!("saved api key for {entry_id} (no live-validation endpoint)");
    };
    let mut toast = format!("✓ saved {entry_id} api key ({backend})");
    if let Some(label) = info.label {
        toast.push_str(&format!(" — '{label}'"));
    }
    if let Some(suffix) = budget_suffix(info.usage, info.limit, info.free_tier) {
        toast.push_str(&suffix);
    }
    toast
}

/// pick the most informative budget descriptor for a key. priority:
///
/// 1. explicit limit + usage  → `$U/$L used`
/// 2. nonzero usage           → `$U used`
/// 3. free-tier flag          → `free tier`
/// 4. nothing meaningful      → `None` so the caller skips the suffix
fn budget_suffix(
    usage: Option<f64>,
    limit: Option<f64>,
    free_tier: Option<bool>,
) -> Option<String> {
    if let (Some(used), Some(cap)) = (usage, limit)
        && cap > 0.0
    {
        return Some(format!(" — ${used:.2}/${cap:.2} used"));
    }
    if let Some(used) = usage
        && used > 0.0
    {
        return Some(format!(" — ${used:.2} used"));
    }
    if free_tier == Some(true) {
        return Some(" — free tier".into());
    }
    None
}

pub(super) async fn handle_streaming_terminal_events(
    pane_mgr: &mut PaneManager,
    stream_state: &mut StreamState,
    deps: &mut InputDeps<'_>,
) -> io::Result<LoopAction> {
    while event::poll(std::time::Duration::ZERO)? {
        let event = event::read()?;
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if pane_mgr.focused().app.interaction.mode == app::AppMode::ToolConfirm {
                    if let Some(allowed) = confirmation_answer(key.code) {
                        answer_confirmation(pane_mgr, stream_state, allowed).await;
                    }
                    continue;
                }

                let app_event = handle_key(&mut pane_mgr.focused_mut().app, key);
                let Some(app_event) = app_event else {
                    continue;
                };

                if let Some(action) =
                    handle_common_app_event(&app_event, pane_mgr, Some(stream_state), deps).await
                {
                    if matches!(action, LoopAction::Quit) {
                        return Ok(LoopAction::Quit);
                    }
                    continue;
                }

                match app_event {
                    AppEvent::Abort => {
                        abort_focused_stream(pane_mgr, stream_state, deps.delegation_queue).await;
                    }
                    AppEvent::UserSubmit { text } => {
                        submit_streaming_input(pane_mgr, stream_state, text).await;
                    }
                    AppEvent::EditSteering => {
                        edit_last_queued_steering(pane_mgr, stream_state).await;
                    }
                    _ => {}
                }
            }
            Event::Mouse(mouse) => handle_mouse(&mut pane_mgr.focused_mut().app, mouse),
            Event::Paste(text) => {
                pane_mgr.focused_mut().app.handle_paste(&text);
            }
            Event::Resize(w, h) => {
                // invalidate the size cache so ratatui's autoresize
                // picks up the new dimensions on the next frame, and
                // reset every pane's scroll baseline so post-resize
                // k/j presses don't get swallowed by stale baseline_vis
                deps.size_cache.invalidate();
                for pane in pane_mgr.panes_mut() {
                    pane.app.notify_resize();
                }
                tracing::debug!(
                    phase = "streaming",
                    width = w,
                    height = h,
                    "terminal resized"
                );
            }
            event => trace_dropped_event(event, "streaming"),
        }
    }

    Ok(LoopAction::Continue)
}

pub(super) async fn handle_idle_terminal_events(
    pane_mgr: &mut PaneManager,
    deps: &mut InputDeps<'_>,
) -> io::Result<LoopAction> {
    if !event::poll(std::time::Duration::from_millis(50))? {
        return Ok(LoopAction::Continue);
    }

    loop {
        let event = event::read()?;
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let app_event = handle_key(&mut pane_mgr.focused_mut().app, key);
                let Some(app_event) = app_event else {
                    if !event::poll(std::time::Duration::ZERO)? {
                        break;
                    }
                    continue;
                };

                if let Some(action) =
                    handle_common_app_event(&app_event, pane_mgr, None, deps).await
                {
                    if matches!(action, LoopAction::Quit) {
                        return Ok(LoopAction::Quit);
                    }
                } else {
                    match app_event {
                        AppEvent::UserSubmit { text } => {
                            // a backgrounded compaction is going to call
                            // `conversation.replace_messages(...)` on
                            // completion, which would clobber any user
                            // message appended while it's running. queue
                            // the text (and any pending images) so the
                            // runtime can replay them as a fresh turn
                            // once compaction lands; the input box
                            // clears so the user can keep typing
                            let pane_id = pane_mgr.focused().id;
                            if deps.pending_compactions.contains_key(&pane_id) {
                                let pane = pane_mgr.focused_mut();
                                let images: Vec<Vec<u8>> = pane
                                    .app
                                    .input
                                    .take_images()
                                    .into_iter()
                                    .map(|img| img.data)
                                    .collect();
                                pane.queue_input_during_compaction(text, images);
                                continue;
                            }
                            let expanded = slash::expand_template(&text);
                            let pane = pane_mgr.focused_mut();
                            if pane.label.is_none() && pane.conversation.is_empty() {
                                pane.label = Some(expanded.chars().take(30).collect());
                            }
                            let pane = pane_mgr.focused_mut();
                            let pending_images: Vec<Vec<u8>> = pane
                                .app
                                .input
                                .images
                                .iter()
                                .map(|img| img.data.clone())
                                .collect();
                            if pending_images.is_empty() {
                                pane.app.push_user_message(expanded.clone());
                            } else {
                                let msg_idx = pane.app.messages.len();
                                pane.app.push_user_message_with_images(
                                    expanded.clone(),
                                    pending_images.clone(),
                                );
                                if let Some(picker) = deps.image_picker {
                                    for (img_idx, data) in pending_images.iter().enumerate() {
                                        if let Ok(dyn_img) = image::load_from_memory(data) {
                                            let proto = picker.new_resize_protocol(dyn_img);
                                            pane.image_protos.insert((msg_idx, img_idx), proto);
                                        }
                                    }
                                }
                            }
                            let app = &mut pane_mgr.focused_mut().app;
                            app.active_tools.clear();
                            app.start_streaming();
                            *deps.pending_prompt = Some(expanded);
                            // save the user message immediately so it survives crashes
                            if let Some(ref saver) = deps.tui_config.save_session {
                                saver(super::streams::build_session_snapshot(
                                    pane_mgr,
                                    deps.tui_config,
                                ));
                            }
                        }
                        AppEvent::SlashCommand { action } => {
                            let state_changed = handle_slash_action(
                                pane_mgr,
                                action,
                                SlashEnv {
                                    tui_config: deps.tui_config,
                                    thinking_prefs: deps.thinking_prefs,
                                    registry: deps.registry,
                                    message_bus: deps.message_bus,
                                    file_tracker: deps.file_tracker,
                                    lifecycle_hooks: deps.lifecycle_hooks,
                                    cwd: deps.cwd,
                                    pending_prompt: deps.pending_prompt,
                                    pending_compactions: deps.pending_compactions,
                                },
                            )
                            .await;

                            if state_changed && let Some(ref saver) = deps.tui_config.save_session {
                                saver(super::streams::build_session_snapshot(
                                    pane_mgr,
                                    deps.tui_config,
                                ));
                            }
                        }
                        AppEvent::SettingsToggleSelected => {
                            apply_settings_toggle(pane_mgr, deps.tui_config);
                        }
                        AppEvent::ModelSelected { model_id } => {
                            // route through the same slash action so the
                            // switch-model logic stays single-sourced
                            let state_changed = handle_slash_action(
                                pane_mgr,
                                crate::slash::SlashAction::Model {
                                    model_id: Some(model_id),
                                    show_all: false,
                                },
                                SlashEnv {
                                    tui_config: deps.tui_config,
                                    thinking_prefs: deps.thinking_prefs,
                                    registry: deps.registry,
                                    message_bus: deps.message_bus,
                                    file_tracker: deps.file_tracker,
                                    lifecycle_hooks: deps.lifecycle_hooks,
                                    cwd: deps.cwd,
                                    pending_prompt: deps.pending_prompt,
                                    pending_compactions: deps.pending_compactions,
                                },
                            )
                            .await;
                            if state_changed && let Some(ref saver) = deps.tui_config.save_session {
                                saver(super::streams::build_session_snapshot(
                                    pane_mgr,
                                    deps.tui_config,
                                ));
                            }
                        }
                        AppEvent::PersistFavourites => {
                            if let Some(ref saver) = deps.tui_config.save_favourite_models {
                                saver(&pane_mgr.focused().app.completion.favourite_models);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Mouse(mouse) => handle_mouse(&mut pane_mgr.focused_mut().app, mouse),
            Event::Paste(text) => {
                pane_mgr.focused_mut().app.handle_paste(&text);
            }
            Event::Resize(w, h) => {
                deps.size_cache.invalidate();
                for pane in pane_mgr.panes_mut() {
                    pane.app.notify_resize();
                }
                tracing::debug!(phase = "idle", width = w, height = h, "terminal resized");
            }
            event => trace_dropped_event(event, "idle"),
        }

        if !event::poll(std::time::Duration::ZERO)? {
            break;
        }
    }

    Ok(LoopAction::Redraw)
}

/// apply a toggle to the selected /settings menu item and sync the result
/// into the live StreamOptions so the next turn picks up the change
fn apply_settings_toggle(
    pane_mgr: &mut crate::pane::PaneManager,
    tui_config: &mut crate::runner::TuiConfig,
) {
    let app = &mut pane_mgr.focused_mut().app;
    let Some(menu) = app.settings_menu.as_mut() else {
        return;
    };
    let item = menu.current();
    let new_value = item.activate(&mut tui_config.settings);
    // mirror beta changes into the in-flight stream options so they take
    // effect on the next LLM call
    tui_config.options.anthropic_betas = Some(tui_config.settings.anthropic_betas.clone());

    // persist if scope requires it
    let persist_note = match crate::settings::persist(&tui_config.settings, &tui_config.cwd) {
        Ok(Some(path)) => format!(" (saved to {})", path.display()),
        Ok(None) => String::new(),
        Err(e) => format!(" (persist failed: {e})"),
    };
    app.status = Some(format!("{} → {}{}", item.label(), new_value, persist_note));
}

async fn paste_clipboard_image(app: &mut App) {
    app.status = Some("reading clipboard…".into());
    match tokio::task::spawn_blocking(crate::clipboard::read_clipboard_image).await {
        Ok(Some(image)) => {
            let mime = image.mime_type.as_str();
            let size = image.bytes.len();
            app.input.add_image(image);
            let n = app.input.images.len();
            let kb = size / 1024;
            app.status = Some(format!("{n} image(s) attached ({mime}, {kb}kb)"));
        }
        Ok(None) => {
            app.status = Some("no image in clipboard".into());
        }
        Err(_) => {
            app.status = Some("failed to read clipboard".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_answer_accepts_yes_keys() {
        assert_eq!(confirmation_answer(KeyCode::Char('y')), Some(true));
        assert_eq!(confirmation_answer(KeyCode::Char('Y')), Some(true));
        assert_eq!(confirmation_answer(KeyCode::Enter), Some(true));
    }

    #[test]
    fn confirmation_answer_accepts_no_keys() {
        assert_eq!(confirmation_answer(KeyCode::Char('n')), Some(false));
        assert_eq!(confirmation_answer(KeyCode::Char('N')), Some(false));
        assert_eq!(confirmation_answer(KeyCode::Esc), Some(false));
    }

    #[test]
    fn confirmation_answer_ignores_other_keys() {
        assert_eq!(confirmation_answer(KeyCode::Char('x')), None);
        assert_eq!(confirmation_answer(KeyCode::Tab), None);
    }

    fn key_info(
        label: Option<&str>,
        usage: Option<f64>,
        limit: Option<f64>,
        free_tier: Option<bool>,
    ) -> mush_ai::login::KeyInfo {
        mush_ai::login::KeyInfo {
            label: label.map(str::to_string),
            usage,
            limit,
            free_tier,
        }
    }

    #[test]
    fn format_save_toast_omits_zero_usage_when_no_limit_is_known() {
        // a fresh openrouter key returns usage=0 and limit=null. that
        // doesn't tell the user anything useful, so the toast should
        // skip the "$0 used" suffix instead of polluting the message
        let toast = format_save_toast(
            "openrouter",
            "keyring",
            Some(key_info(Some("k"), Some(0.0), None, None)),
        );
        assert!(!toast.contains('$'), "got: {toast}");
    }

    #[test]
    fn format_save_toast_shows_budget_when_limit_is_set() {
        let toast = format_save_toast(
            "openrouter",
            "keyring",
            Some(key_info(None, Some(1.23), Some(10.0), None)),
        );
        assert!(toast.contains("$1.23/$10.00"), "got: {toast}");
    }

    #[test]
    fn format_save_toast_marks_free_tier_keys() {
        // openrouter exposes is_free_tier; surface it instead of (or
        // alongside) usage so users on free plans get a useful badge
        let toast = format_save_toast(
            "openrouter",
            "keyring",
            Some(key_info(None, Some(0.0), None, Some(true))),
        );
        assert!(
            toast.to_lowercase().contains("free tier"),
            "expected free tier marker, got: {toast}"
        );
    }

    #[test]
    fn format_save_toast_shows_used_amount_when_no_limit_but_nonzero_usage() {
        let toast = format_save_toast(
            "openrouter",
            "keyring",
            Some(key_info(None, Some(4.50), None, None)),
        );
        assert!(toast.contains("$4.50"), "got: {toast}");
    }
}
