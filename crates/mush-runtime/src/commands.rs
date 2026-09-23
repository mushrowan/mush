use std::sync::Arc;

use mush_ai::types::{Message, SessionId};
use mush_kernel::KernelState;
use mush_session::{ConversationState, SessionStore};

use crate::session::SessionRuntime;
use crate::snapshot::SessionSnapshot;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionCommand {
    New,
    Resume {
        id: SessionId,
    },
    Fork {
        before_user_message: usize,
    },
    Clone,
    Branch {
        user_message: usize,
    },
    NavigateTree {
        entry_id: mush_session::tree::EntryId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    Undo,
    Clear,
    Name {
        title: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Replaced {
    New,
    Resumed,
    Forked,
    Cloned,
    Branched,
    Undone,
    Cleared,
    Resynced,
}

impl Replaced {
    #[must_use]
    pub fn keeps_live_stats(self) -> bool {
        matches!(
            self,
            Self::Branched | Self::Undone | Self::Cleared | Self::Resynced
        )
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionIdChanged(SessionId),
    SystemPromptChanged(Option<String>),
    Title(Option<String>),
    TranscriptReplaced {
        reason: Replaced,
        messages: Vec<Message>,
        editor_text: Option<String>,
    },
    Status(String),
    Failed(String),
}

impl SessionRuntime {
    pub async fn handle(&mut self, command: SessionCommand) -> Vec<SessionEvent> {
        let events = self.dispatch(command).await;
        if events
            .iter()
            .any(|event| matches!(event, SessionEvent::TranscriptReplaced { .. }))
        {
            self.refresh_context_tokens();
        }
        events
    }

    async fn dispatch(&mut self, command: SessionCommand) -> Vec<SessionEvent> {
        match command {
            SessionCommand::New => {
                self.save_now();
                self.snapshot_kernel(&self.session_id.clone()).await;
                if let Some(kernel) = &self.kernel_state {
                    kernel.reset().await;
                }
                let id = SessionId::new();
                let lease = self.next_lease(&id);
                self.replace_session(id, ConversationState::new(), None, lease);
                let mut events = self.session_changed_events();
                events.push(SessionEvent::TranscriptReplaced {
                    reason: Replaced::New,
                    messages: Vec::new(),
                    editor_text: None,
                });
                events.push(SessionEvent::Status(
                    "new session started (previous session saved)".into(),
                ));
                events
            }
            SessionCommand::Resume { id } => {
                let store = SessionStore::new(SessionStore::default_dir());
                let lease = if self.lease.is_some() {
                    match store.acquire(&id) {
                        Ok(lease) => Some(lease),
                        Err(error) => return vec![SessionEvent::Failed(error.to_string())],
                    }
                } else {
                    None
                };
                let loaded = match store.load(&id) {
                    Ok(session) => session,
                    Err(error) => {
                        return vec![SessionEvent::Failed(format!(
                            "failed to load session: {error}"
                        ))];
                    }
                };
                self.save_now();
                let previous = self.session_id.clone();
                self.snapshot_kernel(&previous).await;
                if let Some(kernel) = &self.kernel_state {
                    kernel.reset().await;
                }
                if let Some(prompt) = loaded.system_prompt {
                    self.system_prompt = Some(prompt);
                }
                let title = loaded.meta.title.clone();
                self.replace_session(id, loaded.conversation, title.clone(), lease);
                self.begin_kernel_restore();
                self.title_generated = title.is_some();
                let mut events = self.session_changed_events();
                events.push(SessionEvent::SystemPromptChanged(
                    self.system_prompt.clone(),
                ));
                events.push(SessionEvent::TranscriptReplaced {
                    reason: Replaced::Resumed,
                    messages: self.conversation.context(),
                    editor_text: None,
                });
                events.push(SessionEvent::Status(format!(
                    "resumed: {}",
                    title.as_deref().unwrap_or("untitled")
                )));
                events
            }
            SessionCommand::Fork {
                before_user_message,
            } => {
                let messages = self.conversation.context();
                let positions = user_positions(&messages);
                let Some(&position) = positions.get(before_user_message.wrapping_sub(1)) else {
                    return vec![SessionEvent::Failed(format!(
                        "invalid: use 1-{} (try /tree to see messages)",
                        positions.len()
                    ))];
                };
                let prompt = match &messages[position] {
                    Message::User(user) => user.text(),
                    _ => String::new(),
                };
                let prefix = messages[..position].to_vec();
                self.save_now();
                self.snapshot_kernel(&self.session_id.clone()).await;
                let id = SessionId::new();
                let lease = self.next_lease(&id);
                self.replace_session(id, ConversationState::from_messages(prefix), None, lease);
                let mut events = self.session_changed_events();
                events.push(SessionEvent::TranscriptReplaced {
                    reason: Replaced::Forked,
                    messages: self.conversation.context(),
                    editor_text: Some(prompt),
                });
                events.push(SessionEvent::Status(format!(
                    "forked before message {before_user_message}"
                )));
                events
            }
            SessionCommand::Clone => {
                let branch = self.conversation.context();
                self.save_now();
                self.snapshot_kernel(&self.session_id.clone()).await;
                let id = SessionId::new();
                let lease = self.next_lease(&id);
                self.replace_session(id, ConversationState::from_messages(branch), None, lease);
                let mut events = self.session_changed_events();
                events.push(SessionEvent::TranscriptReplaced {
                    reason: Replaced::Cloned,
                    messages: self.conversation.context(),
                    editor_text: None,
                });
                events.push(SessionEvent::Status(
                    "cloned the current branch into a new session".into(),
                ));
                events
            }
            SessionCommand::Branch { user_message } => {
                let user_messages = self.conversation.user_messages_in_branch();
                let count = user_messages.len();
                let target = user_messages
                    .get(user_message.wrapping_sub(1))
                    .map(|entry| (entry.parent_id.clone(), entry_preview(entry)));
                drop(user_messages);
                let Some((parent, preview)) = target else {
                    return vec![SessionEvent::Failed(format!(
                        "invalid: use 1-{count} (try /tree to see messages)"
                    ))];
                };
                match parent {
                    Some(parent) => {
                        self.conversation.branch(&parent);
                    }
                    None => self.conversation.reset_leaf(),
                }
                vec![
                    SessionEvent::TranscriptReplaced {
                        reason: Replaced::Branched,
                        messages: self.conversation.context(),
                        editor_text: None,
                    },
                    SessionEvent::Status(format!("branched before: {preview}")),
                ]
            }
            SessionCommand::NavigateTree { entry_id, summary } => {
                self.navigate_tree(&entry_id, summary)
            }
            SessionCommand::Undo => {
                let parent = self
                    .conversation
                    .user_messages_in_branch()
                    .last()
                    .map(|entry| entry.parent_id.clone());
                match parent {
                    None => vec![SessionEvent::Failed("nothing to undo".into())],
                    Some(parent) => {
                        match parent {
                            Some(parent) => {
                                self.conversation.branch(&parent);
                            }
                            None => self.conversation.reset_leaf(),
                        }
                        vec![
                            SessionEvent::TranscriptReplaced {
                                reason: Replaced::Undone,
                                messages: self.conversation.context(),
                                editor_text: None,
                            },
                            SessionEvent::Status("undid last turn".into()),
                        ]
                    }
                }
            }
            SessionCommand::Clear => {
                self.conversation.replace_messages(Vec::new());
                vec![
                    SessionEvent::TranscriptReplaced {
                        reason: Replaced::Cleared,
                        messages: Vec::new(),
                        editor_text: None,
                    },
                    SessionEvent::Status("conversation cleared".into()),
                ]
            }
            SessionCommand::Name { title } => {
                self.title = title.clone();
                self.title_generated = title.is_some();
                let mut events = vec![SessionEvent::Title(title.clone())];
                if let Some(title) = title {
                    events.push(SessionEvent::Status(format!("session named: {title}")));
                }
                events
            }
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            session_id: self.session_id.clone(),
            title: self.title.clone(),
            primary: self.conversation.clone(),
            model_id: self.model_id.clone(),
            panes: Vec::new(),
            system_prompt: self.system_prompt.clone(),
            parent: self.parent.clone(),
            subagent_model: self.subagent_model.clone(),
            paused: self.paused.clone(),
        }
    }

    pub fn save_now(&self) {
        if let Some(saver) = &self.saver {
            saver(self.snapshot());
        }
    }

    pub fn save_now_and_settle(&mut self) {
        self.save_now();
        self.saved_just_now();
    }

    pub async fn snapshot_kernel(&self, session_id: &SessionId) {
        let Some(kernel) = &self.kernel_state else {
            return;
        };
        let path = SessionStore::kernel_state_path(session_id);
        match kernel.snapshot(&path).await {
            Ok(Some(report)) => tracing::info!(
                saved = report.saved.len(),
                skipped = report.skipped.len(),
                "kernel state saved"
            ),
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "kernel state snapshot failed"),
        }
    }

    pub fn refresh_context_tokens(&mut self) {
        let estimate = mush_session::compact::estimate_tokens(&self.conversation.context());
        self.context_tokens = mush_ai::types::TokenCount::new(estimate as u64);
    }

    pub fn set_kernel_state(&mut self, kernel: Option<Arc<dyn KernelState>>) {
        self.kernel_state = kernel;
        self.attach_kernel_session();
    }

    pub fn set_rlm_max_depth(&mut self, max_depth: u32) {
        self.rlm_max_depth = max_depth;
        self.attach_kernel_session();
    }

    #[must_use]
    pub fn kernel_session(&self) -> mush_kernel::KernelSession {
        mush_kernel::KernelSession {
            session_dir: SessionStore::default_artifact_dir(&self.session_id),
            state_path: SessionStore::kernel_state_path(&self.session_id),
            depth: self.rlm_depth,
            max_depth: self.rlm_max_depth,
        }
    }

    fn attach_kernel_session(&self) {
        if let Some(kernel) = &self.kernel_state {
            kernel.attach_session(self.kernel_session());
        }
    }

    fn next_lease(&self, id: &SessionId) -> Option<mush_session::SessionLease> {
        self.lease.as_ref()?;
        SessionStore::new(SessionStore::default_dir())
            .acquire(id)
            .ok()
    }

    fn replace_session(
        &mut self,
        id: SessionId,
        conversation: ConversationState,
        title: Option<String>,
        lease: Option<mush_session::SessionLease>,
    ) {
        self.lease = lease;
        self.session_id = id;
        self.conversation = conversation;
        self.title = title;
        self.title_generated = false;
        self.pending_prompt = None;
        self.kernel_restore = None;
        self.attach_kernel_session();
    }

    fn session_changed_events(&self) -> Vec<SessionEvent> {
        vec![
            SessionEvent::SessionIdChanged(self.session_id.clone()),
            SessionEvent::Title(self.title.clone()),
        ]
    }

    pub fn navigate_tree(
        &mut self,
        entry_id: &mush_session::tree::EntryId,
        summary: Option<String>,
    ) -> Vec<SessionEvent> {
        let tree = self.conversation.tree();
        let Some(target) = tree.by_id(entry_id) else {
            return vec![SessionEvent::Failed(
                "that message is no longer in the tree".into(),
            )];
        };
        let preview = entry_preview(target);
        let editor_text = match &target.kind {
            mush_session::tree::EntryKind::Message {
                message: Message::User(user),
            } => Some(user.text()),
            _ => None,
        };
        let new_leaf = navigation_leaf(tree, target);
        let status = match (new_leaf, summary) {
            (Some(leaf), Some(summary)) => {
                self.conversation.branch_with_summary(&leaf, summary);
                format!("branched before: {preview}, with a summary of the branch left behind")
            }
            (Some(leaf), None) => {
                self.conversation.branch(&leaf);
                format!("branched before: {preview}")
            }
            (None, Some(summary)) => {
                self.conversation.reset_leaf_with_summary(summary);
                format!("branched before: {preview}, with a summary of the branch left behind")
            }
            (None, None) => {
                self.conversation.reset_leaf();
                format!("branched before: {preview}")
            }
        };
        vec![
            SessionEvent::TranscriptReplaced {
                reason: Replaced::Branched,
                messages: self.conversation.context(),
                editor_text,
            },
            SessionEvent::Status(status),
        ]
    }
}

#[must_use]
pub fn navigation_leaf(
    tree: &mush_session::tree::SessionTree,
    target: &mush_session::tree::SessionEntry,
) -> Option<mush_session::tree::EntryId> {
    match &target.kind {
        mush_session::tree::EntryKind::Message {
            message: Message::User(_),
        } => target.parent_id.clone(),
        _ if tree.unanswered_tool_calls(&target.id).is_empty() => Some(target.id.clone()),
        _ => tree.nearest_branchable_ancestor(&target.id).cloned(),
    }
}

fn user_positions(messages: &[Message]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| matches!(message, Message::User(_)))
        .map(|(position, _)| position)
        .collect()
}

fn entry_preview(entry: &mush_session::tree::SessionEntry) -> String {
    match &entry.kind {
        mush_session::tree::EntryKind::Message {
            message: Message::User(user),
        } => {
            let text = user.text();
            let mut chars = text.chars();
            let head: String = chars.by_ref().take(40).collect();
            if chars.next().is_some() {
                format!("{head}...")
            } else {
                head
            }
        }
        _ => "message".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{Timestamp, UserContent, UserMessage};

    struct RecordingKernel(std::sync::Mutex<Vec<mush_kernel::KernelSession>>);

    #[async_trait::async_trait]
    impl KernelState for RecordingKernel {
        async fn snapshot(
            &self,
            _path: &std::path::Path,
        ) -> Result<Option<mush_kernel::SnapshotReport>, String> {
            Ok(None)
        }
        async fn restore_from(
            &self,
            _path: &std::path::Path,
        ) -> Option<Result<mush_kernel::RestoreReport, String>> {
            None
        }
        async fn reset(&self) {}
        fn attach_session(&self, session: mush_kernel::KernelSession) {
            self.0.lock().unwrap().push(session);
        }
    }

    #[tokio::test]
    async fn the_kernel_learns_the_session_dir_and_depth_on_every_switch() {
        let kernel = Arc::new(RecordingKernel(std::sync::Mutex::new(Vec::new())));
        let mut runtime = SessionRuntime::new(ConversationState::new());
        runtime.set_kernel_state(Some(kernel.clone()));
        runtime.set_rlm_max_depth(4);
        runtime.handle(SessionCommand::New).await;

        let seen = kernel.0.lock().unwrap().clone();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[1].max_depth, 4);
        assert_eq!(seen[2].depth, 0);
        assert_eq!(
            seen[2].session_dir,
            SessionStore::default_artifact_dir(&runtime.session_id)
        );
        assert_ne!(seen[0].session_dir, seen[2].session_dir);
        let env = seen[2].env();
        assert!(env.iter().any(|(k, v)| k == "RLM_MAX_DEPTH" && v == "4"));
        assert!(env.iter().any(|(k, v)| k == "RLM_DEPTH" && v == "0"));
    }

    fn user(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    fn assistant(text: &str) -> Message {
        Message::Assistant(
            mush_ai::types::AssistantMessage::builder()
                .content(vec![mush_ai::types::AssistantContentPart::Text(
                    mush_ai::types::TextContent { text: text.into() },
                )])
                .model("m".into())
                .provider(mush_ai::types::Provider::Anthropic)
                .api(mush_ai::types::Api::AnthropicMessages)
                .stop_reason(mush_ai::types::StopReason::Stop)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    fn runtime(messages: Vec<Message>) -> SessionRuntime {
        SessionRuntime::new(ConversationState::from_messages(messages))
    }

    fn replaced(events: &[SessionEvent]) -> Option<(Replaced, Vec<Message>, Option<String>)> {
        events.iter().find_map(|event| match event {
            SessionEvent::TranscriptReplaced {
                reason,
                messages,
                editor_text,
            } => Some((*reason, messages.clone(), editor_text.clone())),
            _ => None,
        })
    }

    #[tokio::test]
    async fn new_swaps_the_id_and_empties_the_transcript() {
        let mut runtime = runtime(vec![user("first")]);
        runtime.title = Some("old".into());
        let old = runtime.session_id.clone();
        let saved = Arc::new(std::sync::Mutex::new(Vec::new()));
        runtime.saver = Some({
            let saved = saved.clone();
            Arc::new(move |snapshot: SessionSnapshot| {
                saved.lock().unwrap().push(snapshot.session_id.clone());
            })
        });

        let events = runtime.handle(SessionCommand::New).await;

        assert_ne!(runtime.session_id, old);
        assert!(runtime.conversation.is_empty());
        assert!(runtime.title.is_none());
        assert_eq!(saved.lock().unwrap().as_slice(), [old]);
        assert!(events.contains(&SessionEvent::Title(None)));
        assert_eq!(replaced(&events), Some((Replaced::New, Vec::new(), None)));
    }

    #[tokio::test]
    async fn fork_keeps_the_prefix_and_hands_back_the_prompt() {
        let mut runtime = runtime(vec![user("first"), user("second"), user("third")]);
        let old = runtime.session_id.clone();

        let events = runtime
            .handle(SessionCommand::Fork {
                before_user_message: 2,
            })
            .await;

        assert_ne!(runtime.session_id, old);
        assert_eq!(
            replaced(&events),
            Some((Replaced::Forked, vec![user("first")], Some("second".into())))
        );
    }

    #[tokio::test]
    async fn fork_out_of_range_fails_without_touching_anything() {
        let mut runtime = runtime(vec![user("first")]);
        let old = runtime.session_id.clone();

        let events = runtime
            .handle(SessionCommand::Fork {
                before_user_message: 5,
            })
            .await;

        assert_eq!(runtime.session_id, old);
        assert!(matches!(events.as_slice(), [SessionEvent::Failed(_)]));
    }

    #[tokio::test]
    async fn clone_copies_the_branch_under_a_new_id() {
        let mut runtime = runtime(vec![user("a"), user("b")]);
        let old = runtime.session_id.clone();

        let events = runtime.handle(SessionCommand::Clone).await;

        assert_ne!(runtime.session_id, old);
        assert_eq!(
            replaced(&events).map(|(reason, messages, _)| (reason, messages.len())),
            Some((Replaced::Cloned, 2))
        );
    }

    #[tokio::test]
    async fn undo_drops_the_last_turn_and_branch_rewinds_to_a_message() {
        let mut runtime = runtime(vec![user("a"), user("b"), user("c")]);

        let undone = runtime.handle(SessionCommand::Undo).await;
        assert_eq!(runtime.conversation.context_len(), 2);
        assert!(matches!(
            replaced(&undone),
            Some((Replaced::Undone, _, None))
        ));

        let branched = runtime
            .handle(SessionCommand::Branch { user_message: 1 })
            .await;
        assert_eq!(runtime.conversation.context_len(), 0);
        assert!(matches!(
            replaced(&branched),
            Some((Replaced::Branched, _, None))
        ));

        let nothing = runtime.handle(SessionCommand::Undo).await;
        assert!(matches!(nothing.as_slice(), [SessionEvent::Failed(_)]));
    }

    #[tokio::test]
    async fn navigating_to_a_user_message_rewinds_before_it_and_hands_back_its_text() {
        let mut runtime = runtime(vec![user("a"), user("b"), user("c")]);
        let target = runtime.conversation.tree().entries()[1].id.clone();

        let events = runtime
            .handle(SessionCommand::NavigateTree {
                entry_id: target,
                summary: None,
            })
            .await;

        assert_eq!(runtime.conversation.context(), vec![user("a")]);
        assert!(matches!(
            replaced(&events),
            Some((Replaced::Branched, _, Some(text))) if text == "b"
        ));
    }

    #[tokio::test]
    async fn navigating_to_an_assistant_message_keeps_it_as_the_leaf() {
        let mut runtime = runtime(vec![user("a"), assistant("reply"), user("b")]);
        let target = runtime.conversation.tree().entries()[1].id.clone();

        let events = runtime
            .handle(SessionCommand::NavigateTree {
                entry_id: target,
                summary: None,
            })
            .await;

        assert_eq!(runtime.conversation.context_len(), 2);
        assert!(matches!(
            replaced(&events),
            Some((Replaced::Branched, _, None))
        ));
    }

    #[tokio::test]
    async fn navigating_with_a_summary_leaves_it_on_the_new_branch() {
        let mut runtime = runtime(vec![user("a"), assistant("reply"), user("b")]);
        let target = runtime.conversation.tree().entries()[0].id.clone();

        runtime
            .handle(SessionCommand::NavigateTree {
                entry_id: target,
                summary: Some("we tried b".into()),
            })
            .await;

        let context = runtime.conversation.context();
        assert_eq!(context.len(), 1);
        assert!(matches!(&context[0], Message::User(user) if user.text().contains("we tried b")));
    }

    #[tokio::test]
    async fn navigating_to_an_unknown_entry_fails() {
        let mut runtime = runtime(vec![user("a")]);

        let events = runtime
            .handle(SessionCommand::NavigateTree {
                entry_id: mush_session::tree::EntryId::new(),
                summary: None,
            })
            .await;

        assert!(matches!(events.as_slice(), [SessionEvent::Failed(_)]));
        assert_eq!(runtime.conversation.context_len(), 1);
    }

    #[tokio::test]
    async fn name_sets_the_title_and_stops_generation() {
        let mut runtime = runtime(vec![]);

        let events = runtime
            .handle(SessionCommand::Name {
                title: Some("port prime".into()),
            })
            .await;

        assert_eq!(runtime.title.as_deref(), Some("port prime"));
        assert!(runtime.title_generated);
        assert_eq!(events[0], SessionEvent::Title(Some("port prime".into())));
    }

    #[tokio::test]
    async fn resuming_a_session_with_a_snapshot_restores_its_kernel_in_the_background() {
        let data = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("MUSH_DATA_DIR", data.path()) };
        let saved = mush_session::Session::new("model", "/tmp");
        SessionStore::new(SessionStore::default_dir())
            .save(&saved)
            .unwrap();
        let path = SessionStore::kernel_state_path(&saved.meta.id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"snapshot").unwrap();
        let kernel = Arc::new(RecordingKernel(std::sync::Mutex::new(Vec::new())));
        let mut runtime = SessionRuntime::new(ConversationState::new());
        runtime.set_kernel_state(Some(kernel.clone()));
        let before = runtime.kernel_restore_pending();

        runtime
            .handle(SessionCommand::Resume {
                id: saved.meta.id.clone(),
            })
            .await;
        let pending = runtime.kernel_restore_pending();
        let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(status) = runtime.settle_kernel_restore() {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert!(!before);
        assert!(pending);
        assert_eq!(settled, "python state restored");
        assert_eq!(kernel.0.lock().unwrap().last().unwrap().state_path, path);
    }

    #[tokio::test]
    async fn resuming_a_missing_session_fails() {
        let mut runtime = runtime(vec![]);

        let events = runtime
            .handle(SessionCommand::Resume {
                id: SessionId::from("does-not-exist"),
            })
            .await;

        assert!(matches!(events.as_slice(), [SessionEvent::Failed(_)]));
    }
}
