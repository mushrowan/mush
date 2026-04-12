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

pub(super) async fn run_loop_iteration(
    agent_streams: &mut AgentStreams<'_>,
    stream_state: &mut StreamState,
    runtime: &mut RunnerRuntime,
    services: &RunnerServices,
    tui_config: &mut TuiConfig,
    registry: &ApiRegistry,
    image_picker: &Option<ratatui_image::picker::Picker>,
) -> io::Result<LoopAction> {
    if agent_streams.is_empty() {
        handle_idle_iteration(runtime, services, tui_config, registry).await
    } else {
        handle_streaming_iteration(
            agent_streams,
            stream_state,
            runtime,
            services,
            tui_config,
            registry,
            image_picker,
        )
        .await
    }
}

fn input_parts<'a>(
    runtime: &'a mut RunnerRuntime,
    services: &'a RunnerServices,
    tui_config: &'a mut TuiConfig,
    registry: &'a ApiRegistry,
) -> (&'a mut crate::pane::PaneManager, InputDeps<'a>) {
    let RunnerRuntime {
        pane_mgr,
        thinking_prefs,
        thinking_saver,
        lifecycle_hooks,
        cwd,
        pending_prompt,
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
        },
    )
}

async fn handle_streaming_iteration(
    agent_streams: &mut AgentStreams<'_>,
    stream_state: &mut StreamState,
    runtime: &mut RunnerRuntime,
    services: &RunnerServices,
    tui_config: &mut TuiConfig,
    registry: &ApiRegistry,
    image_picker: &Option<ratatui_image::picker::Picker>,
) -> io::Result<LoopAction> {
    let tick = tokio::time::sleep(std::time::Duration::from_millis(16));
    tokio::pin!(tick);

    // wait for either an agent event or the frame tick
    tokio::select! {
        result = agent_streams.next() => {
            if let Some((pane_id, event)) = result {
                dispatch_agent_event(pane_id, event, stream_state, runtime, services, tui_config, registry, image_picker).await;
            }

            // drain all immediately-available events within this frame
            // so we don't redraw between each one
            loop {
                use futures::FutureExt;
                match agent_streams.next().now_or_never() {
                    Some(Some((pane_id, event))) => {
                        dispatch_agent_event(pane_id, event, stream_state, runtime, services, tui_config, registry, image_picker).await;
                    }
                    _ => break,
                }
            }
        }
        _ = tick => {
            poll_confirmation_prompt(&mut runtime.pane_mgr, stream_state).await;
            poll_live_tool_output(&mut runtime.pane_mgr, &tui_config.tool_output_live);
            drain_inboxes(&mut runtime.pane_mgr).await;

            let (pane_mgr, mut deps) = input_parts(runtime, services, tui_config, registry);
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
    event: AgentEvent,
    stream_state: &mut super::streams::StreamState,
    runtime: &mut RunnerRuntime,
    services: &RunnerServices,
    tui_config: &mut super::TuiConfig,
    registry: &ApiRegistry,
    image_picker: &Option<ratatui_image::picker::Picker>,
) {
    if stream_state.is_aborted(pane_id) {
        if matches!(event, AgentEvent::AgentEnd) {
            stream_state.finish_aborted(pane_id);
        }
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
}

async fn handle_idle_iteration(
    runtime: &mut RunnerRuntime,
    services: &RunnerServices,
    tui_config: &mut TuiConfig,
    registry: &ApiRegistry,
) -> io::Result<LoopAction> {
    drain_inboxes(&mut runtime.pane_mgr).await;

    let (pane_mgr, mut deps) = input_parts(runtime, services, tui_config, registry);
    handle_idle_terminal_events(pane_mgr, &mut deps).await
}
