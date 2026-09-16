use mush_agent::ipc::control::{Control, Outcome};

use super::{
    TuiConfig,
    runtime::RunnerRuntime,
    streams::{AgentStreams, StreamState},
};

pub(super) async fn poll(
    streams: &AgentStreams<'_>,
    state: &StreamState,
    runtime: &mut RunnerRuntime,
    config: &TuiConfig,
) -> std::io::Result<()> {
    let Some(control) = runtime.session.runtime.control.clone() else {
        return Ok(());
    };
    if control.target().session != config.session_id.as_str() {
        control.activate(config.session_id.to_string());
    }
    if !super::inbox::safe_to_consume(streams, state, runtime).await {
        return Ok(());
    }
    control.finish(Outcome::Failed);
    let Some(delivery) = control.take_next(&control.target()) else {
        return Ok(());
    };
    let message = delivery.message().map_err(std::io::Error::other)?;
    let mush_ai::types::Message::Event(event) = &message else {
        return Ok(());
    };
    let text = event.text();
    runtime.session.app.push_event_message(event);
    runtime.session.runtime.conversation.append_message(message);
    runtime.session.app.start_streaming();
    let mut prompt = crate::session::PendingPrompt::text(text);
    prompt.mark_persisted();
    runtime.session.runtime.pending_prompt = Some(prompt);
    Ok(())
}

pub(super) fn complete(control: Option<&Control>, event: &mush_agent::AgentEvent) {
    let Some(control) = control else { return };
    match event {
        mush_agent::AgentEvent::Error { .. } | mush_agent::AgentEvent::MaxTurnsReached { .. } => {
            control.running_failed(true);
        }
        mush_agent::AgentEvent::AgentEnd => control.finish(Outcome::Completed),
        _ => {}
    }
}
