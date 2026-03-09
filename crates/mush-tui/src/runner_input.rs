use std::collections::HashMap;
use std::io;
use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use mush_ai::registry::ApiRegistry;
use mush_ai::types::ThinkingLevel;

use crate::app::{self, App, AppEvent};
use crate::input::handle_key;
use crate::pane::PaneManager;
use crate::runner::{ThinkingPrefsSaver, TuiConfig};
use crate::runner_commands::{SlashEnv, handle_slash_action, save_thinking_pref};
use crate::runner_panes::{close_focused_pane, fork_pane};
use crate::runner_render::handle_mouse;
use crate::runner_streams::{
    StreamState, abort_focused_stream, answer_confirmation, edit_last_queued_steering,
    submit_streaming_input,
};
use crate::slash;

pub(super) enum LoopAction {
    Continue,
    Quit,
}

pub(super) struct InputDeps<'a> {
    pub tui_config: &'a mut TuiConfig,
    pub thinking_prefs: &'a mut HashMap<String, ThinkingLevel>,
    pub thinking_saver: &'a Option<ThinkingPrefsSaver>,
    pub registry: &'a ApiRegistry,
    pub message_bus: &'a crate::messaging::MessageBus,
    pub file_tracker: &'a crate::file_tracker::FileTracker,
    pub cwd: &'a Path,
    pub pending_prompt: &'a mut Option<String>,
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
            tracing::trace!(
                %phase,
                code = ?key.code,
                kind = ?key.kind,
                "dropped non-press key event"
            );
        }
        event => {
            tracing::trace!(%phase, ?event, "dropped non-key event");
        }
    }
}

async fn handle_common_app_event(
    app_event: &AppEvent,
    pane_mgr: &mut PaneManager,
    deps: &mut InputDeps<'_>,
) -> Option<LoopAction> {
    match app_event {
        AppEvent::Quit => Some(LoopAction::Quit),
        AppEvent::CycleThinkingLevel => {
            let app = &pane_mgr.focused().app;
            save_thinking_pref(
                deps.thinking_prefs,
                deps.thinking_saver,
                &app.model_id,
                app.thinking_level,
            );
            Some(LoopAction::Continue)
        }
        AppEvent::PasteImage => {
            paste_clipboard_image(&mut pane_mgr.focused_mut().app).await;
            Some(LoopAction::Continue)
        }
        AppEvent::SplitPane => {
            fork_pane(
                pane_mgr,
                deps.tui_config,
                deps.message_bus,
                &deps.tui_config.tool_output_live,
            )
            .await;
            Some(LoopAction::Continue)
        }
        AppEvent::ClosePane => {
            close_focused_pane(pane_mgr, deps.message_bus, deps.file_tracker, deps.cwd).await;
            Some(LoopAction::Continue)
        }
        AppEvent::FocusNextPane => {
            pane_mgr.focus_next();
            Some(LoopAction::Continue)
        }
        AppEvent::FocusPrevPane => {
            pane_mgr.focus_prev();
            Some(LoopAction::Continue)
        }
        AppEvent::FocusPaneByIndex(index) => {
            pane_mgr.focus_index(*index);
            Some(LoopAction::Continue)
        }
        AppEvent::ResizePane(delta) => {
            pane_mgr.resize_focused(*delta);
            Some(LoopAction::Continue)
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
                if pane_mgr.focused().app.mode == app::AppMode::ToolConfirm {
                    if let Some(allowed) = confirmation_answer(key.code) {
                        answer_confirmation(pane_mgr, stream_state, allowed).await;
                    }
                    continue;
                }

                let app_event = handle_key(&mut pane_mgr.focused_mut().app, key);
                let Some(app_event) = app_event else {
                    continue;
                };

                if let Some(action) = handle_common_app_event(&app_event, pane_mgr, deps).await {
                    if matches!(action, LoopAction::Quit) {
                        return Ok(LoopAction::Quit);
                    }
                    continue;
                }

                match app_event {
                    AppEvent::Abort => {
                        abort_focused_stream(pane_mgr, stream_state).await;
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

                if let Some(action) = handle_common_app_event(&app_event, pane_mgr, deps).await {
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
                            let app = &mut pane_mgr.focused_mut().app;
                            app.push_user_message(expanded.clone());
                            app.active_tools.clear();
                            app.start_streaming();
                            *deps.pending_prompt = Some(expanded);
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
                                    cwd: deps.cwd,
                                    pending_prompt: deps.pending_prompt,
                                },
                            )
                            .await;

                            if state_changed && let Some(ref saver) = deps.tui_config.save_session {
                                let pane = pane_mgr.focused();
                                saver(&pane.conversation, &pane.app.model_id);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Mouse(mouse) => handle_mouse(&mut pane_mgr.focused_mut().app, mouse),
            event => trace_dropped_event(event, "idle"),
        }

        if !event::poll(std::time::Duration::ZERO)? {
            break;
        }
    }

    Ok(LoopAction::Continue)
}

async fn paste_clipboard_image(app: &mut App) {
    app.status = Some("reading clipboard...".into());
    match tokio::task::spawn_blocking(crate::clipboard::read_clipboard_image).await {
        Ok(Some(image)) => {
            let mime = image.mime_type.as_str();
            let size = image.bytes.len();
            app.add_image(image);
            let n = app.pending_images.len();
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
