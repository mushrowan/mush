use std::io;

use mush_agent::AgentEvent;
use mush_ai::registry::ApiRegistry;

use crate::event_handler;

use super::TuiConfig;
use super::commands::{drain_finished_browser_logins, refresh_models_after_login};
use super::input::{
    InputDeps, LoopAction, handle_idle_terminal_events, handle_streaming_terminal_events,
};
use super::runtime::RunnerRuntime;
use super::streams::{StreamState, poll_confirmation_prompt, poll_live_tool_output};

pub(super) const ANIMATION_FRAME_INTERVAL: std::time::Duration =
    crate::app::ANIMATION_FRAME_INTERVAL;
const MAX_AGENT_EVENTS_PER_FRAME: usize = 32;
const MAX_AGENT_EVENT_DRAIN: std::time::Duration = std::time::Duration::from_millis(8);

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_loop_iteration(
    stream_state: &mut StreamState,
    runtime: &mut RunnerRuntime,
    tui_config: &mut TuiConfig,
    registry: &ApiRegistry,
    image_picker: &Option<ratatui_image::picker::Picker>,
    size_cache: &std::rc::Rc<super::size_cached_backend::CachedSizeState>,
    last_redraw: std::time::Instant,
) -> io::Result<LoopAction> {
    let action = if !runtime.session.runtime.turn_active() {
        handle_idle_iteration(
            stream_state,
            runtime,
            tui_config,
            registry,
            image_picker,
            size_cache,
        )
        .await
    } else {
        handle_streaming_iteration(
            stream_state,
            runtime,
            tui_config,
            registry,
            image_picker,
            size_cache,
            last_redraw,
        )
        .await
    }?;
    let store = super::subagents::store(tui_config);
    let panel_changed = runtime
        .session
        .app
        .subagents
        .poll(store, std::time::Instant::now())
        .await;
    Ok(if panel_changed && matches!(action, LoopAction::Continue) {
        LoopAction::Redraw
    } else {
        action
    })
}

fn input_parts<'a>(
    runtime: &'a mut RunnerRuntime,
    tui_config: &'a mut TuiConfig,
    registry: &'a ApiRegistry,
    image_picker: &'a Option<ratatui_image::picker::Picker>,
    size_cache: &'a std::rc::Rc<super::size_cached_backend::CachedSizeState>,
) -> (&'a mut crate::session::Session, InputDeps<'a>) {
    let RunnerRuntime {
        session,
        thinking_prefs,
        thinking_saver,
        lifecycle_hooks,
        cwd,
        pending_prompt,
        pending_compactions,
        ..
    } = runtime;
    (
        session,
        InputDeps {
            tui_config,
            thinking_prefs,
            thinking_saver,
            registry,
            lifecycle_hooks,
            cwd,
            pending_prompt,
            pending_compactions,
            image_picker,
            size_cache,
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn handle_streaming_iteration(
    stream_state: &mut StreamState,
    runtime: &mut RunnerRuntime,
    tui_config: &mut TuiConfig,
    registry: &ApiRegistry,
    image_picker: &Option<ratatui_image::picker::Picker>,
    size_cache: &std::rc::Rc<super::size_cached_backend::CachedSizeState>,
    last_redraw: std::time::Instant,
) -> io::Result<LoopAction> {
    let tick = tokio::time::sleep(ANIMATION_FRAME_INTERVAL);
    tokio::pin!(tick);
    let mut saw_agent_event = false;
    let mut saw_settling_agent_event = false;
    let terminal_action;
    let mut tick_changed = false;

    tokio::select! {
        result = runtime.session.runtime.turns.next() => {
            let drain_started = std::time::Instant::now();
            if let Some((pane_id, stream_gen, event)) = result {
                saw_agent_event = true;
                saw_settling_agent_event |= is_settling_agent_event(&event);
                dispatch_agent_event(pane_id, stream_gen, event, stream_state, runtime, tui_config, registry, image_picker).await;
                trace_draw_reason("agent_event");
                runtime.tick_streaming_text_panes();
            }

            let mut drained_events = 0usize;
            loop {
                if !should_continue_agent_drain(
                    drained_events,
                    drain_started,
                    std::time::Instant::now(),
                ) {
                    break;
                }
                match runtime.session.runtime.turns.try_next() {
                    Some((pane_id, stream_gen, event)) => {
                        saw_agent_event = true;
                        saw_settling_agent_event |= is_settling_agent_event(&event);
                        drained_events += 1;
                        dispatch_agent_event(pane_id, stream_gen, event, stream_state, runtime, tui_config, registry, image_picker).await;
                        trace_draw_reason("agent_event_drain");
                        runtime.tick_streaming_text_panes();
                    }
                    _ => break,
                }
            }

            let (session, mut deps) = input_parts(runtime, tui_config, registry, image_picker, size_cache);
            let action = handle_streaming_terminal_events(session, stream_state, &mut deps).await?;
            if matches!(action, LoopAction::Quit) {
                return Ok(LoopAction::Quit);
            }
            terminal_action = action;
        }
        _ = tick => {
            if runtime.tick_animation_panes() {
                tick_changed = true;
                trace_draw_reason("animation_tick");
            }
            if poll_confirmation_prompt(&mut runtime.session, stream_state).await {
                tick_changed = true;
                trace_draw_reason("confirmation_prompt");
            }
            if poll_live_tool_output(&mut runtime.session, &tui_config.tool_output_live) {
                tick_changed = true;
                trace_draw_reason("live_tool_output");
            }
            if drain_session_picker(&mut runtime.session) {
                tick_changed = true;
                trace_draw_reason("session_picker");
            }
            if drain_browser_logins(&mut runtime.session, tui_config).await {
                tick_changed = true;
                trace_draw_reason("browser_login");
            }
            stream_state.save_dirty_session(&mut runtime.session, tui_config);

            let (session, mut deps) = input_parts(runtime, tui_config, registry, image_picker, size_cache);
            let action = handle_streaming_terminal_events(session, stream_state, &mut deps).await?;
            if matches!(action, LoopAction::Quit) {
                return Ok(LoopAction::Quit);
            }
            terminal_action = action;
        }
    }

    Ok(streaming_iteration_action(
        saw_agent_event,
        saw_settling_agent_event,
        terminal_action,
        tick_changed,
        last_redraw,
        std::time::Instant::now(),
    ))
}

fn is_settling_agent_event(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::AgentEnd)
}

fn should_continue_agent_drain(
    drained_events: usize,
    drain_started: std::time::Instant,
    now: std::time::Instant,
) -> bool {
    drained_events < MAX_AGENT_EVENTS_PER_FRAME
        && now.duration_since(drain_started) < MAX_AGENT_EVENT_DRAIN
}

fn streaming_iteration_action(
    saw_agent_event: bool,
    saw_settling_agent_event: bool,
    terminal_action: LoopAction,
    tick_changed: bool,
    last_redraw: std::time::Instant,
    now: std::time::Instant,
) -> LoopAction {
    if matches!(terminal_action, LoopAction::Redraw) || tick_changed {
        return LoopAction::Redraw;
    }
    if saw_settling_agent_event {
        return LoopAction::Redraw;
    }
    if should_defer_streaming_redraw(saw_agent_event, last_redraw, now) {
        LoopAction::Continue
    } else if saw_agent_event {
        LoopAction::Redraw
    } else {
        LoopAction::Continue
    }
}

fn should_defer_streaming_redraw(
    saw_agent_event: bool,
    last_redraw: std::time::Instant,
    now: std::time::Instant,
) -> bool {
    saw_agent_event && now.duration_since(last_redraw) < ANIMATION_FRAME_INTERVAL
}

fn trace_draw_reason(reason: &'static str) {
    tracing::trace!(target: "mush_tui::draw_reason", reason, "draw requested");
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_agent_event(
    pane_id: mush_ai::types::PaneId,
    stream_gen: super::streams::StreamGeneration,
    event: AgentEvent,
    stream_state: &mut super::streams::StreamState,
    runtime: &mut RunnerRuntime,
    tui_config: &mut super::TuiConfig,
    registry: &ApiRegistry,
    image_picker: &Option<ratatui_image::picker::Picker>,
) {
    if !runtime
        .session
        .runtime
        .turns
        .is_current(pane_id, stream_gen)
    {
        return;
    }
    super::control::complete(runtime.session.runtime.control.as_ref(), &event);

    {
        let pane = &mut runtime.session;
        let model = pane
            .runtime
            .turns
            .meta(pane_id)
            .map(|meta| meta.model.clone())
            .unwrap_or_else(|| tui_config.model.clone());
        let (app, conversation, image_protos) = pane.fields_mut();
        let mut ctx = event_handler::EventCtx {
            app,
            conversation,
            image_protos,
        };
        event_handler::handle_agent_event(
            &mut ctx,
            &event,
            &model,
            tui_config.debug_cache,
            image_picker,
        );
    }

    super::streams::handle_agent_event_side_effects(
        &mut runtime.session,
        stream_state,
        pane_id,
        &event,
        tui_config,
        registry,
        runtime.pending_prompt.is_some() || runtime.pending_compactions.contains_key(&pane_id),
    )
    .await;

    if let AgentEvent::ToolExecEnd { tool_name, .. } = &event
        && tool_exec_may_touch_tty(tool_name.as_str())
    {
        super::terminal::reapply_tui_modes_after_tool(tui_config.terminal_policy);
    }
}

fn tool_exec_may_touch_tty(tool_name: &str) -> bool {
    tool_name == "bash"
}

async fn drain_browser_logins(
    session: &mut crate::session::Session,
    tui_config: &TuiConfig,
) -> bool {
    let has_pending = session.app.pending_browser_login.is_some();
    if !has_pending {
        return false;
    }
    let drained = drain_finished_browser_logins(session);
    if drained.saved {
        refresh_models_after_login(session, tui_config).await;
    }
    drained.settled
}

fn drain_session_picker(session: &mut crate::session::Session) -> bool {
    let mut changed = false;
    for pane in std::slice::from_mut(session) {
        if let Some(picker) = pane.app.interaction.session_picker.as_mut() {
            changed |= picker.drain_incoming();
        }
    }
    changed
}

#[allow(clippy::too_many_arguments)]
async fn handle_idle_iteration(
    stream_state: &mut StreamState,
    runtime: &mut RunnerRuntime,
    tui_config: &mut TuiConfig,
    registry: &ApiRegistry,
    image_picker: &Option<ratatui_image::picker::Picker>,
    size_cache: &std::rc::Rc<super::size_cached_backend::CachedSizeState>,
) -> io::Result<LoopAction> {
    stream_state.save_dirty_session(&mut runtime.session, tui_config);
    let picker_changed = drain_session_picker(&mut runtime.session);
    let login_changed = drain_browser_logins(&mut runtime.session, tui_config).await;

    let (session, mut deps) = input_parts(runtime, tui_config, registry, image_picker, size_cache);
    let action = handle_idle_terminal_events(session, stream_state, &mut deps).await?;
    if matches!(action, LoopAction::Continue) && (picker_changed || login_changed) {
        Ok(LoopAction::Redraw)
    } else {
        Ok(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "signal")]
    struct SignalFake(tokio::time::Instant);

    #[cfg(feature = "signal")]
    impl mush_notify::Clock for SignalFake {
        fn now(&self) -> Option<std::time::Duration> {
            Some(std::time::Duration::from_secs(1000) + self.0.elapsed())
        }
    }

    #[cfg(feature = "signal")]
    impl mush_notify::ActivitySource for SignalFake {
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
    impl mush_notify::Transport for SignalFake {
        fn available(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            Box::pin(async { true })
        }

        fn send(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            panic!("dry-run attempted a send")
        }
    }

    #[tokio::test]
    async fn signal_completion_notifications_follow_production_events() {
        use super::super::streams::{StreamMeta, StreamState};
        use mush_ai::types::PaneId;
        use std::sync::{Arc, Mutex};

        for case in [
            "disabled",
            "missing",
            "stale",
            "warm",
            "abort",
            "replaced",
            "cancel",
            "queued",
            "automatic",
            "error",
            "max",
        ] {
            let mut config = super::super::TuiConfig::for_test();
            config.cache_timer = case != "disabled";
            config.auto_fork_compact = false;
            config.generate_title = false;
            config.save_session = None;
            let session = crate::session::Session::new(crate::app::App::new(
                config.model.id.clone(),
                config.model.context_window,
            ));
            let mut runtime = RunnerRuntime::from_pane(session, Default::default());
            if matches!(case, "warm" | "stale") {
                runtime.session.app.cache.refresh();
                if case == "stale" {
                    runtime.session.app.cache.ttl_secs = 1;
                    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
                    assert_eq!(runtime.session.app.cache.remaining_secs(), Some(0));
                }
            }
            if case == "queued" {
                runtime
                    .session
                    .app
                    .push_queued_message("private queued text");
            }
            if case == "automatic" {
                runtime.pending_prompt = Some("private continuation".into());
            }
            let mut state = StreamState::new();
            #[cfg(feature = "signal")]
            {
                let fake = Arc::new(SignalFake(tokio::time::Instant::now()));
                state.signal = Some(
                    mush_notify::Dispatcher::new(
                        mush_notify::Config {
                            mode: mush_notify::Mode::DryRun,
                            account: "+12025550100".into(),
                            recipients: vec!["+12025550101".into()],
                            socket: Some("/run/user/1000/private/socket".into()),
                            grace_seconds: 1,
                            ..Default::default()
                        },
                        fake.clone(),
                        fake.clone(),
                        fake,
                    )
                    .unwrap(),
                );
            }
            let alerts = Arc::new(Mutex::new(Vec::new()));
            let captured = alerts.clone();
            state.notify = Arc::new(move |summary, body, sound| {
                captured
                    .lock()
                    .unwrap()
                    .push((summary.to_owned(), body.to_owned(), sound))
            });
            let pane = PaneId::new(1);
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            let cancel = tokio_util::sync::CancellationToken::new();
            let generation = runtime.session.runtime.turns.register_active(
                pane,
                StreamMeta {
                    steering_queue: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                    confirm_req_rx: rx,
                    confirm_reply: Arc::new(tokio::sync::Mutex::new(None)),
                    model: config.model.clone(),
                    context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    cancel: cancel.clone(),
                },
            );
            if case == "abort" {
                runtime.session.runtime.turns.abort(pane);
            }
            if case == "replaced" {
                let (_tx, rx) = tokio::sync::mpsc::channel(1);
                runtime.session.runtime.turns.register_active(
                    pane,
                    StreamMeta {
                        steering_queue: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                        confirm_req_rx: rx,
                        confirm_reply: Arc::new(tokio::sync::Mutex::new(None)),
                        model: config.model.clone(),
                        context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                        cancel: tokio_util::sync::CancellationToken::new(),
                    },
                );
            }
            if case == "cancel" {
                cancel.cancel();
            }
            let registry = ApiRegistry::new();
            for turn_index in 0..3 {
                dispatch_agent_event(
                    pane,
                    generation,
                    AgentEvent::TurnStart { turn_index },
                    &mut state,
                    &mut runtime,
                    &mut config,
                    &registry,
                    &None,
                )
                .await;
            }
            if case == "error" {
                dispatch_agent_event(
                    pane,
                    generation,
                    AgentEvent::Error {
                        error: "private error".into(),
                    },
                    &mut state,
                    &mut runtime,
                    &mut config,
                    &registry,
                    &None,
                )
                .await;
            }
            if case == "max" {
                dispatch_agent_event(
                    pane,
                    generation,
                    AgentEvent::MaxTurnsReached { max_turns: 1 },
                    &mut state,
                    &mut runtime,
                    &mut config,
                    &registry,
                    &None,
                )
                .await;
            }
            assert!(alerts.lock().unwrap().is_empty());
            #[cfg(feature = "signal")]
            assert_eq!(state.signal.as_mut().unwrap().finished().await, None);
            for _ in 0..2 {
                dispatch_agent_event(
                    pane,
                    generation,
                    AgentEvent::AgentEnd,
                    &mut state,
                    &mut runtime,
                    &mut config,
                    &registry,
                    &None,
                )
                .await;
            }
            let alerts = alerts.lock().unwrap().clone();
            let expected = usize::from(!matches!(
                case,
                "abort" | "replaced" | "cancel" | "queued" | "automatic"
            ));
            assert_eq!(alerts.len(), expected, "{case}");
            if expected == 1 {
                assert_eq!(
                    alerts[0],
                    (
                        "awaiting input".into(),
                        "task stopped and needs your input".into(),
                        Some(crate::notify::Sound::Complete)
                    )
                );
            }
            #[cfg(feature = "signal")]
            {
                assert_eq!(state.signal.as_mut().unwrap().finished().await, None);
                tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
                assert_eq!(
                    state.signal.as_mut().unwrap().finished().await,
                    (expected == 1).then_some(mush_notify::Outcome::WouldNotify)
                );
            }
        }
    }

    #[test]
    fn defers_streaming_redraw_when_agent_events_arrive_inside_frame_budget() {
        let last_redraw = std::time::Instant::now();
        let now = last_redraw + ANIMATION_FRAME_INTERVAL / 2;

        assert!(should_defer_streaming_redraw(true, last_redraw, now));
    }

    #[test]
    fn does_not_defer_streaming_redraw_after_frame_budget() {
        let last_redraw = std::time::Instant::now();
        let now = last_redraw + ANIMATION_FRAME_INTERVAL;

        assert!(!should_defer_streaming_redraw(true, last_redraw, now));
    }

    #[test]
    fn tick_only_iterations_do_not_force_redraws() {
        let last_redraw = std::time::Instant::now();
        let now = last_redraw + ANIMATION_FRAME_INTERVAL / 2;

        assert!(!should_defer_streaming_redraw(false, last_redraw, now));
    }

    #[test]
    fn terminal_redraw_wins_over_streaming_frame_deferral() {
        let last_redraw = std::time::Instant::now();
        let now = last_redraw + ANIMATION_FRAME_INTERVAL / 2;

        let action =
            streaming_iteration_action(true, false, LoopAction::Redraw, false, last_redraw, now);

        assert!(matches!(action, LoopAction::Redraw));
    }

    #[test]
    fn final_agent_event_redraws_even_inside_frame_budget() {
        let last_redraw = std::time::Instant::now();
        let now = last_redraw + ANIMATION_FRAME_INTERVAL / 2;

        let action =
            streaming_iteration_action(true, true, LoopAction::Continue, false, last_redraw, now);

        assert!(
            matches!(action, LoopAction::Redraw),
            "the final agent event must draw immediately so a finished reply is visible without waiting for focus/input"
        );
    }

    #[test]
    fn tick_branch_preserves_terminal_redraws() {
        let last_redraw = std::time::Instant::now();
        let now = last_redraw + ANIMATION_FRAME_INTERVAL / 2;

        let action =
            streaming_iteration_action(false, false, LoopAction::Redraw, false, last_redraw, now);

        assert!(matches!(action, LoopAction::Redraw));
    }

    #[test]
    fn agent_event_drain_is_bounded_per_frame() {
        let started = std::time::Instant::now();

        assert!(should_continue_agent_drain(0, started, started));
        assert!(!should_continue_agent_drain(
            MAX_AGENT_EVENTS_PER_FRAME,
            started,
            started
        ));
        assert!(!should_continue_agent_drain(
            1,
            started,
            started + MAX_AGENT_EVENT_DRAIN
        ));
    }

    #[test]
    fn focused_pane_session_picker_drain_reports_disconnect() {
        let app = crate::app::App::new("test".into(), mush_ai::types::TokenCount::new(200_000));
        let mut session = crate::session::Session::new(app);
        let (tx, rx) = std::sync::mpsc::channel();
        session.app.open_session_picker_streaming(rx, "/tmp".into());
        drop(tx);

        assert!(drain_session_picker(&mut session));
    }
}
