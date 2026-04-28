use std::io;

use futures::StreamExt;
use mush_agent::AgentEvent;
use mush_ai::registry::ApiRegistry;

use crate::event_handler;

use super::TuiConfig;
use super::input::{
    InputDeps, LoopAction, handle_idle_terminal_events, handle_streaming_terminal_events,
};
use super::panes::drain_inboxes;
use super::runtime::{RunnerRuntime, RunnerServices};
use super::streams::{AgentStreams, StreamState, poll_confirmation_prompt, poll_live_tool_output};

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_loop_iteration(
    agent_streams: &mut AgentStreams<'_>,
    stream_state: &mut StreamState,
    runtime: &mut RunnerRuntime,
    services: &RunnerServices,
    tui_config: &mut TuiConfig,
    registry: &ApiRegistry,
    image_picker: &Option<ratatui_image::picker::Picker>,
    size_cache: &std::rc::Rc<super::caching_backend::CachedSizeState>,
) -> io::Result<LoopAction> {
    if agent_streams.is_empty() {
        handle_idle_iteration(
            stream_state,
            runtime,
            services,
            tui_config,
            registry,
            image_picker,
            size_cache,
        )
        .await
    } else {
        handle_streaming_iteration(
            agent_streams,
            stream_state,
            runtime,
            services,
            tui_config,
            registry,
            image_picker,
            size_cache,
        )
        .await
    }
}

fn input_parts<'a>(
    runtime: &'a mut RunnerRuntime,
    services: &'a RunnerServices,
    tui_config: &'a mut TuiConfig,
    registry: &'a ApiRegistry,
    image_picker: &'a Option<ratatui_image::picker::Picker>,
    size_cache: &'a std::rc::Rc<super::caching_backend::CachedSizeState>,
) -> (&'a mut crate::pane::PaneManager, InputDeps<'a>) {
    let RunnerRuntime {
        pane_mgr,
        thinking_prefs,
        thinking_saver,
        lifecycle_hooks,
        cwd,
        pending_prompt,
        pending_compactions,
        ..
    } = runtime;
    (
        pane_mgr,
        InputDeps {
            tui_config,
            thinking_prefs,
            thinking_saver,
            registry,
            message_bus: &services.message_bus,
            file_tracker: &services.file_tracker,
            lifecycle_hooks,
            cwd,
            pending_prompt,
            pending_compactions,
            delegation_queue: &services.delegation_queue,
            image_picker,
            size_cache,
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn handle_streaming_iteration(
    agent_streams: &mut AgentStreams<'_>,
    stream_state: &mut StreamState,
    runtime: &mut RunnerRuntime,
    services: &RunnerServices,
    tui_config: &mut TuiConfig,
    registry: &ApiRegistry,
    image_picker: &Option<ratatui_image::picker::Picker>,
    size_cache: &std::rc::Rc<super::caching_backend::CachedSizeState>,
) -> io::Result<LoopAction> {
    let tick = tokio::time::sleep(std::time::Duration::from_millis(16));
    tokio::pin!(tick);

    // wait for either an agent event or the frame tick
    tokio::select! {
        result = agent_streams.next() => {
            if let Some((pane_id, stream_gen, event)) = result {
                dispatch_agent_event(pane_id, stream_gen, event, stream_state, runtime, services, tui_config, registry, image_picker).await;
            }

            // drain all immediately-available events within this frame
            // so we don't redraw between each one
            loop {
                use futures::FutureExt;
                match agent_streams.next().now_or_never() {
                    Some(Some((pane_id, stream_gen, event))) => {
                        dispatch_agent_event(pane_id, stream_gen, event, stream_state, runtime, services, tui_config, registry, image_picker).await;
                    }
                    _ => break,
                }
            }
        }
        _ = tick => {
            poll_confirmation_prompt(&mut runtime.pane_mgr, stream_state).await;
            poll_live_tool_output(&mut runtime.pane_mgr, &tui_config.tool_output_live);
            drain_session_picker(&mut runtime.pane_mgr);
            drain_inboxes(&mut runtime.pane_mgr, stream_state).await;

            let (pane_mgr, mut deps) = input_parts(runtime, services, tui_config, registry, image_picker, size_cache);
            let action = handle_streaming_terminal_events(pane_mgr, stream_state, &mut deps).await?;
            if matches!(action, LoopAction::Quit) {
                return Ok(LoopAction::Quit);
            }
        }
    }

    Ok(LoopAction::Redraw)
}

/// process a single agent event (extracted for reuse in drain loop)
#[allow(clippy::too_many_arguments)]
async fn dispatch_agent_event(
    pane_id: crate::pane::PaneId,
    stream_gen: super::streams::StreamGeneration,
    event: AgentEvent,
    stream_state: &mut super::streams::StreamState,
    runtime: &mut RunnerRuntime,
    services: &RunnerServices,
    tui_config: &mut super::TuiConfig,
    registry: &ApiRegistry,
    image_picker: &Option<ratatui_image::picker::Picker>,
) {
    // drop events from stale streams (aborted or superseded)
    if !stream_state.is_current(pane_id, stream_gen) {
        return;
    }

    if let Some(pane) = runtime.pane_mgr.pane_mut(pane_id) {
        let model = stream_state
            .meta(pane_id)
            .map(|meta| &meta.model)
            .unwrap_or(&tui_config.model);
        let (app, conversation, image_protos) = pane.fields_mut();
        let mut ctx = event_handler::EventCtx {
            app,
            conversation,
            image_protos,
        };
        event_handler::handle_agent_event(
            &mut ctx,
            &event,
            model,
            tui_config.debug_cache,
            image_picker,
        );
    }

    super::streams::handle_agent_event_side_effects(
        &mut runtime.pane_mgr,
        stream_state,
        pane_id,
        &event,
        &services.file_tracker,
        tui_config,
        registry,
    )
    .await;

    // tools that touch /dev/tty (bash running `stty sane`, `reset`,
    // a pager popping kitty kbd flags, etc) silently strip the modes
    // we enabled at startup. restore them on every tool-end so the
    // next redraw loop has a sane terminal underneath it
    if let AgentEvent::ToolExecEnd { tool_name, .. } = &event
        && tool_exec_may_touch_tty(tool_name.as_str())
    {
        super::terminal::reapply_tui_modes_after_tool(tui_config.terminal_policy);
    }
}

/// tools whose execution can legitimately mutate the real tty state
/// (via `/dev/tty` writes or child processes that take over input).
/// keep this list narrow so we don't thrash the terminal after every
/// tool call; bash is the only current offender worth handling
fn tool_exec_may_touch_tty(tool_name: &str) -> bool {
    tool_name == "bash"
}

/// pull any freshly-loaded session metadata into each pane's picker
/// state. the /sessions slash command kicks off a thread that parses
/// session files off the event loop; the results land here on the next
/// tick so the overlay populates progressively
fn drain_session_picker(pane_mgr: &mut crate::pane::PaneManager) {
    for pane in pane_mgr.panes_mut() {
        if let Some(picker) = pane.app.interaction.session_picker.as_mut() {
            picker.drain_incoming();
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_idle_iteration(
    stream_state: &StreamState,
    runtime: &mut RunnerRuntime,
    services: &RunnerServices,
    tui_config: &mut TuiConfig,
    registry: &ApiRegistry,
    image_picker: &Option<ratatui_image::picker::Picker>,
    size_cache: &std::rc::Rc<super::caching_backend::CachedSizeState>,
) -> io::Result<LoopAction> {
    drain_inboxes(&mut runtime.pane_mgr, stream_state).await;
    drain_session_picker(&mut runtime.pane_mgr);

    let (pane_mgr, mut deps) = input_parts(
        runtime,
        services,
        tui_config,
        registry,
        image_picker,
        size_cache,
    );
    handle_idle_terminal_events(pane_mgr, &mut deps).await
}
