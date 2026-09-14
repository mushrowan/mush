use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mush_session::inbox::{Consumer, Event, append, contains};

use super::{
    TuiConfig,
    runtime::RunnerRuntime,
    streams::{AgentStreams, StreamState},
};

pub(super) struct OrchestratorInbox {
    consumer: Arc<Consumer>,
    store_dir: std::path::PathBuf,
    staged: Option<(Event, Instant)>,
    next_poll: Instant,
    replayed: bool,
}

impl OrchestratorInbox {
    pub(super) fn from_env(config: &TuiConfig) -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os("MUSH_ORCHESTRATOR_INBOX") else {
            return Ok(None);
        };
        if config.save_session.is_none() {
            return Err(io::Error::other(
                "orchestrator inbox requires session persistence",
            ));
        }
        Ok(Some(Self {
            consumer: Arc::new(Consumer::open(
                Path::new(&path),
                config.session_id.clone(),
                &config.cwd,
            )?),
            store_dir: mush_session::SessionStore::default_dir(),
            staged: None,
            next_poll: Instant::now(),
            replayed: false,
        }))
    }

    pub(super) async fn poll(
        &mut self,
        streams: &AgentStreams<'_>,
        state: &StreamState,
        runtime: &mut RunnerRuntime,
        config: &TuiConfig,
    ) -> io::Result<bool> {
        if !self.consumer.matches(&config.session_id) {
            return Err(io::Error::other("orchestrator inbox session changed"));
        }
        if let Some((event, started)) = &self.staged {
            if self.acknowledge_saved(event.event_id.clone()).await? {
                self.staged = None;
                return Ok(true);
            }
            if started.elapsed() > Duration::from_secs(5) {
                return Err(io::Error::other(
                    "orchestrator event session checkpoint timed out",
                ));
            }
            return Ok(false);
        }
        if self.next_poll > Instant::now() || !safe_to_consume(streams, state, runtime).await {
            return Ok(true);
        }
        self.next_poll = Instant::now() + Duration::from_secs(1);
        if !self.replayed {
            self.replayed = true;
            if let Some(mush_ai::types::Message::User(user)) =
                runtime.session.conversation.context().last()
                && user
                    .prompt_id
                    .as_ref()
                    .is_some_and(|id| id.starts_with("orchestrator-event-"))
            {
                let mut prompt = crate::session::PendingPrompt::text(user.text());
                prompt.mark_persisted();
                runtime.session.pending_prompt = Some(prompt);
                return Ok(true);
            }
        }
        let consumer = Arc::clone(&self.consumer);
        let pending = tokio::task::spawn_blocking(move || consumer.pending())
            .await
            .map_err(io::Error::other)??;
        for event in pending {
            if contains(&runtime.session.conversation, &event.event_id) {
                self.acknowledge_saved(event.event_id.clone()).await?;
                continue;
            }
            let text = event.message()?;
            append(&mut runtime.session.conversation, &event)?;
            runtime.session.app.push_user_message(text.clone());
            let mut prompt = crate::session::PendingPrompt::text(text);
            prompt.mark_persisted();
            runtime.session.pending_prompt = Some(prompt);
            if let Some(save) = &config.save_session {
                save(super::streams::build_session_snapshot(
                    &runtime.session,
                    config,
                ));
            }
            self.staged = Some((event, Instant::now()));
            return Ok(false);
        }
        Ok(true)
    }

    async fn acknowledge_saved(&self, event_id: String) -> io::Result<bool> {
        let consumer = Arc::clone(&self.consumer);
        let store = mush_session::SessionStore::new(self.store_dir.clone());
        tokio::task::spawn_blocking(move || consumer.acknowledge_saved(&event_id, &store))
            .await
            .map_err(io::Error::other)?
    }
}

pub(super) async fn safe_to_consume(
    streams: &AgentStreams<'_>,
    state: &StreamState,
    runtime: &RunnerRuntime,
) -> bool {
    streams.is_empty()
        && state.meta(mush_ai::types::PaneId::new(1)).is_none()
        && !runtime.session.app.stream.active
        && runtime.session.app.active_tools.is_empty()
        && runtime.pending_prompt.is_none()
        && runtime.pending_compactions.is_empty()
        && runtime.session.pending_prompt.is_none()
        && runtime.session.pending_compaction_inputs.is_empty()
        && !runtime.session.app.has_queued_messages()
        && runtime.session.steering_queue.lock().await.is_empty()
        && runtime.session.conversation.leaf_id().is_none_or(|leaf| {
            runtime
                .session
                .conversation
                .tree()
                .unanswered_tool_calls(leaf)
                .is_empty()
        })
}

#[cfg(test)]
mod tests {
    use super::super::streams::{StreamMeta, new_agent_streams};
    use super::*;
    use futures::StreamExt;
    use mush_agent::AgentEvent;
    use mush_ai::types::{PaneId, TokenCount};
    use mush_session::inbox::{Binding, Inbox, TerminalReason};

    #[tokio::test]
    async fn production_streams_queue_events_between_tool_calls_and_repeated_agent_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = TuiConfig::for_test();
        config.cwd = dir.path().to_owned();
        let store_dir = dir.path().join("sessions");
        let saved_dir = store_dir.clone();
        let saved_cwd = config.cwd.clone();
        config.save_session = Some(std::sync::Arc::new(move |snapshot| {
            let mut saved =
                mush_session::Session::new(&snapshot.model_id, &saved_cwd.display().to_string());
            saved.meta.id = snapshot.session_id;
            saved.conversation = snapshot.primary;
            saved.system_prompt = snapshot.system_prompt;
            mush_session::SessionStore::new(saved_dir.clone())
                .save(&saved)
                .unwrap();
        }));
        let inbox = Inbox::register(
            &dir.path().join("inbox"),
            Binding {
                session_id: config.session_id.to_string(),
                cwd: dir.path().to_owned(),
                workers: vec![dir.path().to_owned()],
            },
        )
        .unwrap();
        let event = Event {
            event_id: "run.worker.1".into(),
            run_id: "run".into(),
            worker_id: "worker".into(),
            assignment_id: 1,
            round: 1,
            session_id: None,
            reason: TerminalReason::Completed,
            report_path: dir.path().join("private.json"),
        };
        inbox.enqueue(event.clone(), dir.path()).unwrap();
        let consumer = Consumer::open(
            &dir.path().join("inbox"),
            config.session_id.clone(),
            dir.path(),
        )
        .unwrap();
        let mut adapter = OrchestratorInbox {
            consumer: Arc::new(consumer),
            store_dir: store_dir.clone(),
            staged: None,
            next_poll: Instant::now(),
            replayed: false,
        };
        let mut runtime = RunnerRuntime::from_pane(
            crate::session::Session::new(crate::app::App::new(
                "fixture".into(),
                TokenCount::new(200_000),
            )),
            super::super::ThinkingPrefs::default(),
        );
        let mut state = StreamState::new();
        let (_, rx) = tokio::sync::mpsc::channel(1);
        let pane = PaneId::new(1);
        let generation = state.register_active(
            pane,
            StreamMeta {
                steering_queue: runtime.session.steering_queue.clone(),
                confirm_req_rx: rx,
                confirm_reply: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                model: config.model.clone(),
                context_tokens: Default::default(),
                cancel: Default::default(),
            },
        );
        let mut streams = new_agent_streams();
        streams.push(Box::pin(futures::stream::iter(
            [
                AgentEvent::ToolExecStart {
                    tool_call_id: "call".into(),
                    tool_name: "read".into(),
                    args: serde_json::json!({}),
                },
                AgentEvent::AgentEnd,
                AgentEvent::ToolExecEnd {
                    tool_call_id: "call".into(),
                    tool_name: "read".into(),
                    result: mush_agent::tool::ToolResult::text("result"),
                },
                AgentEvent::AgentEnd,
            ]
            .map(|event| (pane, generation, event)),
        )));
        while let Some((_, _, event)) = streams.next().await {
            if matches!(event, AgentEvent::AgentEnd) {
                state.remove(pane);
            }
            assert!(
                adapter
                    .poll(&streams, &state, &mut runtime, &config)
                    .await
                    .unwrap()
            );
            assert!(runtime.session.conversation.is_empty());
            assert_eq!(inbox.pending().unwrap().len(), 1);
        }
        runtime.pending_prompt = Some("automatic continuation".into());
        assert!(
            adapter
                .poll(&streams, &state, &mut runtime, &config)
                .await
                .unwrap()
        );
        assert!(runtime.session.conversation.is_empty());
        runtime.pending_prompt = None;
        runtime
            .session
            .pending_compaction_inputs
            .push(("queued during compaction".into(), Vec::new()));
        assert!(
            adapter
                .poll(&streams, &state, &mut runtime, &config)
                .await
                .unwrap()
        );
        assert!(runtime.session.conversation.is_empty());
        runtime.session.pending_compaction_inputs.clear();
        runtime.session.app.stream.active = true;
        assert!(
            adapter
                .poll(&streams, &state, &mut runtime, &config)
                .await
                .unwrap()
        );
        assert!(runtime.session.conversation.is_empty());
        runtime.session.app.stream.active = false;
        assert!(
            !adapter
                .poll(&streams, &state, &mut runtime, &config)
                .await
                .unwrap()
        );
        assert_eq!(runtime.session.conversation.context_len(), 1);
        assert!(
            runtime
                .session
                .pending_prompt
                .as_ref()
                .unwrap()
                .is_persisted()
        );
        assert_eq!(inbox.pending().unwrap(), vec![event.clone()]);
        assert!(
            adapter
                .poll(&streams, &state, &mut runtime, &config)
                .await
                .unwrap()
        );
        assert!(inbox.pending().unwrap().is_empty());
        let saved = mush_session::SessionStore::new(store_dir)
            .load(&config.session_id)
            .unwrap();
        assert!(contains(&saved.conversation, &event.event_id));
        runtime.session.pending_prompt = None;
        adapter.replayed = false;
        adapter.next_poll = Instant::now();
        assert!(
            adapter
                .poll(&streams, &state, &mut runtime, &config)
                .await
                .unwrap()
        );
        assert!(
            runtime
                .session
                .pending_prompt
                .as_ref()
                .unwrap()
                .is_persisted()
        );
        assert_eq!(runtime.session.conversation.context_len(), 1);
        config.session_id = mush_ai::types::SessionId::new();
        assert!(
            adapter
                .poll(&streams, &state, &mut runtime, &config)
                .await
                .is_err()
        );
    }
}
