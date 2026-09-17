use std::io;
use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use mush_ai::registry::ApiRegistry;

use crate::app::{self, App, AppEvent};
use crate::input::handle_key;
use crate::session::{AgentConnection, Session};

use super::commands::{SlashEnv, handle_slash_action, save_thinking_pref};
use super::streams::{
    StreamState, abort_focused_stream, answer_confirmation, edit_last_queued_steering,
    submit_streaming_input,
};
use super::{ThinkingPrefs, ThinkingPrefsSaver, TuiConfig};
use crate::slash;

pub(super) enum LoopAction {
    Continue,
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
    pub cwd: &'a Path,
    pub pending_prompt: &'a mut Option<String>,
    pub image_picker: &'a Option<ratatui_image::picker::Picker>,
    pub size_cache: &'a std::rc::Rc<super::size_cached_backend::CachedSizeState>,
}

fn confirmation_answer(code: KeyCode) -> Option<bool> {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(true),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
        _ => None,
    }
}

fn kernel_busy_answer(code: KeyCode) -> Option<mush_tools::ipython::BusyKernelChoice> {
    use mush_tools::ipython::BusyKernelChoice;
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(BusyKernelChoice::Wait),
        KeyCode::Char('n') | KeyCode::Char('N') => Some(BusyKernelChoice::Kill),
        KeyCode::Esc => Some(BusyKernelChoice::Cancel),
        _ => None,
    }
}

fn trace_dropped_event(event: Event, phase: &str) {
    match event {
        Event::Key(key) => {
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
            tracing::warn!(
                target: "mush_tui::resize",
                %phase,
                width = w,
                height = h,
                "resize event reached unhandled-event path (no apply_resize)"
            );
        }
        event => {
            tracing::debug!(%phase, ?event, "unhandled terminal event");
        }
    }
}

async fn handle_common_app_event(
    app_event: &AppEvent,
    session: &mut Session,
    _stream_state: Option<&mut StreamState>,
    deps: &mut InputDeps<'_>,
) -> Option<LoopAction> {
    match app_event {
        AppEvent::Quit => Some(LoopAction::Quit),
        AppEvent::CycleThinkingLevel => {
            let app = &session.app;
            save_thinking_pref(
                deps.thinking_prefs,
                deps.thinking_saver,
                deps.cwd,
                &app.model_id,
                app.thinking_level.clone(),
            );
            Some(LoopAction::Redraw)
        }
        AppEvent::PasteImage => {
            paste_clipboard_image(&mut session.app).await;
            Some(LoopAction::Redraw)
        }
        AppEvent::Copy { text } => {
            let env = crate::clipboard::Environment::detect();
            copy_to_clipboard(&mut session.app, text, &env, &mut std::io::stdout());
            Some(LoopAction::Redraw)
        }
        AppEvent::ValidateAndSaveApiKey {
            entry_id,
            provider_name,
            key,
        } => {
            validate_and_save_api_key(&mut session.app, entry_id, provider_name, key).await;
            Some(LoopAction::Redraw)
        }
        AppEvent::CompactionRecoverySelected { action } => {
            handle_compaction_recovery_action(session, deps, *action).await;
            Some(LoopAction::Redraw)
        }
        _ => None,
    }
}

async fn handle_compaction_recovery_action(
    session: &mut Session,
    deps: &mut InputDeps<'_>,
    action: crate::app_state::CompactionRecoveryAction,
) {
    let Some(recovery) = session.app.interaction.compaction_recovery.take() else {
        session.app.interaction.mode = app::AppMode::Insert;
        return;
    };
    session.app.interaction.mode = app::AppMode::Insert;

    let command = match action {
        crate::app_state::CompactionRecoveryAction::Retry => {
            session.app.status = Some("retrying compaction\u{2026}".into());
            mush_runtime::ClientCommand::CompactionRetry
        }
        crate::app_state::CompactionRecoveryAction::Fallback => {
            mush_runtime::ClientCommand::CompactionFallback
        }
        crate::app_state::CompactionRecoveryAction::Cancel => {
            session.app.compacting = false;
            session.app.status = Some("compaction cancelled".into());
            super::runtime::replay_compaction_inputs(session).await;
            return;
        }
    };
    drop(recovery);
    match session.connection.send(command).await {
        Ok(events) => super::looping::apply_command_events(session, deps.tui_config, events).await,
        Err(error) => session.app.push_system_message(error.to_string()),
    }
}

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

    let client = mush_ai::http::client();
    let info = match validate_api_key(&client, &provider, key).await {
        Ok(info) => Some(info),
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

pub(super) fn apply_resize(size_cache: &std::rc::Rc<super::size_cached_backend::CachedSizeState>) {
    size_cache.invalidate();
    tracing::info!(target: "mush_tui::resize", "size cache invalidated");
}

pub(super) async fn handle_streaming_terminal_events(
    session: &mut Session,
    stream_state: &mut StreamState,
    deps: &mut InputDeps<'_>,
) -> io::Result<LoopAction> {
    let mut changed = false;
    while event::poll(std::time::Duration::ZERO)? {
        let event = event::read()?;
        cancel_signal_for_event(stream_state, &event);
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match handle_streaming_key_event(session, stream_state, deps, key).await? {
                    LoopAction::Quit => return Ok(LoopAction::Quit),
                    LoopAction::Redraw => changed = true,
                    LoopAction::Continue => {}
                }
            }
            Event::Mouse(mouse) => {
                crate::input::handle_mouse(&mut session.app, mouse);
                changed = true;
            }
            Event::Paste(text) => {
                session.app.handle_paste(&text);
                changed = true;
            }
            Event::Resize(w, h) => {
                apply_resize(deps.size_cache);
                changed = true;
                tracing::info!(
                    target: "mush_tui::resize",
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
    session: &mut Session,
    stream_state: &mut StreamState,
    deps: &mut InputDeps<'_>,
    key: KeyEvent,
) -> io::Result<LoopAction> {
    stream_state.cancel_signal();
    if session.app.subagents.handle_key(&session.app.keymap, key) {
        return Ok(LoopAction::Redraw);
    }
    if session.app.interaction.mode == app::AppMode::ToolConfirm {
        if session.app.interaction.kernel_busy_reply.is_some() {
            if let Some(choice) = kernel_busy_answer(key.code) {
                super::streams::answer_kernel_busy(session, choice);
                return Ok(LoopAction::Redraw);
            }
            return Ok(LoopAction::Continue);
        }
        if let Some(allowed) = confirmation_answer(key.code) {
            answer_confirmation(session, stream_state, allowed).await;
            return Ok(LoopAction::Redraw);
        }
        return Ok(LoopAction::Continue);
    }

    let app_event = handle_key(&mut session.app, key);
    let Some(app_event) = app_event else {
        return Ok(LoopAction::Redraw);
    };

    if let Some(action) =
        handle_common_app_event(&app_event, session, Some(stream_state), deps).await
    {
        return Ok(match action {
            LoopAction::Continue => LoopAction::Redraw,
            action => action,
        });
    }

    match app_event {
        AppEvent::Abort => {
            abort_focused_stream(session, stream_state).await;
        }
        AppEvent::UserSubmit { text, images } => {
            submit_streaming_input(session, stream_state, text, images).await;
            record_model_use(session, deps.tui_config);
        }
        AppEvent::EditSteering => {
            edit_last_queued_steering(session, stream_state).await;
        }
        AppEvent::SlashCommand { action } => {
            let state_changed = handle_slash_action(
                session,
                action,
                SlashEnv {
                    tui_config: deps.tui_config,
                    thinking_prefs: deps.thinking_prefs,
                    registry: deps.registry,
                    cwd: deps.cwd,
                    pending_prompt: deps.pending_prompt,
                },
            )
            .await;
            if state_changed && let Some(ref saver) = deps.tui_config.save_session {
                saver(super::streams::build_session_snapshot(
                    session,
                    deps.tui_config,
                ));
            }
        }
        _ => {}
    }

    Ok(LoopAction::Redraw)
}

fn record_model_use(session: &mut Session, tui_config: &mut TuiConfig) {
    let model_id = session.app.model_id.clone();
    let updated = crate::model_picker::promote_recent_model(&tui_config.recent_models, &model_id);
    if updated == tui_config.recent_models {
        return;
    }
    tui_config.recent_models = updated.clone();
    session.app.completion.recent_models = updated;
    if let Some(ref saver) = tui_config.save_recent_models {
        saver(&tui_config.recent_models);
    }
}

pub(super) async fn handle_idle_terminal_events(
    session: &mut Session,
    stream_state: &mut StreamState,
    deps: &mut InputDeps<'_>,
) -> io::Result<LoopAction> {
    if !event::poll(std::time::Duration::from_millis(50))? {
        return Ok(LoopAction::Continue);
    }

    loop {
        let event = event::read()?;
        cancel_signal_for_event(stream_state, &event);
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let app_event = handle_key(&mut session.app, key);
                let Some(app_event) = app_event else {
                    if !event::poll(std::time::Duration::ZERO)? {
                        break;
                    }
                    continue;
                };

                if let Some(action) = handle_common_app_event(&app_event, session, None, deps).await
                {
                    if matches!(action, LoopAction::Quit) {
                        return Ok(LoopAction::Quit);
                    }
                } else {
                    match app_event {
                        AppEvent::UserSubmit { text, images } => {
                            if session.connection.state().compacting {
                                let pane = &mut *session;
                                pane.queue_input_during_compaction(text, images);
                                continue;
                            }
                            let expanded = slash::expand_template(&text);
                            let pane = &mut *session;
                            if pane.label.is_none() && pane.connection.state().context_len == 0 {
                                pane.label = Some(expanded.chars().take(30).collect());
                            }
                            let pane = &mut *session;
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
                            let pane = &mut *session;
                            pane.app.active_tools.clear();
                            pane.app.start_streaming();
                            match pane
                                .connection
                                .send(mush_runtime::ClientCommand::Prompt {
                                    text: expanded,
                                    images,
                                })
                                .await
                            {
                                Ok(events) => {
                                    super::looping::apply_command_events(
                                        pane,
                                        deps.tui_config,
                                        events,
                                    )
                                    .await
                                }
                                Err(error) => pane.app.push_system_message(error.to_string()),
                            }
                        }
                        AppEvent::SlashCommand { action } => {
                            let state_changed = handle_slash_action(
                                session,
                                action,
                                SlashEnv {
                                    tui_config: deps.tui_config,
                                    thinking_prefs: deps.thinking_prefs,
                                    registry: deps.registry,
                                    cwd: deps.cwd,
                                    pending_prompt: deps.pending_prompt,
                                },
                            )
                            .await;

                            if state_changed && let Some(ref saver) = deps.tui_config.save_session {
                                saver(super::streams::build_session_snapshot(
                                    session,
                                    deps.tui_config,
                                ));
                            }
                        }
                        AppEvent::SettingsToggleSelected => {
                            apply_settings_toggle(session, deps.tui_config);
                        }
                        AppEvent::ModelSelected { model_id } => {
                            let state_changed = handle_slash_action(
                                session,
                                crate::slash::SlashAction::Model {
                                    model_id: Some(model_id),
                                    show_all: false,
                                },
                                SlashEnv {
                                    tui_config: deps.tui_config,
                                    thinking_prefs: deps.thinking_prefs,
                                    registry: deps.registry,
                                    cwd: deps.cwd,
                                    pending_prompt: deps.pending_prompt,
                                },
                            )
                            .await;
                            if state_changed && let Some(ref saver) = deps.tui_config.save_session {
                                saver(super::streams::build_session_snapshot(
                                    session,
                                    deps.tui_config,
                                ));
                            }
                        }
                        AppEvent::PersistFavourites => {
                            if let Some(ref saver) = deps.tui_config.save_favourite_models {
                                saver(&session.app.completion.favourite_models);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Mouse(mouse) => {
                crate::input::handle_mouse(&mut session.app, mouse);
            }
            Event::Paste(text) => {
                session.app.handle_paste(&text);
            }
            Event::Resize(w, h) => {
                apply_resize(deps.size_cache);
                tracing::info!(
                    target: "mush_tui::resize",
                    phase = "idle",
                    width = w,
                    height = h,
                    "terminal resized"
                );
            }
            event => trace_dropped_event(event, "idle"),
        }

        if !event::poll(std::time::Duration::ZERO)? {
            break;
        }
    }

    Ok(LoopAction::Redraw)
}

fn cancel_signal_for_event(stream_state: &mut StreamState, event: &Event) {
    if matches!(
        event,
        Event::Key(_) | Event::Paste(_) | Event::Mouse(_) | Event::FocusGained
    ) {
        stream_state.cancel_signal();
    }
}

fn apply_settings_toggle(
    session: &mut crate::session::Session,
    tui_config: &mut crate::runner::TuiConfig,
) {
    let app = &mut session.app;
    let Some(menu) = app.settings_menu.as_mut() else {
        return;
    };
    let item = menu.current();
    let new_value = item.activate(&mut tui_config.settings);
    tui_config.options.anthropic_betas = Some(tui_config.settings.anthropic_betas.clone());

    let persist_note = match crate::settings::persist(&tui_config.settings, &tui_config.cwd) {
        Ok(Some(path)) => format!(" (saved to {})", path.display()),
        Ok(None) => String::new(),
        Err(e) => format!(" (persist failed: {e})"),
    };
    app.status = Some(format!("{} → {}{}", item.label(), new_value, persist_note).into());
}

fn copy_to_clipboard(
    app: &mut App,
    text: &str,
    env: &crate::clipboard::Environment,
    out: &mut impl std::io::Write,
) {
    if let Err(error) = crate::clipboard::copy(text, env, out) {
        app.status = Some(crate::app_state::Status::error(error.to_string()));
    }
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
            app.status = Some(format!("{n} image(s) attached ({mime}, {kb}kb)").into());
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
    use crate::runner::size_cached_backend::CachedSizeState;
    use crate::session::Session;
    use crossterm::event::{KeyEvent, KeyEventState, KeyModifiers};
    use mush_ai::types::TokenCount;
    use std::rc::Rc;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn streaming_test_config() -> crate::runner::TuiConfig {
        crate::runner::TuiConfig::for_test()
    }

    #[cfg(feature = "signal")]
    struct SignalProbe {
        started: tokio::time::Instant,
        sends: std::sync::atomic::AtomicUsize,
    }

    #[cfg(feature = "signal")]
    impl mush_notify::Clock for SignalProbe {
        fn now(&self) -> Option<std::time::Duration> {
            Some(std::time::Duration::from_secs(1000) + self.started.elapsed())
        }
    }

    #[cfg(feature = "signal")]
    impl mush_notify::ActivitySource for SignalProbe {
        fn snapshot(
            &self,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Option<mush_notify::Snapshot>> + Send + '_>,
        > {
            Box::pin(async {
                Some(mush_notify::Snapshot {
                    observed_at: mush_notify::Clock::now(self).unwrap(),
                    sessions: vec![mush_notify::SessionActivity {
                        local: true,
                        activity: mush_notify::Activity::Locked,
                    }],
                })
            })
        }
    }

    #[cfg(feature = "signal")]
    impl mush_notify::Transport for SignalProbe {
        fn available(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            Box::pin(async { true })
        }

        fn send(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            Box::pin(async {
                self.sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                true
            })
        }
    }

    #[cfg(feature = "signal")]
    fn signal_probe(state: &mut StreamState) -> std::sync::Arc<SignalProbe> {
        let probe = std::sync::Arc::new(SignalProbe {
            started: tokio::time::Instant::now(),
            sends: std::sync::atomic::AtomicUsize::new(0),
        });
        state.signal = Some(
            mush_notify::Dispatcher::new(
                mush_notify::Config {
                    mode: mush_notify::Mode::Send,
                    account: "+12025550100".into(),
                    recipients: vec!["+12025550101".into()],
                    socket: Some("/run/user/1000/private/socket".into()),
                    grace_seconds: 1,
                    ..Default::default()
                },
                probe.clone(),
                probe.clone(),
                probe.clone(),
            )
            .unwrap(),
        );
        probe
    }

    #[cfg(feature = "signal")]
    async fn signal_grace() {
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }

    #[cfg(feature = "signal")]
    #[tokio::test(start_paused = true)]
    async fn signal_idle_activity_cancels_before_delivery() {
        for (event, cancelled) in [
            (Event::Key(key(KeyCode::Char('a'))), true),
            (Event::Paste("synthetic input".into()), true),
            (
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Moved,
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                }),
                true,
            ),
            (Event::FocusGained, true),
            (Event::FocusLost, false),
            (Event::Resize(80, 24), false),
        ] {
            let mut state = StreamState::new();
            let probe = signal_probe(&mut state);
            state
                .signal
                .as_mut()
                .unwrap()
                .completion(mush_notify::Completion(1));
            cancel_signal_for_event(&mut state, &event);
            signal_grace().await;
            assert_eq!(
                probe.sends.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(!cancelled)
            );
            assert_eq!(
                state.signal.as_mut().unwrap().finished().await,
                (!cancelled).then_some(mush_notify::Outcome::Accepted)
            );
        }
    }

    #[cfg(feature = "signal")]
    #[tokio::test(start_paused = true)]
    async fn signal_approval_input_cancels_and_preserves_reply() {
        for (code, expected) in [
            (KeyCode::Char('y'), Some(true)),
            (KeyCode::Char('n'), Some(false)),
            (KeyCode::Char('x'), None),
        ] {
            let mut session = Session::new(App::new("test".into(), TokenCount::new(200_000)));
            let mut state = StreamState::new();
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            let (reply, mut received) = tokio::sync::oneshot::channel();
            state.register_active(
                mush_ai::types::PaneId::new(1),
                mush_runtime::StreamMeta {
                    cancel: tokio_util::sync::CancellationToken::new(),
                    steering_queue: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
                    confirm_req_rx: rx,
                    confirm_reply: std::sync::Arc::new(tokio::sync::Mutex::new(Some(
                        super::super::streams::PendingConfirmation {
                            tool_call_id: "synthetic-tool".into(),
                            reply,
                        },
                    ))),
                    context_tokens: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    model: mush_ai::models::all_models_with_user()
                        .into_iter()
                        .next()
                        .unwrap(),
                },
            );
            session.app.interaction.mode = app::AppMode::ToolConfirm;
            let probe = signal_probe(&mut state);
            state
                .signal
                .as_mut()
                .unwrap()
                .completion(mush_notify::Completion(1));
            let mut tui_config = streaming_test_config();
            let registry = mush_ai::registry::ApiRegistry::new();
            let mut pending_prompt = None;
            let image_picker = None;
            let size_cache = Rc::new(CachedSizeState::default());
            let mut deps = InputDeps {
                tui_config: &mut tui_config,
                thinking_prefs: &mut ThinkingPrefs::default(),
                thinking_saver: &None,
                registry: &registry,
                cwd: std::path::Path::new(""),
                pending_prompt: &mut pending_prompt,
                image_picker: &image_picker,
                size_cache: &size_cache,
            };
            let action = handle_streaming_key_event(&mut session, &mut state, &mut deps, key(code))
                .await
                .unwrap();
            if let Some(allowed) = expected {
                assert!(matches!(action, LoopAction::Redraw));
                assert_eq!(received.try_recv().unwrap(), allowed);
                assert_eq!(session.app.interaction.mode, app::AppMode::Insert);
            } else {
                assert!(matches!(action, LoopAction::Continue));
                assert!(matches!(
                    received.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                ));
                assert_eq!(session.app.interaction.mode, app::AppMode::ToolConfirm);
            }
            signal_grace().await;
            assert_eq!(state.signal.as_mut().unwrap().finished().await, None);
            assert_eq!(probe.sends.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn apply_resize_invalidates_size_cache() {
        let size_cache = Rc::new(CachedSizeState::default());
        size_cache.set_size_for_test(ratatui::layout::Size::new(80, 25));
        assert!(size_cache.has_cached_size());

        apply_resize(&size_cache);

        assert!(
            !size_cache.has_cached_size(),
            "apply_resize should clear the cached size"
        );
    }

    #[tokio::test]
    async fn streaming_key_input_requests_redraw() {
        let mut session = Session::new(App::new("test".into(), TokenCount::new(200_000)));

        session.app.start_tool(
            &mush_ai::types::ToolCallId::from("toolu_test"),
            "bash_status",
            "job 1",
        );
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut stream_state = StreamState::new();
        session.connection.runtime.turns.register_active(
            mush_ai::types::PaneId::new(1),
            mush_runtime::StreamMeta {
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
        let mut tui_config = streaming_test_config();
        let registry = mush_ai::registry::ApiRegistry::new();
        let mut pending_prompt = None;
        let image_picker = None;
        let size_cache = Rc::new(CachedSizeState::default());
        let mut deps = InputDeps {
            tui_config: &mut tui_config,
            thinking_prefs: &mut ThinkingPrefs::default(),
            thinking_saver: &None,
            registry: &registry,
            cwd: std::path::Path::new(""),
            pending_prompt: &mut pending_prompt,
            image_picker: &image_picker,
            size_cache: &size_cache,
        };

        let action = handle_streaming_key_event(
            &mut session,
            &mut stream_state,
            &mut deps,
            key(KeyCode::Char('h')),
        )
        .await
        .unwrap();

        assert!(matches!(action, LoopAction::Redraw));
        assert_eq!(session.app.input.text, "h");
    }

    #[tokio::test]
    async fn slash_command_runs_during_streaming_iteration() {
        let mut session = Session::new(App::new("test".into(), TokenCount::new(200_000)));
        session.app.stream.active = false;
        assert!(!session.app.interaction.show_cost);
        session.app.input.text = "/cost".into();
        session.app.input.cursor = "/cost".len();

        let mut stream_state = StreamState::new();
        let mut tui_config = streaming_test_config();
        let registry = mush_ai::registry::ApiRegistry::new();
        let mut pending_prompt = None;
        let image_picker = None;
        let size_cache = Rc::new(CachedSizeState::default());
        let mut deps = InputDeps {
            tui_config: &mut tui_config,
            thinking_prefs: &mut ThinkingPrefs::default(),
            thinking_saver: &None,
            registry: &registry,
            cwd: std::path::Path::new(""),
            pending_prompt: &mut pending_prompt,
            image_picker: &image_picker,
            size_cache: &size_cache,
        };

        handle_streaming_key_event(
            &mut session,
            &mut stream_state,
            &mut deps,
            key(KeyCode::Enter),
        )
        .await
        .unwrap();

        assert!(
            session.app.interaction.show_cost,
            "a slash command entered during a streaming iteration must execute, not be dropped"
        );
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

#[cfg(test)]
mod clipboard_failure_tests {
    use super::*;
    use mush_ai::types::TokenCount;

    #[test]
    fn a_clipboard_command_that_fails_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("wl-copy");
        std::fs::write(&tool, "#!/bin/sh\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = crate::clipboard::Environment {
            ssh: false,
            tool: Some(tool.to_string_lossy().into_owned()),
        };
        let mut app = App::new("test".into(), TokenCount::new(200_000));

        copy_to_clipboard(&mut app, "world", &env, &mut std::io::sink());

        let status = app.status.expect("a status");
        assert_eq!(status.text, format!("clipboard failed: {}", tool.display()));
        assert!(status.error);
    }
}
