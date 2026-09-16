use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(test)]
use mush_session::inbox::contains;
use mush_session::inbox::{Consumer, Event};

use super::{TuiConfig, runtime::RunnerRuntime, streams::StreamState};

pub(super) struct OrchestratorInbox {
    consumer: Arc<Consumer>,
    store_dir: std::path::PathBuf,
    staged: Option<(Event, Instant)>,
    next_poll: Instant,
    replayed: bool,
}

pub(super) fn attach(
    result: io::Result<Option<OrchestratorInbox>>,
    runtime: &mut RunnerRuntime,
) -> Option<OrchestratorInbox> {
    match result {
        Ok(inbox) => inbox,
        Err(error) => {
            report_disabled(runtime, &error);
            None
        }
    }
}

pub(super) async fn poll(
    inbox: &mut Option<OrchestratorInbox>,
    state: &StreamState,
    runtime: &mut RunnerRuntime,
    config: &TuiConfig,
) -> bool {
    let Some(active) = inbox.as_mut() else {
        return true;
    };
    match active.poll(state, runtime, config).await {
        Ok(ready) => ready,
        Err(error) => {
            if let Some((event, _)) = &active.staged
                && let Some(mush_ai::types::Message::Event(message)) =
                    runtime.session.runtime.conversation.context().last()
                && message.receipts.iter().any(|receipt| {
                    receipt.id.as_str() == format!("orchestrator-event-{}", event.event_id)
                })
                && runtime
                    .session
                    .runtime
                    .pending_prompt
                    .as_ref()
                    .is_some_and(|prompt| prompt.is_persisted() && prompt.text == message.text())
            {
                runtime.session.runtime.pending_prompt = None;
            }
            *inbox = None;
            report_disabled(runtime, &error);
            true
        }
    }
}

fn report_disabled(runtime: &mut RunnerRuntime, error: &io::Error) {
    tracing::warn!(%error, "orchestrator inbox disabled");
    runtime.session.app.push_system_message(format!(
        "orchestrator inbox disabled: {error}. restart to reconnect"
    ));
}

impl OrchestratorInbox {
    pub(super) fn from_env(config: &TuiConfig) -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os("MUSH_ORCHESTRATOR_INBOX") else {
            return Ok(None);
        };
        Self::open(Path::new(&path), config).map(Some)
    }

    fn open(path: &Path, config: &TuiConfig) -> io::Result<Self> {
        if config.save_session.is_none() {
            return Err(io::Error::other(
                "orchestrator inbox requires session persistence",
            ));
        }
        Ok(Self {
            consumer: Arc::new(Consumer::open(
                path,
                config.session_id.clone(),
                &config.cwd,
            )?),
            store_dir: mush_session::SessionStore::default_dir(),
            staged: None,
            next_poll: Instant::now(),
            replayed: false,
        })
    }

    pub(super) async fn poll(
        &mut self,
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
        if self.next_poll > Instant::now() || !safe_to_consume(state, runtime).await {
            return Ok(true);
        }
        self.next_poll = Instant::now() + Duration::from_secs(1);
        if !self.replayed {
            self.replayed = true;
            if let Some(mush_ai::types::Message::Event(event)) =
                runtime.session.runtime.conversation.context().last()
                && event
                    .receipts
                    .iter()
                    .any(|r| r.id.starts_with("orchestrator-event-"))
                && event.kind != mush_ai::types::EventKind::Summary
            {
                let mut prompt = crate::session::PendingPrompt::text(event.text());
                prompt.mark_persisted();
                runtime.session.runtime.pending_prompt = Some(prompt);
                return Ok(true);
            }
        }
        let consumer = Arc::clone(&self.consumer);
        let pending = tokio::task::spawn_blocking(move || consumer.pending())
            .await
            .map_err(io::Error::other)??;
        for event in pending {
            if runtime.session.runtime.has_inbox_event(&event.event_id) {
                self.acknowledge_saved(event.event_id.clone()).await?;
                continue;
            }
            let message = runtime.session.runtime.accept_inbox_event(&event)?;
            runtime.session.app.push_event_message(&message);
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

pub(super) async fn safe_to_consume(_state: &StreamState, runtime: &RunnerRuntime) -> bool {
    !runtime.session.runtime.turn_active()
        && runtime
            .session
            .runtime
            .turns
            .meta(mush_ai::types::PaneId::new(1))
            .is_none()
        && !runtime.session.app.stream.active
        && runtime.session.app.input.text.is_empty()
        && runtime.session.app.input.images.is_empty()
        && runtime.session.app.active_tools.is_empty()
        && runtime.pending_prompt.is_none()
        && runtime.pending_compactions.is_empty()
        && runtime.session.runtime.pending_prompt.is_none()
        && runtime.session.runtime.pending_compaction_inputs.is_empty()
        && !runtime.session.app.has_queued_messages()
        && runtime
            .session
            .runtime
            .steering_queue
            .lock()
            .await
            .is_empty()
        && runtime
            .session
            .runtime
            .conversation
            .leaf_id()
            .is_none_or(|leaf| {
                runtime
                    .session
                    .runtime
                    .conversation
                    .tree()
                    .unanswered_tool_calls(leaf)
                    .is_empty()
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_agent::AgentEvent;
    use mush_ai::types::{PaneId, TokenCount};
    use mush_runtime::StreamMeta;
    use mush_session::inbox::{Binding, Inbox, TerminalReason};

    fn register(dir: &Path, config: &TuiConfig) -> io::Result<Binding> {
        let binding = Binding {
            session_id: config.session_id.to_string(),
            cwd: config.cwd.clone(),
            workers: vec![config.cwd.clone()],
        };
        Inbox::register(dir, binding.clone())?;
        Ok(binding)
    }

    fn deliver(dir: &Path, binding: &Binding) -> Event {
        let event = Event {
            event_id: "run.worker.1".into(),
            run_id: "run".into(),
            worker_id: "worker".into(),
            assignment_id: 1,
            round: 1,
            session_id: None,
            reason: TerminalReason::Completed,
            report_path: binding.cwd.join("report.json"),
            review: None,
        };
        Inbox::open(dir)
            .unwrap()
            .enqueue(event.clone(), &binding.cwd)
            .unwrap();
        event
    }

    fn runtime() -> RunnerRuntime {
        RunnerRuntime::from_pane(
            crate::session::Session::new(crate::app::App::new(
                "fixture".into(),
                TokenCount::new(200_000),
            )),
            super::super::ThinkingPrefs::default(),
        )
    }

    #[tokio::test]
    async fn held_consumer_lock_keeps_interactive_session_open() {
        let dir = tempfile::tempdir().unwrap();
        let inbox_dir = dir.path().join("inbox");
        let mut config = TuiConfig::for_test();
        config.cwd = dir.path().to_owned();
        config.save_session = Some(Arc::new(|_| {}));
        register(&inbox_dir, &config).unwrap();
        let owner = OrchestratorInbox::open(&inbox_dir, &config).unwrap();
        let mut runtime = runtime();
        runtime.session.runtime.pending_prompt =
            Some(crate::session::PendingPrompt::text("continue"));
        let result = OrchestratorInbox::open(&inbox_dir, &config);
        assert_eq!(
            result.as_ref().err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        let mut inbox = attach(result.map(Some), &mut runtime);
        assert!(inbox.is_none());
        assert!(poll(&mut inbox, &StreamState::new(), &mut runtime, &config,).await);
        assert_eq!(
            runtime
                .session
                .runtime
                .pending_prompt
                .as_ref()
                .unwrap()
                .text,
            "continue"
        );
        assert_eq!(runtime.session.app.messages.len(), 1);
        assert!(!runtime.session.app.should_quit);
        drop(owner);
        assert!(OrchestratorInbox::open(&inbox_dir, &config).is_ok());
    }

    #[tokio::test]
    async fn changed_binding_disables_delivery_and_keeps_queue() {
        let dir = tempfile::tempdir().unwrap();
        let inbox_dir = dir.path().join("inbox");
        let mut config = TuiConfig::for_test();
        config.cwd = dir.path().to_owned();
        config.save_session = Some(Arc::new(|_| {}));
        let binding = register(&inbox_dir, &config).unwrap();
        let event = deliver(&inbox_dir, &binding);
        let mut inbox = Some(OrchestratorInbox::open(&inbox_dir, &config).unwrap());
        let mut runtime = runtime();
        let path = inbox_dir.join("queue.json");
        let original = std::fs::read(&path).unwrap();
        let mut queue: serde_json::Value = serde_json::from_slice(&original).unwrap();
        queue["binding"]["session_id"] = "different-session".into();
        let changed = serde_json::to_vec(&queue).unwrap();
        std::fs::write(&path, &changed).unwrap();
        assert!(poll(&mut inbox, &StreamState::new(), &mut runtime, &config,).await);
        assert!(inbox.is_none());
        assert!(runtime.session.runtime.conversation.context().is_empty());
        assert!(runtime.session.runtime.pending_prompt.is_none());
        assert_eq!(runtime.session.app.messages.len(), 1);
        assert!(!runtime.session.app.should_quit);
        assert_eq!(std::fs::read(&path).unwrap(), changed);
        std::fs::write(&path, original).unwrap();
        let owner = Consumer::open(&inbox_dir, config.session_id.clone(), &config.cwd).unwrap();
        assert_eq!(owner.pending().unwrap(), vec![event]);
    }

    #[tokio::test]
    async fn checkpoint_timeout_does_not_dispatch_unacknowledged_event() {
        let dir = tempfile::tempdir().unwrap();
        let inbox_dir = dir.path().join("inbox");
        let mut config = TuiConfig::for_test();
        config.cwd = dir.path().to_owned();
        config.save_session = Some(Arc::new(|_| {}));
        let binding = register(&inbox_dir, &config).unwrap();
        let event = deliver(&inbox_dir, &binding);
        let mut active = OrchestratorInbox::open(&inbox_dir, &config).unwrap();
        active.store_dir = dir.path().join("sessions");
        let mut inbox = Some(active);
        let mut runtime = runtime();
        let state = StreamState::new();
        assert!(!poll(&mut inbox, &state, &mut runtime, &config).await);
        assert!(runtime.session.runtime.pending_prompt.is_some());
        inbox.as_mut().unwrap().staged.as_mut().unwrap().1 =
            Instant::now() - Duration::from_secs(6);
        assert!(poll(&mut inbox, &state, &mut runtime, &config).await);
        assert!(inbox.is_none());
        assert!(runtime.session.runtime.pending_prompt.is_none());
        assert!(contains(
            &runtime.session.runtime.conversation,
            &event.event_id
        ));
        let owner = Consumer::open(&inbox_dir, config.session_id.clone(), &config.cwd).unwrap();
        assert_eq!(owner.pending().unwrap(), vec![event]);
    }

    #[tokio::test]
    async fn draft_blocks_delivery_and_stays_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let inbox_dir = dir.path().join("inbox");
        let mut config = TuiConfig::for_test();
        config.cwd = dir.path().to_owned();
        config.save_session = Some(Arc::new(|_| {}));
        let binding = register(&inbox_dir, &config).unwrap();
        let event = deliver(&inbox_dir, &binding);
        let mut inbox = Some(OrchestratorInbox::open(&inbox_dir, &config).unwrap());
        let mut runtime = runtime();
        runtime.session.app.input.text = "unsent draft".into();
        runtime.session.app.input.cursor = 4;
        assert!(poll(&mut inbox, &StreamState::new(), &mut runtime, &config).await);
        assert_eq!(runtime.session.app.input.text, "unsent draft");
        assert_eq!(runtime.session.app.input.cursor, 4);
        assert!(runtime.session.runtime.pending_prompt.is_none());
        assert!(runtime.session.runtime.conversation.context().is_empty());
        assert_eq!(
            Inbox::open(&inbox_dir).unwrap().pending().unwrap(),
            vec![event]
        );
    }

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
            review: None,
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
        runtime.session.runtime.session_id = config.session_id.clone();
        let state = StreamState::new();
        let (_, rx) = tokio::sync::mpsc::channel(1);
        let pane = PaneId::new(1);
        let generation = runtime.session.runtime.turns.register_active(
            pane,
            StreamMeta {
                steering_queue: runtime.session.runtime.steering_queue.clone(),
                confirm_req_rx: rx,
                confirm_reply: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
                model: config.model.clone(),
                context_tokens: Default::default(),
                cancel: Default::default(),
            },
        );
        runtime
            .session
            .runtime
            .turns
            .spawn(mush_ai::registry::ApiRegistry::new(), move |_| {
                Box::pin(futures::stream::iter(
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
                ))
            });
        while let Some((_, _, event)) = runtime.session.runtime.turns.next().await {
            if matches!(event, AgentEvent::AgentEnd) {
                runtime.session.runtime.turns.remove(pane);
            }
            if !runtime.session.runtime.turn_active() {
                break;
            }
            assert!(adapter.poll(&state, &mut runtime, &config).await.unwrap());
            assert!(runtime.session.runtime.conversation.is_empty());
            assert_eq!(inbox.pending().unwrap().len(), 1);
        }
        runtime.pending_prompt = Some("automatic continuation".into());
        assert!(adapter.poll(&state, &mut runtime, &config).await.unwrap());
        assert!(runtime.session.runtime.conversation.is_empty());
        runtime.pending_prompt = None;
        runtime
            .session
            .runtime
            .pending_compaction_inputs
            .push(("queued during compaction".into(), Vec::new()));
        assert!(adapter.poll(&state, &mut runtime, &config).await.unwrap());
        assert!(runtime.session.runtime.conversation.is_empty());
        runtime.session.runtime.pending_compaction_inputs.clear();
        runtime.session.app.stream.active = true;
        assert!(adapter.poll(&state, &mut runtime, &config).await.unwrap());
        assert!(runtime.session.runtime.conversation.is_empty());
        runtime.session.app.stream.active = false;
        assert!(!adapter.poll(&state, &mut runtime, &config).await.unwrap());
        assert_eq!(runtime.session.runtime.conversation.context_len(), 1);
        assert!(
            runtime
                .session
                .runtime
                .pending_prompt
                .as_ref()
                .unwrap()
                .is_persisted()
        );
        assert_eq!(inbox.pending().unwrap(), vec![event.clone()]);
        assert!(adapter.poll(&state, &mut runtime, &config).await.unwrap());
        assert!(inbox.pending().unwrap().is_empty());
        let saved = mush_session::SessionStore::new(store_dir)
            .load(&config.session_id)
            .unwrap();
        assert!(contains(&saved.conversation, &event.event_id));
        runtime.session.runtime.pending_prompt = None;
        adapter.replayed = false;
        adapter.next_poll = Instant::now();
        assert!(adapter.poll(&state, &mut runtime, &config).await.unwrap());
        assert!(
            runtime
                .session
                .runtime
                .pending_prompt
                .as_ref()
                .unwrap()
                .is_persisted()
        );
        assert_eq!(runtime.session.runtime.conversation.context_len(), 1);
        config.session_id = mush_ai::types::SessionId::new();
        assert!(adapter.poll(&state, &mut runtime, &config).await.is_err());
    }
}
