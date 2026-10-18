use std::io;

use crossterm::event::{self, Event, KeyEvent, KeyEventKind};

use crate::app::{self, App, AppEvent};
use crate::input::handle_key;
use crate::question::{Answer, Question};
use crate::session::Session;

use super::TuiConfig;
use super::commands::{SlashEnv, handle_slash_action};
use super::streams::{
    StreamState, abort_focused_stream, answer_confirmation, edit_last_queued_steering,
    submit_streaming_input,
};
use crate::slash;

const RESIZE_QUIET: std::time::Duration = std::time::Duration::from_millis(16);
const RESIZE_MAX_WAIT: std::time::Duration = std::time::Duration::from_millis(50);
const MAX_TERMINAL_EVENTS_PER_ITERATION: usize = 64;

#[derive(Debug, Default)]
pub(super) struct ResizeState {
    first: Option<std::time::Instant>,
    latest: Option<(std::time::Instant, u16, u16)>,
}

impl ResizeState {
    fn note(&mut self, now: std::time::Instant, width: u16, height: u16) {
        self.first.get_or_insert(now);
        self.latest = Some((now, width, height));
    }

    pub(super) fn deadline(&self) -> Option<std::time::Instant> {
        let latest = self.latest?.0;
        let first = self.first.unwrap_or(latest);
        Some((latest + RESIZE_QUIET).min(first + RESIZE_MAX_WAIT))
    }

    pub(super) fn pending(&self) -> bool {
        self.latest.is_some()
    }

    pub(super) fn take_due(&mut self, now: std::time::Instant) -> Option<(u16, u16)> {
        if !self.deadline().is_some_and(|deadline| now >= deadline) {
            return None;
        }
        self.first = None;
        self.latest.take().map(|(_, width, height)| (width, height))
    }
}

#[derive(Debug)]
pub(super) enum LoopAction {
    Continue,
    Redraw,
    Quit,
}

pub(super) struct InputDeps<'a> {
    pub tui_config: &'a mut TuiConfig,
    pub pending_prompt: &'a mut Option<String>,
    pub image_picker: &'a Option<ratatui_image::picker::Picker>,
    pub resize: &'a mut ResizeState,
}

impl InputDeps<'_> {
    fn slash_env(&mut self) -> SlashEnv<'_> {
        SlashEnv {
            tui_config: self.tui_config,
            pending_prompt: self.pending_prompt,
        }
    }
}

async fn handle_question_key(
    session: &mut Session,
    deps: &mut InputDeps<'_>,
    key: KeyEvent,
) -> LoopAction {
    let (Some(question), Some(answer)) = (Question::open(&session.app), Answer::from_key(key.code))
    else {
        return LoopAction::Continue;
    };
    match question {
        Question::Tool => answer_confirmation(session, answer == Answer::Yes).await,
        Question::KernelBusy => {
            super::streams::answer_kernel_busy(session, kernel_busy_choice(answer)).await;
        }
        Question::BranchSummary => {
            answer_branch_summary(session, deps.tui_config, branch_summary_choice(answer)).await;
        }
    }
    LoopAction::Redraw
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchSummaryChoice {
    Summarise,
    JustJump,
    Stay,
}

fn branch_summary_choice(answer: Answer) -> BranchSummaryChoice {
    match answer {
        Answer::Yes => BranchSummaryChoice::Summarise,
        Answer::No => BranchSummaryChoice::JustJump,
        Answer::Dismiss => BranchSummaryChoice::Stay,
    }
}

async fn answer_branch_summary(
    session: &mut Session,
    tui_config: &mut TuiConfig,
    choice: BranchSummaryChoice,
) {
    let app = &mut session.app;
    app.interaction.mode = app::AppMode::Insert;
    let Some(entry_id) = app.interaction.branch_summary_ask.take() else {
        return;
    };
    match choice {
        BranchSummaryChoice::Stay => {}
        BranchSummaryChoice::JustJump => {
            super::commands::run_session_command(
                session,
                tui_config,
                mush_runtime::SessionCommand::NavigateTree {
                    entry_id,
                    summary: None,
                },
            )
            .await;
        }
        BranchSummaryChoice::Summarise => {
            super::commands::send_command(
                session,
                tui_config,
                mush_runtime::ClientCommand::SummariseAndNavigate { entry_id },
            )
            .await;
        }
    }
}

fn kernel_busy_choice(answer: Answer) -> mush_tools::ipython::BusyKernelChoice {
    use mush_tools::ipython::BusyKernelChoice;
    match answer {
        Answer::Yes => BusyKernelChoice::Wait,
        Answer::No => BusyKernelChoice::Kill,
        Answer::Dismiss => BusyKernelChoice::Cancel,
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
    deps: &mut InputDeps<'_>,
) -> Option<LoopAction> {
    match app_event {
        AppEvent::Quit => Some(LoopAction::Quit),
        AppEvent::CycleThinkingLevel => Some(LoopAction::Redraw),
        AppEvent::PasteImage => {
            paste_clipboard_image(session, deps.tui_config).await;
            Some(LoopAction::Redraw)
        }
        AppEvent::ExternalEditor => {
            super::external_editor::edit(&mut session.app, deps.tui_config.terminal_policy);
            Some(LoopAction::Redraw)
        }
        AppEvent::Copy { text } => {
            let env = crate::clipboard::Environment::detect();
            copy_to_clipboard(&mut session.app, text, &env, &mut std::io::stdout());
            Some(LoopAction::Redraw)
        }
        AppEvent::PutClipboard { before, count } => {
            put_clipboard_text(&mut session.app, *before, *count).await;
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
        AppEvent::DecisionDrafts(drafts) => {
            for (key, answer) in drafts {
                let command = mush_runtime::DecisionsCommand::Draft {
                    key: key.clone(),
                    answer: answer.clone(),
                };
                super::commands::send_command(
                    session,
                    deps.tui_config,
                    mush_runtime::ClientCommand::Decisions(command),
                )
                .await;
            }
            Some(LoopAction::Redraw)
        }
        AppEvent::DecisionsSend { keys, confirms } => {
            for (child, allow) in confirms {
                super::commands::send_command(
                    session,
                    deps.tui_config,
                    mush_runtime::ClientCommand::ConfirmChildTool {
                        child: child.clone(),
                        allow: *allow,
                    },
                )
                .await;
            }
            if !keys.is_empty() {
                super::commands::send_command(
                    session,
                    deps.tui_config,
                    mush_runtime::ClientCommand::Decisions(mush_runtime::DecisionsCommand::Send {
                        keys: keys.clone(),
                    }),
                )
                .await;
            }
            Some(LoopAction::Redraw)
        }
        AppEvent::DecisionsRefresh => {
            super::commands::refresh_decisions(session, deps.tui_config).await;
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
    session.app.interaction.mode = app::AppMode::Insert;
    if session.app.interaction.compaction_recovery.take().is_none() {
        return;
    }

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
            super::runtime::promote_queued_inputs(session);
            return;
        }
    };
    super::commands::send_command(session, deps.tui_config, command).await;
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

pub(super) enum SaveOutcome {
    Saved(String),
    Failed(String),
}

pub(super) async fn run_validation_and_save(entry_id: &str, key: &str) -> SaveOutcome {
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
    let mut handled = 0;
    while handled < MAX_TERMINAL_EVENTS_PER_ITERATION && event::poll(std::time::Duration::ZERO)? {
        let event = event::read()?;
        handled += 1;
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
                deps.resize.note(std::time::Instant::now(), w, h);
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
        return Ok(handle_question_key(session, deps, key).await);
    }

    let app_event = handle_key(&mut session.app, key);
    let Some(app_event) = app_event else {
        return Ok(LoopAction::Redraw);
    };

    if let Some(action) = handle_common_app_event(&app_event, session, deps).await {
        return Ok(match action {
            LoopAction::Continue => LoopAction::Redraw,
            action => action,
        });
    }

    match app_event {
        AppEvent::Abort => {
            abort_focused_stream(session).await;
        }
        AppEvent::UserSubmit { text, images } => {
            submit_streaming_input(session, text, images).await;
            record_model_use(session, deps.tui_config);
        }
        AppEvent::EditSteering => {
            edit_last_queued_steering(session).await;
        }
        AppEvent::SlashCommand { action } => {
            handle_slash_action(session, action, deps.slash_env()).await;
        }
        AppEvent::SettingsToggleSelected => {
            apply_settings_toggle(session, deps.tui_config).await;
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
    let now = std::time::Instant::now();
    let wait = deps
        .resize
        .deadline()
        .map_or(std::time::Duration::from_millis(50), |deadline| {
            deadline
                .saturating_duration_since(now)
                .min(std::time::Duration::from_millis(50))
        });
    let wait = if session.app.render_state.transcript_pending.get() {
        std::time::Duration::ZERO
    } else {
        wait
    };
    if !event::poll(wait)? {
        return Ok(LoopAction::Continue);
    }

    let mut changed = false;
    let mut handled = 0;
    while handled < MAX_TERMINAL_EVENTS_PER_ITERATION
        && (handled == 0 || event::poll(std::time::Duration::ZERO)?)
    {
        let event = event::read()?;
        handled += 1;
        cancel_signal_for_event(stream_state, &event);
        changed |= !matches!(&event, Event::Resize(..));
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match handle_idle_key(session, deps, key).await {
                    IdleStep::Quit => return Ok(LoopAction::Quit),
                    IdleStep::ReadNext => continue,
                    IdleStep::Done => {}
                }
            }
            Event::Mouse(mouse) => {
                crate::input::handle_mouse(&mut session.app, mouse);
            }
            Event::Paste(text) => {
                session.app.handle_paste(&text);
            }
            Event::Resize(w, h) => {
                deps.resize.note(std::time::Instant::now(), w, h);
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
    }

    Ok(if changed {
        LoopAction::Redraw
    } else {
        LoopAction::Continue
    })
}

enum IdleStep {
    Done,
    Quit,
    ReadNext,
}

async fn handle_idle_key(
    session: &mut Session,
    deps: &mut InputDeps<'_>,
    key: KeyEvent,
) -> IdleStep {
    if session.app.interaction.mode == app::AppMode::ToolConfirm {
        if !session.app.subagents.handle_key(&session.app.keymap, key) {
            handle_question_key(session, deps, key).await;
        }
        return IdleStep::Done;
    }
    let Some(app_event) = handle_key(&mut session.app, key) else {
        return IdleStep::ReadNext;
    };
    handle_idle_app_event(session, deps, app_event).await
}

async fn handle_idle_app_event(
    session: &mut Session,
    deps: &mut InputDeps<'_>,
    app_event: AppEvent,
) -> IdleStep {
    if let Some(action) = handle_common_app_event(&app_event, session, deps).await {
        return match action {
            LoopAction::Quit => IdleStep::Quit,
            _ => IdleStep::Done,
        };
    }
    match app_event {
        AppEvent::Abort if session.connection.state().compacting => {
            super::commands::send_command(
                session,
                deps.tui_config,
                mush_runtime::ClientCommand::Abort,
            )
            .await;
        }
        AppEvent::UserSubmit { text, images } => {
            if session.connection.state().compacting {
                session.queue_input_during_compaction(text, images).await;
                return IdleStep::ReadNext;
            }
            submit_idle_prompt(session, deps, text, images).await;
        }
        AppEvent::SlashCommand { action } => {
            handle_slash_action(session, action, deps.slash_env()).await;
        }
        AppEvent::SettingsToggleSelected => {
            apply_settings_toggle(session, deps.tui_config).await;
        }
        AppEvent::ModelSelected { model_id } => {
            handle_slash_action(
                session,
                crate::slash::SlashAction::Model {
                    model_id: Some(model_id),
                    show_all: false,
                },
                deps.slash_env(),
            )
            .await;
        }
        AppEvent::PersistFavourites => {
            if let Some(ref saver) = deps.tui_config.save_favourite_models {
                saver(&session.app.completion.favourite_models);
            }
        }
        _ => {}
    }
    IdleStep::Done
}

async fn submit_idle_prompt(
    session: &mut Session,
    deps: &mut InputDeps<'_>,
    text: String,
    images: Vec<crate::input_buffer::PendingImage>,
) {
    let expanded = slash::expand_template(&text);
    if session.label.is_none() && session.connection.state().context_len == 0 {
        session.label = Some(expanded.chars().take(30).collect());
    }
    let image_bytes: Vec<Vec<u8>> = images.iter().map(|img| img.data.clone()).collect();
    let msg_idx = session.app.messages.len();
    if let Some(picker) = deps.image_picker {
        for (img_idx, data) in image_bytes.iter().enumerate() {
            if let Ok(dyn_img) = image::load_from_memory(data) {
                let proto = picker.new_resize_protocol(dyn_img);
                session.image_protos.insert((msg_idx, img_idx), proto);
            }
        }
    }
    session
        .app
        .push_user_message_with_images(expanded.clone(), image_bytes);
    session.app.active_tools.clear();
    session.app.start_streaming();
    super::commands::send_command(
        session,
        deps.tui_config,
        mush_runtime::ClientCommand::Prompt {
            text: expanded,
            images,
        },
    )
    .await;
}

fn cancel_signal_for_event(stream_state: &mut StreamState, event: &Event) {
    if matches!(
        event,
        Event::Key(_) | Event::Paste(_) | Event::Mouse(_) | Event::FocusGained
    ) {
        stream_state.cancel_signal();
    }
}

async fn apply_settings_toggle(
    session: &mut crate::session::Session,
    tui_config: &mut crate::runner::TuiConfig,
) {
    let Some(item) = session
        .app
        .settings_menu
        .as_ref()
        .map(|menu| menu.current())
    else {
        return;
    };
    if item == crate::settings::MenuItemKind::SubagentModel {
        session.app.settings_menu = None;
        session.app.open_subagent_default_picker();
        return;
    }
    let new_value = item.activate(&mut tui_config.settings);
    if !item.persists() {
        let on = tui_config.settings.auto_compact;
        if super::commands::send_command(
            session,
            tui_config,
            mush_runtime::ClientCommand::SetAutoCompact(on),
        )
        .await
        {
            session.app.status =
                Some(format!("{} → {new_value} for this session", item.label()).into());
        }
        tui_config.settings.auto_compact = session.connection.state().auto_compact;
        return;
    }
    tui_config.options.anthropic_betas = Some(tui_config.settings.anthropic_betas.clone());
    let command = match item {
        crate::settings::MenuItemKind::PriceCliffPin => Some(
            mush_runtime::ClientCommand::SetPriceCliffPin(tui_config.settings.price_cliff_pin),
        ),
        _ if item.is_beta() => Some(mush_runtime::ClientCommand::SetAnthropicBetas(
            tui_config.settings.anthropic_betas.clone(),
        )),
        _ => None,
    };
    let persisted = crate::settings::persist(&tui_config.settings, &tui_config.cwd);
    let live = match command {
        None => String::new(),
        Some(command) => match session.connection.send(command).await {
            Ok(events) => {
                super::looping::apply_command_events(session, tui_config, events);
                crate::settings::LIVE_FROM_NEXT_TURN.to_string()
            }
            Err(error) => crate::settings::not_live(&error.to_string(), &persisted),
        },
    };
    let persist_note = match persisted {
        Ok(Some(path)) => format!(" (saved to {})", path.display()),
        Ok(None) => String::new(),
        Err(e) => format!(" (persist failed: {e})"),
    };
    session.app.status = Some(format!("{} → {new_value}{live}{persist_note}", item.label()).into());
}

fn copy_to_clipboard(
    app: &mut App,
    text: &str,
    env: &crate::clipboard::Environment,
    out: &mut impl std::io::Write,
) {
    if let Err(error) = crate::clipboard::copy(text, env, out) {
        app.show_error(error.to_string());
    }
}

async fn put_clipboard_text(app: &mut App, before: bool, count: Option<usize>) {
    match tokio::task::spawn_blocking(crate::clipboard::read_clipboard_text).await {
        Ok(Some(text)) => crate::normal::edit::put_text(app, text, before, count),
        Ok(None) | Err(_) => app.status = Some("no text in the clipboard".into()),
    }
}

async fn paste_clipboard_image(session: &mut Session, tui_config: &TuiConfig) {
    session.app.status = Some("reading clipboard…".into());
    let image = match tokio::task::spawn_blocking(crate::clipboard::read_clipboard_image).await {
        Ok(Some(image)) => image,
        Ok(None) => {
            session.app.status = Some("no image in clipboard".into());
            return;
        }
        Err(_) => {
            session.app.status = Some("failed to read clipboard".into());
            return;
        }
    };
    let state = session.connection.state();
    let model = mush_ai::discovery::resolve_model_by_id(&state.model_id)
        .unwrap_or_else(|| tui_config.model.clone());
    let originals = mush_session::SessionStore::pasted_images_dir(&state.session_id);
    let prepared = tokio::task::spawn_blocking(move || {
        crate::pasted_image::prepare(image, &model, &originals)
    })
    .await;
    session.app.status = Some(
        match prepared {
            Ok(Ok(pasted)) => {
                session.app.input.attach_image(pasted.image);
                pasted.status
            }
            Ok(Err(error)) => error,
            Err(_) => "failed to read the image".into(),
        }
        .into(),
    );
}

#[cfg(test)]
mod resize_tests {
    use super::*;

    #[test]
    fn resize_waits_for_a_quiet_window() {
        let start = std::time::Instant::now();
        let mut resize = ResizeState::default();
        resize.note(start, 80, 24);
        resize.note(start + std::time::Duration::from_millis(8), 90, 30);

        assert_eq!(
            resize.take_due(start + std::time::Duration::from_millis(23)),
            None
        );
        assert_eq!(
            resize.take_due(start + std::time::Duration::from_millis(24)),
            Some((90, 30))
        );
        assert!(!resize.pending());
    }

    #[test]
    fn continuous_resize_has_a_maximum_wait() {
        let start = std::time::Instant::now();
        let mut resize = ResizeState::default();
        resize.note(start, 80, 24);
        resize.note(start + std::time::Duration::from_millis(45), 100, 40);

        assert_eq!(resize.deadline(), Some(start + RESIZE_MAX_WAIT));
        assert_eq!(resize.take_due(start + RESIZE_MAX_WAIT), Some((100, 40)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::size_cached_backend::CachedSizeState;
    use crate::session::Session;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventState, KeyModifiers};
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
            let mut pending_prompt = None;
            let image_picker = None;
            let mut resize = ResizeState::default();
            let mut deps = InputDeps {
                tui_config: &mut tui_config,
                pending_prompt: &mut pending_prompt,
                image_picker: &image_picker,
                resize: &mut resize,
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

    #[tokio::test]
    async fn answering_n_to_the_branch_question_jumps_without_a_summary() {
        let mut session = Session::with_conversation(
            App::new("test".into(), TokenCount::new(200_000)),
            mush_session::ConversationState::from_messages(vec![
                crate::test_support::user_message("first"),
                crate::test_support::user_message("second"),
            ]),
        );
        let target = session.in_process().runtime.conversation.tree().entries()[1]
            .id
            .clone();
        session.app.interaction.branch_summary_ask = Some(target);
        session.app.interaction.mode = app::AppMode::ToolConfirm;
        let mut config = streaming_test_config();

        answer_branch_summary(&mut session, &mut config, BranchSummaryChoice::JustJump).await;

        assert_eq!(session.app.interaction.mode, app::AppMode::Insert);
        assert!(session.app.interaction.branch_summary_ask.is_none());
        assert_eq!(session.in_process().runtime.conversation.context_len(), 2);
        assert_eq!(session.app.input.text, "second");
    }

    #[tokio::test]
    async fn esc_on_the_branch_question_stays_put() {
        let mut session = Session::with_conversation(
            App::new("test".into(), TokenCount::new(200_000)),
            mush_session::ConversationState::from_messages(vec![
                crate::test_support::user_message("first"),
                crate::test_support::user_message("second"),
            ]),
        );
        let target = session.in_process().runtime.conversation.tree().entries()[1]
            .id
            .clone();
        session.app.interaction.branch_summary_ask = Some(target);
        session.app.interaction.mode = app::AppMode::ToolConfirm;
        let mut config = streaming_test_config();

        answer_branch_summary(&mut session, &mut config, BranchSummaryChoice::Stay).await;

        assert_eq!(session.app.interaction.mode, app::AppMode::Insert);
        assert_eq!(session.in_process().runtime.conversation.context_len(), 2);
    }

    #[test]
    fn answers_map_onto_each_question() {
        use mush_tools::ipython::BusyKernelChoice;
        assert_eq!(
            branch_summary_choice(Answer::Yes),
            BranchSummaryChoice::Summarise
        );
        assert_eq!(
            branch_summary_choice(Answer::No),
            BranchSummaryChoice::JustJump
        );
        assert_eq!(
            branch_summary_choice(Answer::Dismiss),
            BranchSummaryChoice::Stay
        );
        assert_eq!(kernel_busy_choice(Answer::Yes), BusyKernelChoice::Wait);
        assert_eq!(kernel_busy_choice(Answer::No), BusyKernelChoice::Kill);
        assert_eq!(
            kernel_busy_choice(Answer::Dismiss),
            BusyKernelChoice::Cancel
        );
    }

    fn asking_about_the_branch(draft: &str) -> Session {
        let mut session = Session::with_conversation(
            App::new("test".into(), TokenCount::new(200_000)),
            mush_session::ConversationState::from_messages(vec![
                crate::test_support::user_message("first"),
                crate::test_support::user_message("second"),
            ]),
        );
        let target = session.in_process().runtime.conversation.tree().entries()[1]
            .id
            .clone();
        session.app.interaction.branch_summary_ask = Some(target);
        session.app.interaction.mode = app::AppMode::ToolConfirm;
        session.app.input.text = draft.into();
        session.app.input.cursor = draft.len();
        session
    }

    #[tokio::test]
    async fn the_settings_subagent_row_opens_a_picker_for_the_default() {
        let mut session = Session::new(App::new("test".into(), TokenCount::new(200_000)));
        session.app.completion.model_completions = vec![crate::slash_menu::ModelCompletion {
            id: "claude-haiku-5-5".into(),
            name: "Claude Haiku 5.5".into(),
            provider: "anthropic".into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: None,
        }];
        let mut tui_config = streaming_test_config();
        let index = crate::settings::MenuItemKind::all()
            .iter()
            .position(|item| *item == crate::settings::MenuItemKind::SubagentModel)
            .unwrap();
        session.app.settings_menu = Some(crate::settings::SettingsMenuState {
            selected: index,
            scroll_offset: 0,
        });

        apply_settings_toggle(&mut session, &mut tui_config).await;

        assert!(session.app.settings_menu.is_none());
        let picker = session.app.interaction.model_picker.as_ref().unwrap();
        assert_eq!(
            picker.purpose,
            crate::model_picker::ModelPickerPurpose::SubagentDefault
        );
        assert_eq!(picker.models[0].id, crate::model_picker::INHERIT_ID);
    }

    #[tokio::test]
    async fn a_beta_toggle_in_the_settings_menu_reaches_the_agent() {
        let mut session = Session::new(App::new("test".into(), TokenCount::new(200_000)));
        let mut tui_config = streaming_test_config();
        let index = crate::settings::MenuItemKind::all()
            .iter()
            .position(|item| *item == crate::settings::MenuItemKind::Effort)
            .unwrap();
        session.app.settings_menu = Some(crate::settings::SettingsMenuState {
            selected: index,
            scroll_offset: 0,
        });
        assert!(tui_config.settings.anthropic_betas.effort);

        apply_settings_toggle(&mut session, &mut tui_config).await;

        let live = session
            .connection
            .state()
            .anthropic_betas
            .expect("the agent took the betas");
        assert!(!live.effort);
        assert_eq!(live, tui_config.settings.anthropic_betas);
        assert_eq!(
            session
                .app
                .status
                .as_ref()
                .map(|status| status.text.as_str()),
            Some("effort → off from the next turn, subagents included")
        );
    }

    #[tokio::test]
    async fn the_settings_menu_switches_auto_compaction_for_this_session() {
        let mut session = Session::new(App::new("test".into(), TokenCount::new(200_000)));
        let mut tui_config = streaming_test_config();
        let index = crate::settings::MenuItemKind::all()
            .iter()
            .position(|item| *item == crate::settings::MenuItemKind::AutoCompact)
            .unwrap();
        session.app.settings_menu = Some(crate::settings::SettingsMenuState {
            selected: index,
            scroll_offset: 0,
        });
        session
            .connection
            .send(mush_runtime::ClientCommand::SetAutoCompact(true))
            .await
            .unwrap();
        tui_config.settings.auto_compact = session.connection.state().auto_compact;
        assert_eq!(
            crate::settings::MenuItemKind::AutoCompact.value(&tui_config.settings),
            "on"
        );

        apply_settings_toggle(&mut session, &mut tui_config).await;

        assert!(!session.connection.state().auto_compact);
        assert!(!tui_config.settings.auto_compact);
        assert_eq!(
            session
                .app
                .status
                .as_ref()
                .map(|status| status.text.as_str()),
            Some("auto_compact → off for this session")
        );

        apply_settings_toggle(&mut session, &mut tui_config).await;
        assert!(session.connection.state().auto_compact);
    }

    #[tokio::test]
    async fn ctrl_c_cancels_a_running_compaction_and_the_view_stops_compacting() {
        let (connection, _release) = mush_runtime::testing::stalled_connection();
        let mut session = Session::with_connection(
            App::new("test".into(), TokenCount::new(200_000)),
            Box::new(connection),
        );
        session.in_process().runtime.conversation = mush_session::ConversationState::from_messages(
            (0..12)
                .map(|i| crate::test_support::user_message(&format!("turn {i}")))
                .collect(),
        );
        let mut tui_config = streaming_test_config();
        super::super::commands::send_command(
            &mut session,
            &mut tui_config,
            mush_runtime::ClientCommand::Compact {
                focus: mush_session::compact::CompactionFocus::default(),
                auto: false,
            },
        )
        .await;
        assert!(session.app.compacting);
        let mut pending_prompt = None;
        let image_picker = None;
        let mut resize = ResizeState::default();
        let mut deps = InputDeps {
            tui_config: &mut tui_config,
            pending_prompt: &mut pending_prompt,
            image_picker: &image_picker,
            resize: &mut resize,
        };

        let ctrl_c = KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..key(KeyCode::Char('c'))
        };
        handle_idle_key(&mut session, &mut deps, ctrl_c).await;

        assert!(!session.app.compacting);
        assert!(!session.connection.state().compacting);
        assert_eq!(
            session
                .app
                .status
                .as_ref()
                .map(|status| status.text.as_str()),
            Some("compaction cancelled")
        );
        assert_eq!(session.in_process().runtime.conversation.context_len(), 12);
    }

    #[tokio::test]
    async fn an_idle_question_keeps_typing_out_of_the_draft() {
        let mut session = asking_about_the_branch("half typed");
        let mut tui_config = streaming_test_config();
        let mut pending_prompt = None;
        let image_picker = None;
        let mut resize = ResizeState::default();
        let mut deps = InputDeps {
            tui_config: &mut tui_config,
            pending_prompt: &mut pending_prompt,
            image_picker: &image_picker,
            resize: &mut resize,
        };

        for code in [KeyCode::Char('x'), KeyCode::Backspace, KeyCode::Tab] {
            handle_idle_key(&mut session, &mut deps, key(code)).await;
        }
        assert_eq!(session.app.interaction.mode, app::AppMode::ToolConfirm);
        assert!(session.app.interaction.branch_summary_ask.is_some());
        assert_eq!(session.app.input.text, "half typed");

        handle_idle_key(&mut session, &mut deps, key(KeyCode::Esc)).await;
        assert_eq!(session.app.interaction.mode, app::AppMode::Insert);
        assert!(session.app.interaction.branch_summary_ask.is_none());
        assert_eq!(session.in_process().runtime.conversation.context_len(), 2);
        assert_eq!(session.app.input.text, "half typed");
        assert_eq!(session.app.input.cursor, "half typed".len());
    }

    #[tokio::test]
    async fn an_idle_no_to_the_branch_question_jumps() {
        let mut session = asking_about_the_branch("");
        let mut tui_config = streaming_test_config();
        let mut pending_prompt = None;
        let image_picker = None;
        let mut resize = ResizeState::default();
        let mut deps = InputDeps {
            tui_config: &mut tui_config,
            pending_prompt: &mut pending_prompt,
            image_picker: &image_picker,
            resize: &mut resize,
        };

        handle_idle_key(&mut session, &mut deps, key(KeyCode::Char('n'))).await;

        assert_eq!(session.app.interaction.mode, app::AppMode::Insert);
        assert_eq!(session.in_process().runtime.conversation.context_len(), 2);
    }

    #[tokio::test]
    async fn a_tool_question_swallows_other_keys_and_keeps_the_draft() {
        let mut session = Session::new(App::new("test".into(), TokenCount::new(200_000)));
        let mut state = StreamState::new();
        session.app.interaction.mode = app::AppMode::ToolConfirm;
        session.app.interaction.confirm_prompt = Some("bash ls".into());
        session.app.input.text = "half typed".into();
        session.app.input.cursor = 4;
        let mut tui_config = streaming_test_config();
        let mut pending_prompt = None;
        let image_picker = None;
        let mut resize = ResizeState::default();
        let mut deps = InputDeps {
            tui_config: &mut tui_config,
            pending_prompt: &mut pending_prompt,
            image_picker: &image_picker,
            resize: &mut resize,
        };

        let action = handle_streaming_key_event(
            &mut session,
            &mut state,
            &mut deps,
            key(KeyCode::Char('x')),
        )
        .await
        .unwrap();
        assert!(matches!(action, LoopAction::Continue));
        assert_eq!(session.app.interaction.mode, app::AppMode::ToolConfirm);

        let action =
            handle_streaming_key_event(&mut session, &mut state, &mut deps, key(KeyCode::Esc))
                .await
                .unwrap();
        assert!(matches!(action, LoopAction::Redraw));
        assert_eq!(session.app.interaction.mode, app::AppMode::Insert);
        assert!(session.app.interaction.confirm_prompt.is_none());
        assert_eq!(session.app.input.text, "half typed");
        assert_eq!(session.app.input.cursor, 4);
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
        session.in_process().runtime.turns.register_active(
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
        let mut pending_prompt = None;
        let image_picker = None;
        let mut resize = ResizeState::default();
        let mut deps = InputDeps {
            tui_config: &mut tui_config,
            pending_prompt: &mut pending_prompt,
            image_picker: &image_picker,
            resize: &mut resize,
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
        let mut pending_prompt = None;
        let image_picker = None;
        let mut resize = ResizeState::default();
        let mut deps = InputDeps {
            tui_config: &mut tui_config,
            pending_prompt: &mut pending_prompt,
            image_picker: &image_picker,
            resize: &mut resize,
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
        std::fs::write(&tool, "#!/bin/sh\ncat >/dev/null\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = crate::clipboard::Environment {
            ssh: false,
            tool: Some(tool.to_string_lossy().into_owned()),
        };
        let mut app = App::new("test".into(), TokenCount::new(200_000));

        copy_to_clipboard(&mut app, "world", &env, &mut std::io::sink());

        let last = app.messages.last().expect("an error line");
        assert_eq!(
            last.content,
            format!("clipboard failed: {}", tool.display())
        );
        assert_eq!(last.role, crate::app::MessageRole::Status { error: true });
    }
}
