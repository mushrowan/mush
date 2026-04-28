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
        _ => None,
    }
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
                pane_mgr.focused_mut().app.input.insert_str(&text);
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
                pane_mgr.focused_mut().app.input.insert_str(&text);
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
}
