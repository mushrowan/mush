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
    ThinkingLevel(mush_ai::types::ThinkingLevel),
    Accounts(mush_ai::types::SessionAccounts),
}

impl SessionRuntime {
    pub async fn handle(&mut self, command: SessionCommand) -> Vec<SessionEvent> {
        let mut events = self.dispatch(command).await;
        if events
            .iter()
            .any(|event| matches!(event, SessionEvent::TranscriptReplaced { .. }))
        {
            self.prompt_cache.forget();
            self.refresh_context_tokens();
            if let Some(event) = self.adopt_branch_thinking_level() {
                events.insert(0, event);
            }
        }
        events
    }

    pub fn adopt_branch_thinking_level(&mut self) -> Option<SessionEvent> {
        let level = self
            .conversation
            .thinking_level()
            .filter(|level| *level != self.thinking_level)?;
        self.models.set_thinking(level.clone());
        self.thinking_level = level.clone();
        Some(SessionEvent::ThinkingLevel(level))
    }

    pub fn record_thinking_level(&mut self) -> bool {
        self.conversation
            .record_thinking_level(&self.thinking_level)
    }

    async fn dispatch(&mut self, command: SessionCommand) -> Vec<SessionEvent> {
        match command {
            SessionCommand::New => {
                self.leave_session().await;
                if let Some(kernel) = &self.kernel_state {
                    kernel.reset().await;
                }
                self.restore_rlm_max_depth(None);
                self.set_accounts(mush_ai::types::SessionAccounts::default());
                self.enter_fresh_session(ConversationState::new());
                self.fresh_session_events(
                    Replaced::New,
                    None,
                    "new session started (previous session saved)".into(),
                )
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
                if lease.is_some() && !loaded.meta.lifecycle.is_live() {
                    let _ = store.set_lifecycle(&id, mush_session::Lifecycle::Live);
                }
                self.leave_session().await;
                if let Some(kernel) = &self.kernel_state {
                    kernel.reset().await;
                }
                if let Some(prompt) = loaded.system_prompt {
                    self.system_prompt = Some(prompt);
                }
                let title = loaded.meta.title.clone();
                self.restore_rlm_max_depth(loaded.meta.rlm_max_depth);
                self.set_accounts(loaded.meta.accounts.clone());
                self.replace_session(id, loaded.conversation, title.clone(), lease);
                self.begin_kernel_restore();
                self.title_generated = title.is_some();
                let mut events = self.session_changed_events();
                events.push(SessionEvent::SystemPromptChanged(
                    self.system_prompt.clone(),
                ));
                events.push(self.transcript_replaced(Replaced::Resumed, None));
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
                let Some(forked) = self.conversation.forked_before(position) else {
                    return vec![SessionEvent::Failed(String::from(
                        "that message is no longer in the conversation",
                    ))];
                };
                self.leave_session().await;
                self.enter_fresh_session(forked);
                self.fresh_session_events(
                    Replaced::Forked,
                    Some(prompt),
                    format!("forked before message {before_user_message}"),
                )
            }
            SessionCommand::Clone => {
                let branch = self.conversation.cloned_branch();
                self.leave_session().await;
                self.enter_fresh_session(branch);
                self.fresh_session_events(
                    Replaced::Cloned,
                    None,
                    "cloned the current branch into a new session".into(),
                )
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
                let from = self.conversation.leaf_id().cloned();
                self.move_leaf_to(parent);
                let carried = self.settle_move(from, Some(&preview));
                vec![
                    self.transcript_replaced(Replaced::Branched, None),
                    SessionEvent::Status(format!("branched before: {preview}{carried}")),
                ]
            }
            SessionCommand::NavigateTree { entry_id, summary } => {
                self.navigate_tree(&entry_id, summary)
            }
            SessionCommand::Undo => {
                let branch = self.conversation.tree().current_branch();
                let undone = branch.iter().rev().find_map(|entry| match &entry.kind {
                    mush_session::EntryKind::Compaction { .. } => {
                        Some((entry.parent_id.clone(), "undid compaction", None))
                    }
                    mush_session::EntryKind::Message {
                        message: Message::User(_),
                    } => Some((
                        entry.parent_id.clone(),
                        "undid last turn",
                        Some(entry_preview(entry)),
                    )),
                    _ => None,
                });
                match undone {
                    None => vec![SessionEvent::Failed("nothing to undo".into())],
                    Some((parent, status, preview)) => {
                        let from = self.conversation.leaf_id().cloned();
                        self.move_leaf_to(parent);
                        let carried = self.settle_move(from, preview.as_deref());
                        vec![
                            self.transcript_replaced(Replaced::Undone, None),
                            SessionEvent::Status(format!("{status}{carried}")),
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
            accounts: self.accounts.clone(),
            paused: self.paused.clone(),
            rlm_max_depth: (self.rlm_max_depth_source == crate::session::RlmMaxDepthSource::Chat)
                .then_some(self.rlm_max_depth),
            spawn_view: self.spawn_view.clone(),
        }
    }

    pub fn set_accounts(&mut self, accounts: mush_ai::types::SessionAccounts) {
        self.models.set_accounts(accounts.clone());
        self.accounts = accounts;
    }

    #[must_use]
    pub fn account_for(
        &self,
        model: &mush_ai::types::Model,
    ) -> Option<mush_ai::types::AccountLabel> {
        let oauth_provider = mush_ai::auth::oauth_provider_id(&model.provider)?;
        self.accounts.get(oauth_provider).cloned()
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
        self.rlm_max_depth_source = crate::session::RlmMaxDepthSource::Chat;
        self.attach_kernel_session();
        self.save_now();
    }

    pub fn restore_rlm_max_depth(&mut self, chat: Option<u32>) {
        (self.rlm_max_depth, self.rlm_max_depth_source) = match chat {
            Some(max_depth) => (max_depth, crate::session::RlmMaxDepthSource::Chat),
            None => self.rlm_max_depth_base,
        };
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
        self.kernel_released = false;
        self.harness_digest_pending = true;
        self.attach_kernel_session();
    }

    async fn leave_session(&self) {
        self.save_now();
        self.snapshot_kernel(&self.session_id).await;
    }

    fn enter_fresh_session(&mut self, conversation: ConversationState) {
        let id = SessionId::new();
        let lease = self.next_lease(&id);
        self.replace_session(id, conversation, None, lease);
    }

    fn fresh_session_events(
        &self,
        reason: Replaced,
        editor_text: Option<String>,
        status: String,
    ) -> Vec<SessionEvent> {
        let mut events = self.session_changed_events();
        events.push(self.transcript_replaced(reason, editor_text));
        events.push(SessionEvent::Status(status));
        events
    }

    fn transcript_replaced(&self, reason: Replaced, editor_text: Option<String>) -> SessionEvent {
        SessionEvent::TranscriptReplaced {
            reason,
            messages: self.conversation.context(),
            editor_text,
        }
    }

    fn settle_move(
        &mut self,
        from: Option<mush_session::tree::EntryId>,
        before: Option<&str>,
    ) -> String {
        let to = self.conversation.leaf_id().cloned();
        if from == to {
            return String::new();
        }
        let left = self
            .conversation
            .tree()
            .left_behind(from.as_ref(), to.as_ref());
        let notice = moved_notice(self.conversation.tree(), &left, before);
        let carried = left.events.len();
        self.conversation.carry(notice, left.events);
        match carried {
            0 => String::new(),
            1 => " \u{b7} 1 event from the branch you left carried over".into(),
            carried => format!(" \u{b7} {carried} events from the branch you left carried over"),
        }
    }

    fn move_leaf_to(&mut self, parent: Option<mush_session::tree::EntryId>) {
        match parent {
            Some(parent) => {
                self.conversation.branch(&parent);
            }
            None => self.conversation.reset_leaf(),
        }
    }

    fn session_changed_events(&self) -> Vec<SessionEvent> {
        vec![
            SessionEvent::SessionIdChanged(self.session_id.clone()),
            SessionEvent::Title(self.title.clone()),
            SessionEvent::Accounts(self.accounts.clone()),
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
        let before = editor_text.is_some().then(|| preview.clone());
        let from = self.conversation.leaf_id().cloned();
        self.harness_digest_pending = true;
        self.prompt_cache.forget();
        let status = match summary {
            Some(summary) => {
                match new_leaf {
                    Some(leaf) => {
                        self.conversation.branch_with_summary(&leaf, summary);
                    }
                    None => {
                        self.conversation.reset_leaf_with_summary(summary);
                    }
                }
                format!("branched before: {preview}, with a summary of the branch left behind")
            }
            None => {
                self.move_leaf_to(new_leaf);
                format!("branched before: {preview}")
            }
        };
        let status = status + &self.settle_move(from, before.as_deref());
        self.adopt_branch_thinking_level()
            .into_iter()
            .chain([
                self.transcript_replaced(Replaced::Branched, editor_text),
                SessionEvent::Status(status),
            ])
            .collect()
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

fn moved_notice(
    tree: &mush_session::tree::SessionTree,
    left: &mush_session::tree::LeftBehind,
    before: Option<&str>,
) -> mush_ai::types::EventMessage {
    let at = |stamp: mush_ai::types::Timestamp| {
        i64::try_from(stamp.as_ms())
            .ok()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map_or_else(String::new, |at| {
                format!(", {}", at.format("%Y-%m-%d %H:%M UTC"))
            })
    };
    let place = match (before, tree.leaf_id().and_then(|id| tree.by_id(id))) {
        (Some(preview), _) => format!("to just before their message \"{preview}\""),
        (None, Some(entry)) => format!(
            "to entry {} ({}{})",
            entry.id,
            entry_kind(entry),
            at(entry.timestamp)
        ),
        (None, None) => "to the start of the session".to_string(),
    };
    let stays = match (left.entries, &left.last) {
        (0, _) | (_, None) => String::new(),
        (1, Some(last)) => format!(
            " The entry after that point, {last}, stays in the session tree but is no longer in your context."
        ),
        (entries, Some(last)) => format!(
            " The {entries} entries after that point, up to {last}, stay in the session tree but are no longer in your context."
        ),
    };
    let carried = match left.events.len() {
        0 => String::new(),
        1 => "\n\nThe event below arrived after that point and is carried over.".to_string(),
        events => format!(
            "\n\nThe {events} events below arrived after that point and are carried over, in the order they arrived."
        ),
    };
    let now = mush_ai::types::Timestamp::now();
    mush_ai::types::EventMessage {
        content: format!(
            "{MOVED_TAG}\n\nThe user moved this conversation {place} and is continuing from there.{stays}\n\n\
             Only the conversation moved. Files, the python kernel and its variables, child agents, \
             background jobs, notes, the goal, scheduled jobs and harness memory are as they were just \
             before the move, so anything done after that point may still be in effect.{carried}"
        ),
        timestamp_ms: now,
        kind: mush_ai::types::EventKind::Moved,
        receipts: left
            .events
            .iter()
            .filter_map(|event| event.receipts.first().cloned())
            .collect(),
    }
}

pub const MOVED_TAG: &str = "[branch-moved]";

fn entry_kind(entry: &mush_session::tree::SessionEntry) -> &'static str {
    match &entry.kind {
        mush_session::tree::EntryKind::Message { message } => match message {
            Message::User(_) => "a user message",
            Message::Assistant(_) => "your reply",
            Message::ToolResult(_) => "a tool result",
            Message::Event(_) => "an event",
        },
        mush_session::tree::EntryKind::BranchSummary { .. } => "a branch summary",
        mush_session::tree::EntryKind::Compaction { .. } => "a compaction summary",
        mush_session::tree::EntryKind::Refinement { .. } => "a harness refinement",
        mush_session::tree::EntryKind::ThinkingLevelChange { .. }
        | mush_session::tree::EntryKind::Unknown(_) => "a setting change",
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
        fn attach_session(&self, session: mush_kernel::KernelSession) {
            self.0.lock().unwrap().push(session);
        }
    }

    #[tokio::test]
    async fn the_kernel_learns_the_session_dir_and_depth_on_every_switch() {
        let kernel = Arc::new(RecordingKernel(std::sync::Mutex::new(Vec::new())));
        let mut runtime = SessionRuntime::new(ConversationState::new());
        runtime.rlm_max_depth_base = (4, crate::session::RlmMaxDepthSource::Global);
        runtime.set_kernel_state(Some(kernel.clone()));
        runtime.set_rlm_max_depth(1);
        runtime.handle(SessionCommand::New).await;

        let seen = kernel.0.lock().unwrap().clone();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[1].max_depth, 1);
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

    #[tokio::test]
    async fn a_chat_max_depth_is_saved_and_a_new_session_falls_back() {
        let saved = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut runtime = SessionRuntime::new(ConversationState::new());
        runtime.rlm_max_depth_base = (2, crate::session::RlmMaxDepthSource::Global);
        runtime.restore_rlm_max_depth(None);
        runtime.saver = Some(Arc::new({
            let saved = Arc::clone(&saved);
            move |snapshot: crate::snapshot::SessionSnapshot| {
                saved.lock().unwrap().push(snapshot.rlm_max_depth);
            }
        }));
        assert_eq!(runtime.snapshot().rlm_max_depth, None);

        runtime.set_rlm_max_depth(0);
        assert_eq!(saved.lock().unwrap().last(), Some(&Some(0)));
        assert_eq!(
            runtime.rlm_max_depth_source,
            crate::session::RlmMaxDepthSource::Chat
        );

        runtime.handle(SessionCommand::New).await;
        assert_eq!(runtime.rlm_max_depth, 2);
        assert_eq!(
            runtime.rlm_max_depth_source,
            crate::session::RlmMaxDepthSource::Global
        );
        assert_eq!(runtime.snapshot().rlm_max_depth, None);

        runtime.restore_rlm_max_depth(Some(5));
        assert_eq!(runtime.rlm_max_depth, 5);
        assert_eq!(runtime.snapshot().rlm_max_depth, Some(5));
    }

    #[tokio::test]
    async fn a_new_session_starts_on_the_default_account() {
        let mut runtime = SessionRuntime::new(ConversationState::new());
        let mut accounts = mush_ai::types::SessionAccounts::default();
        accounts.set(
            "anthropic",
            Some(mush_ai::types::AccountLabel::parse("work").unwrap()),
        );
        runtime.set_accounts(accounts);

        let events = runtime.handle(SessionCommand::New).await;

        assert!(runtime.accounts.is_empty());
        assert!(runtime.models.accounts().is_empty());
        assert!(
            events.iter().any(
                |event| matches!(event, SessionEvent::Accounts(accounts) if accounts.is_empty())
            )
        );
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
        assert_eq!(without_notices(&runtime), vec![user("a"), user("b")]);
        assert!(matches!(
            replaced(&undone),
            Some((Replaced::Undone, _, None))
        ));

        let branched = runtime
            .handle(SessionCommand::Branch { user_message: 1 })
            .await;
        assert!(without_notices(&runtime).is_empty());
        assert!(matches!(
            replaced(&branched),
            Some((Replaced::Branched, _, None))
        ));

        let nothing = runtime.handle(SessionCommand::Undo).await;
        assert!(matches!(nothing.as_slice(), [SessionEvent::Failed(_)]));
    }

    #[tokio::test]
    async fn undo_after_a_compaction_takes_back_only_the_compaction() {
        let mut runtime = runtime(vec![user("a"), user("b"), user("c"), user("d"), user("e")]);
        let before = runtime.conversation.context();
        let compact =
            mush_session::compact::compact_with_summary(before.clone(), "earlier", Some(2));
        runtime
            .conversation
            .apply_compaction(compact.summary, compact.summarised_count, &compact.messages)
            .unwrap();

        let undone = runtime.handle(SessionCommand::Undo).await;
        assert_eq!(without_notices(&runtime), before);
        assert!(
            undone
                .iter()
                .any(|event| matches!(event, SessionEvent::Status(s) if s == "undid compaction"))
        );

        runtime.handle(SessionCommand::Undo).await;
        assert_eq!(without_notices(&runtime), before[..4]);
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

        assert_eq!(without_notices(&runtime), vec![user("a")]);
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

        assert_eq!(
            without_notices(&runtime),
            vec![user("a"), assistant("reply")]
        );
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
        assert_eq!(context.len(), 2);
        assert!(matches!(&context[0], Message::User(user) if user.text().contains("we tried b")));
        assert!(is_notice(&context[1]));
    }

    fn is_notice(message: &Message) -> bool {
        matches!(message, Message::Event(event) if event.kind == mush_ai::types::EventKind::Moved)
    }

    fn without_notices(runtime: &SessionRuntime) -> Vec<Message> {
        runtime
            .conversation
            .context()
            .into_iter()
            .filter(|message| !is_notice(message))
            .collect()
    }

    fn child_message(text: &str, receipt: &str) -> Message {
        Message::Event(mush_ai::types::EventMessage {
            content: format!("[agent-message from child:auth]\n\n{text}"),
            timestamp_ms: Timestamp::zero(),
            kind: mush_ai::types::EventKind::Message,
            receipts: vec![mush_ai::types::EventReceipt {
                id: receipt.into(),
                timestamp_ms: Timestamp::zero(),
                kind: mush_ai::types::EventKind::Message,
                origin: mush_ai::types::EventOrigin::Agent {
                    session_id: "child".into(),
                    name: "auth".into(),
                    relationship: "child".into(),
                },
            }],
        })
    }

    fn status(events: &[SessionEvent]) -> String {
        events
            .iter()
            .find_map(|event| match event {
                SessionEvent::Status(status) => Some(status.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn moving_back_tells_the_agent_and_carries_what_arrived_after_the_point_it_left() {
        let mut runtime = runtime(vec![
            user("a"),
            assistant("reply"),
            user("b"),
            child_message("found the race", "agentmsg_1"),
            assistant("noted"),
        ]);
        let target = runtime.conversation.tree().entries()[2].id.clone();

        let events = runtime
            .handle(SessionCommand::NavigateTree {
                entry_id: target,
                summary: None,
            })
            .await;

        let context = runtime.conversation.context();
        assert_eq!(context[..2], [user("a"), assistant("reply")]);
        let Message::Event(notice) = &context[2] else {
            panic!("{context:?}");
        };
        assert_eq!(notice.kind, mush_ai::types::EventKind::Moved);
        assert!(
            notice.content.starts_with(
                "[branch-moved]\n\nThe user moved this conversation to just before their message \"b\""
            ),
            "{}",
            notice.content
        );
        assert!(
            notice.content.contains("The 3 entries after that point"),
            "{}",
            notice.content
        );
        assert!(notice.content.contains("Only the conversation moved."));
        assert!(
            notice
                .content
                .ends_with("The event below arrived after that point and is carried over.")
        );
        assert_eq!(notice.text(), notice.content);
        assert_eq!(context[3], child_message("found the race", "agentmsg_1"));
        assert_eq!(context.len(), 4);
        assert_eq!(
            status(&events),
            "branched before: b \u{b7} 1 event from the branch you left carried over"
        );
    }

    #[tokio::test]
    async fn moving_back_and_forth_carries_each_event_once() {
        let mut runtime = runtime(vec![
            user("a"),
            user("b"),
            child_message("found the race", "agentmsg_1"),
        ]);
        let left = runtime.conversation.leaf_id().cloned().unwrap();
        let b = runtime.conversation.tree().entries()[1].id.clone();

        runtime
            .handle(SessionCommand::NavigateTree {
                entry_id: b,
                summary: None,
            })
            .await;
        let events = runtime
            .handle(SessionCommand::NavigateTree {
                entry_id: left,
                summary: None,
            })
            .await;

        let context = runtime.conversation.context();
        let copies = context
            .iter()
            .filter(|message| **message == child_message("found the race", "agentmsg_1"))
            .count();
        assert_eq!(copies, 1, "{context:?}");
        assert!(is_notice(context.last().unwrap()));
        assert!(
            !status(&events).contains("carried over"),
            "{}",
            status(&events)
        );
    }

    #[tokio::test]
    async fn an_agent_message_steered_as_text_by_an_older_build_is_carried_as_an_event() {
        let Message::Event(event) = child_message("found the race", "agentmsg_1") else {
            unreachable!()
        };
        let mut runtime = runtime(vec![user("a"), user("b"), user(&event.text())]);
        let b = runtime.conversation.tree().entries()[1].id.clone();

        runtime
            .handle(SessionCommand::NavigateTree {
                entry_id: b,
                summary: None,
            })
            .await;

        let context = runtime.conversation.context();
        assert_eq!(context.last(), Some(&Message::Event(event)));
    }

    #[tokio::test]
    async fn a_summary_of_the_branch_left_does_not_stand_in_for_its_events() {
        let mut runtime = runtime(vec![
            user("a"),
            user("b"),
            child_message("found the race", "agentmsg_1"),
        ]);
        let b = runtime.conversation.tree().entries()[1].id.clone();

        runtime
            .handle(SessionCommand::NavigateTree {
                entry_id: b,
                summary: Some("we asked b and auth answered".into()),
            })
            .await;

        let context = runtime.conversation.context();
        assert_eq!(
            context.last(),
            Some(&child_message("found the race", "agentmsg_1"))
        );
    }

    #[tokio::test]
    async fn undo_also_leaves_the_notice_and_staying_put_leaves_nothing() {
        let mut runtime = runtime(vec![user("a"), assistant("reply")]);
        let leaf = runtime.conversation.leaf_id().cloned().unwrap();

        runtime
            .handle(SessionCommand::NavigateTree {
                entry_id: leaf,
                summary: None,
            })
            .await;
        assert_eq!(
            runtime.conversation.context_len(),
            2,
            "staying put is not a move"
        );

        let events = runtime.handle(SessionCommand::Undo).await;
        let context = runtime.conversation.context();
        assert!(
            matches!(&context[..], [notice] if is_notice(notice)),
            "{context:?}"
        );
        assert_eq!(status(&events), "undid last turn");
    }

    #[tokio::test]
    async fn navigating_to_another_branch_adopts_the_thinking_level_it_ran_with() {
        use mush_ai::types::ThinkingLevel;
        let mut runtime = runtime(vec![]);
        runtime
            .conversation
            .record_thinking_level(&ThinkingLevel::Low);
        runtime.conversation.append_message(user("a"));
        runtime
            .conversation
            .record_thinking_level(&ThinkingLevel::High);
        runtime.conversation.append_message(user("b"));
        runtime.thinking_level = ThinkingLevel::High;
        let target = runtime
            .conversation
            .user_messages_in_branch()
            .last()
            .unwrap()
            .id
            .clone();

        let events = runtime
            .handle(SessionCommand::NavigateTree {
                entry_id: target,
                summary: None,
            })
            .await;

        assert_eq!(runtime.thinking_level, ThinkingLevel::High);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, SessionEvent::ThinkingLevel(_)))
        );

        let first = runtime.conversation.user_messages_in_branch()[0].id.clone();
        let events = runtime
            .handle(SessionCommand::NavigateTree {
                entry_id: first,
                summary: None,
            })
            .await;

        assert_eq!(runtime.thinking_level, ThinkingLevel::Low);
        assert_eq!(events[0], SessionEvent::ThinkingLevel(ThinkingLevel::Low));
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
                title: Some("port parser".into()),
            })
            .await;

        assert_eq!(runtime.title.as_deref(), Some("port parser"));
        assert!(runtime.title_generated);
        assert_eq!(events[0], SessionEvent::Title(Some("port parser".into())));
    }

    #[tokio::test]
    async fn resuming_a_session_with_a_snapshot_restores_its_kernel_in_the_background() {
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
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
