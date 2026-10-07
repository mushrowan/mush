use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mush_ai::types::Message;
use mush_session::inbox::{Consumer, Event};

use crate::session::{PendingPrompt, SessionRuntime};

const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

pub struct OrchestratorInbox {
    consumer: Arc<Consumer>,
    store_dir: PathBuf,
    staged: Option<(Event, Instant)>,
    next_poll: Instant,
    replayed: bool,
}

pub enum Delivered {
    Ready,
    Staged,
    Injected(mush_ai::types::EventMessage),
}

impl OrchestratorInbox {
    pub fn from_env(runtime: &SessionRuntime, cwd: &Path) -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os("MUSH_ORCHESTRATOR_INBOX") else {
            return Ok(None);
        };
        Self::open(Path::new(&path), runtime, cwd).map(Some)
    }

    pub fn open(path: &Path, runtime: &SessionRuntime, cwd: &Path) -> io::Result<Self> {
        if runtime.saver.is_none() {
            return Err(io::Error::other(
                "orchestrator inbox requires session persistence",
            ));
        }
        Ok(Self::with_consumer(
            Consumer::open(path, runtime.session_id.clone(), cwd)?,
            mush_session::SessionStore::default_dir(),
        ))
    }

    #[must_use]
    pub fn with_consumer(consumer: Consumer, store_dir: PathBuf) -> Self {
        Self {
            consumer: Arc::new(consumer),
            store_dir,
            staged: None,
            next_poll: Instant::now(),
            replayed: false,
        }
    }

    pub fn set_store_dir(&mut self, store_dir: PathBuf) {
        self.store_dir = store_dir;
    }

    pub fn age_staged(&mut self, by: Duration) {
        if let Some((_, started)) = &mut self.staged {
            *started -= by;
        }
    }

    pub fn rearm(&mut self) {
        self.replayed = false;
        self.next_poll = Instant::now();
    }

    #[must_use]
    pub fn staged(&self) -> Option<&Event> {
        self.staged.as_ref().map(|(event, _)| event)
    }

    pub fn disconnect(&self, runtime: &mut SessionRuntime) {
        let Some((event, _)) = &self.staged else {
            return;
        };
        let last_is_this_event = match runtime.conversation.context().last() {
            Some(Message::Event(message)) => {
                message.receipts.iter().any(|receipt| {
                    receipt.id.as_str() == format!("orchestrator-event-{}", event.event_id)
                }) && runtime
                    .pending_prompt
                    .as_ref()
                    .is_some_and(|prompt| prompt.is_persisted() && prompt.text == message.text())
            }
            _ => false,
        };
        if last_is_this_event {
            runtime.pending_prompt = None;
        }
    }

    pub async fn poll(
        &mut self,
        runtime: &mut SessionRuntime,
        idle: bool,
    ) -> io::Result<Delivered> {
        if !self.consumer.matches(&runtime.session_id) {
            return Err(io::Error::other("orchestrator inbox session changed"));
        }
        if let Some((event, started)) = &self.staged {
            if self.acknowledge_saved(event.event_id.clone()).await? {
                self.staged = None;
                return Ok(Delivered::Ready);
            }
            if started.elapsed() > CHECKPOINT_TIMEOUT {
                return Err(io::Error::other(
                    "orchestrator event session checkpoint timed out",
                ));
            }
            return Ok(Delivered::Staged);
        }
        if self.next_poll > Instant::now() || !idle {
            return Ok(Delivered::Ready);
        }
        self.next_poll = Instant::now() + POLL_INTERVAL;
        if !self.replayed {
            self.replayed = true;
            if let Some(Message::Event(event)) = runtime.conversation.context().last()
                && event
                    .receipts
                    .iter()
                    .any(|r| r.id.starts_with("orchestrator-event-"))
                && !event.kind.is_summary()
            {
                runtime.pending_prompt = Some(PendingPrompt::persisted(event.text()));
                return Ok(Delivered::Ready);
            }
        }
        let consumer = Arc::clone(&self.consumer);
        let pending = tokio::task::spawn_blocking(move || consumer.pending())
            .await
            .map_err(io::Error::other)??;
        for event in pending {
            if runtime.has_inbox_event(&event.event_id) {
                self.acknowledge_saved(event.event_id.clone()).await?;
                continue;
            }
            let message = runtime.accept_inbox_event(&event)?;
            runtime.save_now();
            self.staged = Some((event, Instant::now()));
            return Ok(Delivered::Injected(message));
        }
        Ok(Delivered::Ready)
    }

    async fn acknowledge_saved(&self, event_id: String) -> io::Result<bool> {
        let consumer = Arc::clone(&self.consumer);
        let store = mush_session::SessionStore::new(self.store_dir.clone());
        tokio::task::spawn_blocking(move || consumer.acknowledge_saved(&event_id, &store))
            .await
            .map_err(io::Error::other)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{AgentConnection, ClientEvent, InProcessConnection, PRIMARY_PANE};
    use crate::session::StreamMeta;
    use crate::testing::echo_connection;
    use mush_agent::AgentEvent;
    use mush_ai::types::SessionId;
    use mush_session::inbox::{Binding, Inbox, TerminalReason, contains};

    fn register(dir: &Path, session_id: &SessionId, cwd: &Path) -> Binding {
        let binding = Binding {
            session_id: session_id.to_string(),
            cwd: cwd.to_owned(),
            workers: vec![cwd.to_owned()],
        };
        Inbox::register(dir, binding.clone()).unwrap();
        binding
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

    struct Fixture {
        dir: tempfile::TempDir,
        inbox_dir: PathBuf,
        connection: InProcessConnection,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let inbox_dir = dir.path().join("inbox");
        let (mut connection, _) = echo_connection();
        connection.runtime.saver = Some(Arc::new(|_| {}));
        Fixture {
            dir,
            inbox_dir,
            connection,
        }
    }

    impl Fixture {
        fn cwd(&self) -> &Path {
            self.dir.path()
        }

        fn open(&self) -> io::Result<OrchestratorInbox> {
            OrchestratorInbox::open(&self.inbox_dir, &self.connection.runtime, self.cwd())
        }

        fn statuses(&mut self) -> Vec<String> {
            let mut statuses = Vec::new();
            while let Some(event) = self.connection.try_next_event() {
                if let ClientEvent::Session(crate::commands::SessionEvent::Status(text)) = event {
                    statuses.push(text);
                }
            }
            statuses
        }
    }

    #[tokio::test]
    async fn a_held_consumer_lock_refuses_a_second_opener() {
        let fixture = fixture();
        register(
            &fixture.inbox_dir,
            &fixture.connection.runtime.session_id,
            fixture.cwd(),
        );
        let owner = fixture.open().unwrap();

        let error = fixture.open().err().unwrap();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(owner);
        assert!(fixture.open().is_ok());
    }

    #[tokio::test]
    async fn a_changed_binding_disables_delivery_and_keeps_the_queue() {
        let mut fixture = fixture();
        let session_id = fixture.connection.runtime.session_id.clone();
        let binding = register(&fixture.inbox_dir, &session_id, fixture.cwd());
        let event = deliver(&fixture.inbox_dir, &binding);
        let inbox = fixture.open().unwrap();
        fixture.connection.attach_inbox(inbox);
        let path = fixture.inbox_dir.join("queue.json");
        let original = std::fs::read(&path).unwrap();
        let mut queue: serde_json::Value = serde_json::from_slice(&original).unwrap();
        queue["binding"]["session_id"] = "different-session".into();
        let changed = serde_json::to_vec(&queue).unwrap();
        std::fs::write(&path, &changed).unwrap();

        assert!(fixture.connection.poll_inbox().await);

        assert!(fixture.connection.inbox().is_none());
        assert!(fixture.connection.runtime.conversation.context().is_empty());
        assert!(fixture.connection.runtime.pending_prompt.is_none());
        let statuses = fixture.statuses();
        assert!(
            statuses[0].starts_with("orchestrator inbox disabled"),
            "{statuses:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), changed);
        std::fs::write(&path, original).unwrap();
        let owner = Consumer::open(&fixture.inbox_dir, session_id, fixture.cwd()).unwrap();
        assert_eq!(owner.pending().unwrap(), vec![event]);
    }

    #[tokio::test]
    async fn a_checkpoint_timeout_leaves_the_event_undispatched() {
        let mut fixture = fixture();
        let session_id = fixture.connection.runtime.session_id.clone();
        let binding = register(&fixture.inbox_dir, &session_id, fixture.cwd());
        let event = deliver(&fixture.inbox_dir, &binding);
        let mut inbox = fixture.open().unwrap();
        inbox.set_store_dir(fixture.cwd().join("sessions"));
        fixture.connection.attach_inbox(inbox);

        assert!(!fixture.connection.poll_inbox().await);
        assert!(fixture.connection.runtime.pending_prompt.is_some());
        assert!(matches!(
            fixture.connection.try_next_event(),
            Some(ClientEvent::MessageInjected(_))
        ));

        fixture
            .connection
            .inbox_mut()
            .unwrap()
            .age_staged(Duration::from_secs(6));
        assert!(fixture.connection.poll_inbox().await);

        assert!(fixture.connection.inbox().is_none());
        assert!(fixture.connection.runtime.pending_prompt.is_none());
        assert!(contains(
            &fixture.connection.runtime.conversation,
            &event.event_id
        ));
        let owner = Consumer::open(&fixture.inbox_dir, session_id, fixture.cwd()).unwrap();
        assert_eq!(owner.pending().unwrap(), vec![event]);
    }

    #[tokio::test]
    async fn a_queued_prompt_blocks_delivery() {
        let mut fixture = fixture();
        let session_id = fixture.connection.runtime.session_id.clone();
        let binding = register(&fixture.inbox_dir, &session_id, fixture.cwd());
        let event = deliver(&fixture.inbox_dir, &binding);
        let inbox = fixture.open().unwrap();
        fixture.connection.attach_inbox(inbox);
        fixture.connection.runtime.pending_prompt = Some(PendingPrompt::text("unsent"));

        assert!(fixture.connection.poll_inbox().await);

        assert_eq!(
            fixture
                .connection
                .runtime
                .pending_prompt
                .as_ref()
                .unwrap()
                .text,
            "unsent"
        );
        assert!(fixture.connection.runtime.conversation.context().is_empty());
        assert_eq!(
            Inbox::open(&fixture.inbox_dir).unwrap().pending().unwrap(),
            vec![event]
        );
    }

    #[tokio::test]
    async fn events_wait_for_the_turn_and_then_run_exactly_once() {
        let mut fixture = fixture();
        let store_dir = fixture.cwd().join("sessions");
        let saved_dir = store_dir.clone();
        let saved_cwd = fixture.cwd().to_owned();
        fixture.connection.runtime.saver = Some(Arc::new(move |snapshot| {
            let mut saved =
                mush_session::Session::new(&snapshot.model_id, &saved_cwd.display().to_string());
            saved.meta.id = snapshot.session_id;
            saved.conversation = snapshot.primary;
            saved.system_prompt = snapshot.system_prompt;
            mush_session::SessionStore::new(saved_dir.clone())
                .save(&saved)
                .unwrap();
        }));
        let session_id = fixture.connection.runtime.session_id.clone();
        let binding = register(&fixture.inbox_dir, &session_id, fixture.cwd());
        let inbox = Inbox::open(&fixture.inbox_dir).unwrap();
        let event = deliver(&fixture.inbox_dir, &binding);
        let consumer =
            Consumer::open(&fixture.inbox_dir, session_id.clone(), fixture.cwd()).unwrap();
        fixture
            .connection
            .attach_inbox(OrchestratorInbox::with_consumer(
                consumer,
                store_dir.clone(),
            ));

        let model = crate::testing::echo_model();
        let (_, rx) = tokio::sync::mpsc::channel(1);
        let generation = fixture.connection.runtime.turns.register_active(
            PRIMARY_PANE,
            StreamMeta {
                steering_queue: fixture.connection.runtime.steering_queue.clone(),
                confirm_req_rx: rx,
                confirm_reply: Arc::new(tokio::sync::Mutex::new(None)),
                model,
                context_tokens: Default::default(),
                cancel: Default::default(),
            },
        );
        fixture
            .connection
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
                        AgentEvent::ToolExecEnd {
                            tool_call_id: "call".into(),
                            tool_name: "read".into(),
                            result: mush_agent::tool::ToolResult::text("result"),
                        },
                        AgentEvent::AgentEnd,
                    ]
                    .map(|event| (PRIMARY_PANE, generation, event)),
                ))
            });
        while let Some((pane, generation, agent_event)) =
            fixture.connection.runtime.turns.next().await
        {
            fixture
                .connection
                .ingest_agent_event(pane, generation, agent_event);
            if !fixture.connection.turn_active() {
                break;
            }
            assert!(fixture.connection.poll_inbox().await);
            assert!(!contains(
                &fixture.connection.runtime.conversation,
                &event.event_id
            ));
            assert_eq!(inbox.pending().unwrap().len(), 1);
        }
        while fixture.connection.try_next_event().is_some() {}
        let before = fixture.connection.runtime.conversation.context_len();

        fixture.connection.runtime.pending_prompt = Some(PendingPrompt::text("continuation"));
        assert!(fixture.connection.poll_inbox().await);
        assert!(!contains(
            &fixture.connection.runtime.conversation,
            &event.event_id
        ));
        fixture.connection.runtime.pending_prompt = None;

        assert!(!fixture.connection.poll_inbox().await);
        assert_eq!(
            fixture.connection.runtime.conversation.context_len(),
            before + 1
        );
        assert!(
            fixture
                .connection
                .runtime
                .pending_prompt
                .as_ref()
                .unwrap()
                .is_persisted()
        );
        assert_eq!(inbox.pending().unwrap(), vec![event.clone()]);
        assert!(fixture.connection.poll_inbox().await);
        assert!(inbox.pending().unwrap().is_empty());
        let saved = mush_session::SessionStore::new(store_dir)
            .load(&session_id)
            .unwrap();
        assert!(contains(&saved.conversation, &event.event_id));

        fixture.connection.runtime.pending_prompt = None;
        fixture.connection.inbox_mut().unwrap().rearm();
        assert!(fixture.connection.poll_inbox().await);
        assert!(
            fixture
                .connection
                .runtime
                .pending_prompt
                .as_ref()
                .unwrap()
                .is_persisted()
        );
        assert_eq!(
            fixture.connection.runtime.conversation.context_len(),
            before + 1
        );

        fixture.connection.runtime.session_id = SessionId::new();
        assert!(fixture.connection.poll_inbox().await);
        assert!(fixture.connection.inbox().is_none());
    }
}
