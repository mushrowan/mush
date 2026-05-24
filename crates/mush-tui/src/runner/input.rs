use std::io;
use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
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

impl std::fmt::Debug for LoopAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Continue => f.write_str("Continue"),
            Self::Redraw => f.write_str("Redraw"),
            Self::Quit => f.write_str("Quit"),
        }
    }
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

/// shared resize handler: invalidate the cached size so ratatui's
/// `autoresize` refetches dimensions on the next frame, and reset
/// every pane's scroll baseline so post-resize k/j presses don't get
/// swallowed by stale `baseline_vis` values. used by every code path
/// that observes a `crossterm::event::Event::Resize`
pub(super) fn apply_resize(
    size_cache: &std::rc::Rc<super::caching_backend::CachedSizeState>,
    pane_mgr: &mut PaneManager,
) {
    size_cache.invalidate();
    for pane in pane_mgr.panes_mut() {
        pane.app.notify_resize();
    }
}

pub(super) async fn handle_streaming_terminal_events(
    pane_mgr: &mut PaneManager,
    stream_state: &mut StreamState,
    deps: &mut InputDeps<'_>,
) -> io::Result<LoopAction> {
    let mut changed = false;
    while event::poll(std::time::Duration::ZERO)? {
        let event = event::read()?;
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match handle_streaming_key_event(pane_mgr, stream_state, deps, key).await? {
                    LoopAction::Quit => return Ok(LoopAction::Quit),
                    LoopAction::Redraw => changed = true,
                    LoopAction::Continue => {}
                }
            }
            Event::Mouse(mouse) => {
                handle_mouse(&mut pane_mgr.focused_mut().app, mouse);
                changed = true;
            }
            Event::Paste(text) => {
                pane_mgr.focused_mut().app.handle_paste(&text);
                changed = true;
            }
            Event::Resize(w, h) => {
                // invalidate the size cache so ratatui's autoresize
                // picks up the new dimensions on the next frame, and
                // reset every pane's scroll baseline so post-resize
                // k/j presses don't get swallowed by stale baseline_vis
                apply_resize(deps.size_cache, pane_mgr);
                changed = true;
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

    Ok(if changed {
        LoopAction::Redraw
    } else {
        LoopAction::Continue
    })
}

async fn handle_streaming_key_event(
    pane_mgr: &mut PaneManager,
    stream_state: &mut StreamState,
    deps: &mut InputDeps<'_>,
    key: KeyEvent,
) -> io::Result<LoopAction> {
    if pane_mgr.focused().app.interaction.mode == app::AppMode::ToolConfirm {
        if let Some(allowed) = confirmation_answer(key.code) {
            answer_confirmation(pane_mgr, stream_state, allowed).await;
            return Ok(LoopAction::Redraw);
        }
        return Ok(LoopAction::Continue);
    }

    let app_event = handle_key(&mut pane_mgr.focused_mut().app, key);
    let Some(app_event) = app_event else {
        return Ok(LoopAction::Redraw);
    };

    if let Some(action) =
        handle_common_app_event(&app_event, pane_mgr, Some(stream_state), deps).await
    {
        return Ok(match action {
            LoopAction::Continue => LoopAction::Redraw,
            action => action,
        });
    }

    match app_event {
        AppEvent::Abort => {
            abort_focused_stream(pane_mgr, stream_state, deps.delegation_queue).await;
        }
        AppEvent::UserSubmit { text, images } => {
            submit_streaming_input(pane_mgr, stream_state, text, images).await;
        }
        AppEvent::EditSteering => {
            edit_last_queued_steering(pane_mgr, stream_state).await;
        }
        _ => {}
    }

    Ok(LoopAction::Redraw)
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
                        AppEvent::UserSubmit { text, images } => {
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
                                pane.queue_input_during_compaction(text, images);
                                continue;
                            }
                            let expanded = slash::expand_template(&text);
                            let pane = pane_mgr.focused_mut();
                            if pane.label.is_none() && pane.conversation.is_empty() {
                                pane.label = Some(expanded.chars().take(30).collect());
                            }
                            let pane = pane_mgr.focused_mut();
                            let pending_images: Vec<Vec<u8>> =
                                images.iter().map(|img| img.data.clone()).collect();
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
                            let pane = pane_mgr.focused_mut();
                            pane.app.active_tools.clear();
                            pane.app.start_streaming();
                            pane.pending_prompt =
                                Some(crate::pane::PendingPrompt::with_images(expanded, images));
                            super::runtime::save_pane_pending_prompt(pane);
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
                apply_resize(deps.size_cache, pane_mgr);
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
    use crate::pane::{Pane, PaneId, PaneManager};
    use crate::runner::caching_backend::CachedSizeState;
    use crossterm::event::{KeyEvent, KeyEventState, KeyModifiers};
    use mush_ai::types::{StreamOptions, TokenCount};
    use std::rc::Rc;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn pane_with_baselines(id: u32, baseline: usize, baseline_vis: u16) -> Pane {
        let pane = Pane::new(
            PaneId::new(id),
            App::new("test".into(), TokenCount::new(200_000)),
        );
        pane.app.render_state.scroll_baseline.set(baseline);
        pane.app.render_state.scroll_baseline_vis.set(baseline_vis);
        pane
    }

    #[test]
    fn apply_resize_resets_scroll_baselines_across_all_panes() {
        // a queued resize event should reset *every* pane's scroll
        // baseline, not just the focused one. otherwise post-resize k/j
        // presses get swallowed by stale baseline_vis on background
        // panes once they regain focus
        let mut mgr = PaneManager::new(pane_with_baselines(1, 42, 7));
        mgr.add_pane(pane_with_baselines(2, 99, 11));
        mgr.add_pane(pane_with_baselines(3, 13, 3));

        let size_cache = Rc::new(CachedSizeState::default());
        apply_resize(&size_cache, &mut mgr);

        for pane in mgr.panes_mut() {
            assert_eq!(
                pane.app.render_state.scroll_baseline.get(),
                0,
                "scroll_baseline should reset on every pane"
            );
            assert_eq!(
                pane.app.render_state.scroll_baseline_vis.get(),
                0,
                "scroll_baseline_vis should reset on every pane"
            );
        }
    }

    #[test]
    fn apply_resize_invalidates_size_cache() {
        // mid-streaming resize must mark the size cache stale so the
        // next backend.size() refetches. observed by checking
        // `has_cached_size()` flips false after the call
        let mut mgr = PaneManager::new(pane_with_baselines(1, 0, 0));
        let size_cache = Rc::new(CachedSizeState::default());
        size_cache.set_size_for_test(ratatui::layout::Size::new(80, 25));
        assert!(size_cache.has_cached_size());

        apply_resize(&size_cache, &mut mgr);

        assert!(
            !size_cache.has_cached_size(),
            "apply_resize should clear the cached size"
        );
    }

    #[tokio::test]
    async fn streaming_key_input_requests_redraw() {
        let pane = Pane::new(
            PaneId::new(1),
            App::new("test".into(), TokenCount::new(200_000)),
        );
        let mut pane_mgr = PaneManager::new(pane);
        pane_mgr.focused_mut().app.start_tool(
            &mush_ai::types::ToolCallId::from("toolu_test"),
            "bash_status",
            "job 1",
        );
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut stream_state = StreamState::new();
        stream_state.register_active(
            pane_mgr.focused().id,
            super::super::streams::StreamMeta {
                cancel: tokio_util::sync::CancellationToken::new(),
                steering_queue: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
                confirm_req_rx: rx,
                confirm_reply: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                context_tokens: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                model: mush_ai::models::all_models_with_user()
                    .into_iter()
                    .next()
                    .expect("expected at least one model"),
            },
        );
        let mut tui_config = crate::runner::TuiConfig {
            model: mush_ai::models::all_models_with_user()
                .into_iter()
                .next()
                .expect("expected at least one model"),
            system_prompt: None,
            options: StreamOptions::default(),
            max_turns: 32,
            initial_messages: Vec::new(),
            initial_panes: Vec::new(),
            theme: crate::theme::Theme::default(),
            config_path: None,
            provider_api_keys: std::collections::HashMap::new(),
            thinking_prefs: ThinkingPrefs::default(),
            save_thinking_prefs: None,
            last_models: crate::runner::LastModels::default(),
            save_last_models: None,
            save_session: None,
            update_title: None,
            confirm_tools: false,
            auto_compact: false,
            auto_fork_compact: false,
            show_cost: false,
            show_usage_lines: false,
            show_token_counters: false,
            status_bar_config: Default::default(),
            debug_cache: false,
            cache_timer: false,
            thinking_display: crate::app::ThinkingDisplay::Collapse,
            tool_output_live: None,
            log_buffer: None,
            isolation_mode: crate::file_tracker::IsolationMode::None,
            terminal_policy: crate::terminal_policy::TerminalPolicy::default(),
            lifecycle_hooks: mush_agent::LifecycleHooks::default(),
            cwd: std::path::PathBuf::new(),
            dynamic_system_context: None,
            file_rules: None,
            lsp_diagnostics: None,
            agent_card: None,
            model_tiers: std::collections::HashMap::new(),
            compaction_model: None,
            resolve_model: None,
            http_client: None,
            session_id: mush_ai::types::SessionId::new(),
            settings: Default::default(),
            scroll_lines: crate::app::DEFAULT_SCROLL_LINES,
            favourite_models: Vec::new(),
            favourites_locked: false,
            save_favourite_models: None,
            keymap: crate::keybinds::KeyMap::default(),
            reload_context: None,
            forbidden_chars: Default::default(),
        };
        let registry = mush_ai::registry::ApiRegistry::new();
        let message_bus = crate::messaging::MessageBus::new();
        let file_tracker = crate::file_tracker::FileTracker::new(std::path::PathBuf::new());
        let lifecycle_hooks = mush_agent::LifecycleHooks::default();
        let mut pending_prompt = None;
        let mut pending_compactions = std::collections::HashMap::new();
        let delegation_queue = crate::delegate::new_queue();
        let image_picker = None;
        let size_cache = Rc::new(CachedSizeState::default());
        let mut deps = InputDeps {
            tui_config: &mut tui_config,
            thinking_prefs: &mut ThinkingPrefs::default(),
            thinking_saver: &None,
            registry: &registry,
            message_bus: &message_bus,
            file_tracker: &file_tracker,
            lifecycle_hooks: &lifecycle_hooks,
            cwd: std::path::Path::new(""),
            pending_prompt: &mut pending_prompt,
            pending_compactions: &mut pending_compactions,
            delegation_queue: &delegation_queue,
            image_picker: &image_picker,
            size_cache: &size_cache,
        };

        let action = handle_streaming_key_event(
            &mut pane_mgr,
            &mut stream_state,
            &mut deps,
            key(KeyCode::Char('h')),
        )
        .await
        .unwrap();

        assert!(matches!(action, LoopAction::Redraw));
        assert_eq!(pane_mgr.focused().app.input.text, "h");
    }

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
