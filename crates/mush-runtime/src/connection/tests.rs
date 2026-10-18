use super::*;
use mush_ai::types::{Api, AssistantContentPart, AssistantMessage, StopReason, TextContent};
use mush_session::ConversationState;
use std::sync::{Arc, Mutex};

fn connection() -> (InProcessConnection, crate::testing::Requests) {
    crate::testing::echo_connection()
}

async fn drain_turn(connection: &mut InProcessConnection) -> Vec<ClientEvent> {
    let mut events = Vec::new();
    loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(5), connection.next_event())
                .await
                .expect("an event before the timeout")
                .expect("the connection stays open");
        let done = matches!(event, ClientEvent::TurnEnded { .. });
        events.push(event);
        if done {
            return events;
        }
    }
}

#[tokio::test]
async fn a_prompt_runs_a_turn_and_the_transcript_holds_both_sides() {
    let (mut connection, requests) = connection();

    let started = connection
        .send(ClientCommand::Prompt {
            text: "hello".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    let events = drain_turn(&mut connection).await;

    assert!(
        matches!(started[..], [ClientEvent::TurnStarted]),
        "{started:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::Agent {
            event: AgentEvent::MessageEnd { .. },
            applied: Applied::Appended,
            ..
        }
    )));
    let transcript = connection.transcript().await;
    assert_eq!(transcript.len(), 2);
    assert!(matches!(&transcript[0], Message::User(user) if user.text() == "hello"));
    assert!(matches!(&transcript[1], Message::Assistant(_)));
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(!connection.state().turn_active);
    assert_eq!(connection.state().context_len, 2);
}

#[tokio::test]
async fn a_new_session_replaces_the_transcript_through_events() {
    let (mut connection, _) = connection();
    connection
        .send(ClientCommand::Prompt {
            text: "hello".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;
    let before = connection.state().session_id.clone();

    let events = connection
        .send(ClientCommand::Session(SessionCommand::New))
        .await
        .unwrap();

    assert!(matches!(
        events.first(),
        Some(ClientEvent::Session(SessionEvent::SessionIdChanged(_)))
    ));
    assert_ne!(connection.state().session_id, before);
    assert!(connection.transcript().await.is_empty());
    assert_eq!(connection.state().context_tokens, TokenCount::ZERO);
}

fn user(text: &str) -> Message {
    Message::User(
        UserMessage::builder()
            .content(mush_ai::types::UserContent::Text(text.into()))
            .timestamp_ms(Timestamp::zero())
            .build(),
    )
}

fn connection_replying(reply: &str) -> (InProcessConnection, crate::testing::Requests) {
    let requests: crate::testing::Requests = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(crate::testing::Echo {
        requests: requests.clone(),
        reply: reply.into(),
    }));
    let model = crate::testing::echo_model();
    let mut runtime = SessionRuntime::new(ConversationState::new());
    runtime.model_id = model.id.to_string();
    let connection = InProcessConnection::new(
        runtime,
        ToolRegistry::new(),
        registry,
        crate::testing::echo_config(model),
        mush_agent::AgentInjections::default(),
    );
    (connection, requests)
}

async fn wait_for(
    connection: &mut InProcessConnection,
    accept: impl Fn(&ClientEvent) -> bool,
) -> ClientEvent {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(event) = connection.next_event().await
                && accept(&event)
            {
                break event;
            }
        }
    })
    .await
    .unwrap()
}

async fn pump_until(
    connection: &mut InProcessConnection,
    accept: impl Fn(&ClientEvent) -> bool,
) -> ClientEvent {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            connection.start_pending().await;
            if let Some(event) = connection.try_next_event() {
                if accept(&event) {
                    break event;
                }
                continue;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn refine_applies_the_planned_edits_and_the_next_turn_carries_the_digest() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let (mut connection, requests) = connection_replying(
        "{\"summary\":\"remember the runner\",\"rationale\":\"seen twice\",\"expectedOutcome\":\"fewer misses\",\"edits\":[{\"action\":\"create\",\"kind\":\"memory\",\"title\":\"test runner\",\"content\":\"cargo nextest run\"}]}",
    );
    connection.runtime.saver = Some(Arc::new(|_| {}));
    connection.runtime.conversation = ConversationState::from_messages(vec![user("hi")]);

    let started = connection
        .send(ClientCommand::Refine(
            mush_session::refine::RefineRequest::default(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        started[..],
        [ClientEvent::Session(SessionEvent::Status(_))]
    ));
    let refined = wait_for(&mut connection, |event| {
        matches!(event, ClientEvent::Refined(_))
    })
    .await;
    let ClientEvent::Refined(result) = refined else {
        unreachable!()
    };
    assert_eq!(result.scope, mush_session::harness::Scope::Local);
    assert_eq!(result.applied_edits[0].id, "test_runner");
    assert!(result.applied_edits[0].applied);
    let local = mush_session::harness::load_merged(&connection.runtime.session_id);
    assert_eq!(
        local.entries[&mush_session::harness::Kind::Memory]["test_runner"].content,
        "cargo nextest run"
    );
    assert_eq!(connection.runtime.refinement_history().len(), 1);

    connection
        .send(ClientCommand::Prompt {
            text: "next".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;
    let turn = requests.lock().unwrap()[1].messages.clone();
    assert_eq!(turn.len(), 4);
    assert!(
        matches!(&turn[1], Message::User(u) if u.text() == "[user-refinement]\n\nthese harness edits are already applied, nothing to do\nsummary: remember the runner\n- created memory [local:test_runner] test runner: cargo nextest run"),
        "{turn:?}"
    );
    let Message::User(digest) = &turn[2] else {
        panic!("{turn:?}")
    };
    assert!(
        digest
            .text()
            .starts_with(mush_session::harness::DIGEST_PREFIX)
    );
    assert!(digest.text().contains("[local:test_runner] test runner"));
    assert!(matches!(&turn[3], Message::User(u) if u.text() == "next"));

    connection
        .send(ClientCommand::Prompt {
            text: "again".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;
    let turn = requests.lock().unwrap()[2].messages.clone();
    assert!(
        !turn
            .iter()
            .skip(3)
            .any(|message| matches!(message, Message::User(u) if u.text().starts_with(mush_session::harness::DIGEST_PREFIX))),
        "an unchanged harness is not re-delivered"
    );
}

fn is_digest(message: &Message) -> bool {
    matches!(message, Message::User(u) if u.text().starts_with(mush_session::harness::DIGEST_PREFIX))
}

async fn prompt(connection: &mut InProcessConnection, text: &str) {
    connection
        .send(ClientCommand::Prompt {
            text: text.into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(connection).await;
}

#[tokio::test]
async fn a_mid_session_refine_reaches_the_model_as_a_notice_and_the_digest_waits_for_a_cold_boundary()
 {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let (mut connection, requests) = connection_replying(
        "{\"summary\":\"remember the runner\",\"edits\":[{\"action\":\"create\",\"kind\":\"memory\",\"title\":\"test runner\",\"content\":\"cargo nextest run\"}]}",
    );
    connection.runtime.saver = Some(Arc::new(|_| {}));

    prompt(&mut connection, "hello").await;
    connection
        .send(ClientCommand::Refine(
            mush_session::refine::RefineRequest::default(),
        ))
        .await
        .unwrap();
    wait_for(&mut connection, |event| {
        matches!(event, ClientEvent::Refined(_))
    })
    .await;
    prompt(&mut connection, "next").await;

    let turn = requests.lock().unwrap()[2].messages.clone();
    assert!(!turn.iter().any(is_digest), "{turn:?}");
    assert!(
        matches!(&turn[turn.len() - 2], Message::User(u) if u.text().starts_with("[user-refinement]\n\nthese harness edits are already applied")),
        "{turn:?}"
    );

    let leaf = connection.runtime.conversation.leaf_id().unwrap().clone();
    connection.runtime.navigate_tree(&leaf, None);
    prompt(&mut connection, "again").await;

    let turn = requests.lock().unwrap()[3].messages.clone();
    assert!(is_digest(&turn[turn.len() - 2]), "{turn:?}");
    assert_eq!(turn.iter().filter(|message| is_digest(message)).count(), 1);
}

#[tokio::test]
async fn the_turn_after_a_compaction_carries_the_digest_again() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let (mut connection, requests) = connection();
    connection.runtime.saver = Some(Arc::new(|_| {}));
    let mut state = mush_session::harness::State::default();
    state
        .upsert(
            &mush_session::harness::Edit {
                action: mush_session::harness::Action::Create,
                kind: mush_session::harness::Kind::Memory,
                id: None,
                title: Some("test runner".into()),
                content: Some("cargo nextest run".into()),
                path: None,
                reference: None,
                arguments: None,
                metadata: None,
                reason: None,
            },
            mush_session::harness::Scope::Local,
            "test",
        )
        .unwrap();
    state
        .save(&mush_session::harness::local_dir(
            &connection.runtime.session_id,
        ))
        .unwrap();

    prompt(&mut connection, "one").await;
    prompt(&mut connection, "two").await;
    let context = connection.runtime.conversation.context();
    connection.runtime.apply_compaction(
        "earlier work".into(),
        3,
        &[vec![user("earlier work")], context[3..].to_vec()].concat(),
    );
    prompt(&mut connection, "three").await;

    let asked = requests.lock().unwrap();
    assert!(is_digest(&asked[0].messages[0]), "{:?}", asked[0].messages);
    assert!(!asked[1].messages.iter().skip(1).any(is_digest));
    let after = &asked[2].messages;
    assert!(is_digest(&after[after.len() - 2]), "{after:?}");
}

#[tokio::test]
async fn the_digest_ranks_entries_by_the_goal_and_the_prompt() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let (mut connection, requests) = connection();
    connection.runtime.saver = Some(Arc::new(|_| {}));
    let memory = |title: &str, content: &str| mush_session::harness::Edit {
        action: mush_session::harness::Action::Create,
        kind: mush_session::harness::Kind::Memory,
        id: None,
        title: Some(title.into()),
        content: Some(content.into()),
        path: None,
        reference: None,
        arguments: None,
        metadata: None,
        reason: None,
    };
    let mut state = mush_session::harness::State::default();
    for name in [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf",
    ] {
        state
            .upsert(
                &memory(&format!("{name} note"), "nothing in particular"),
                mush_session::harness::Scope::Local,
                "test",
            )
            .unwrap();
    }
    for (title, content) in [
        ("xray kernel", "the xray kernel restarts on every deploy"),
        (
            "yankee parser",
            "the yankee parser keeps its grammar in crates/grammar",
        ),
    ] {
        state
            .upsert(
                &memory(title, content),
                mush_session::harness::Scope::Local,
                "test",
            )
            .unwrap();
    }
    state
        .save(&mush_session::harness::local_dir(
            &connection.runtime.session_id,
        ))
        .unwrap();
    let goal = Arc::new(crate::goal::GoalStore::new(None));
    goal.apply(crate::goal::GoalCommand::Create {
        objective: "rewrite the yankee parser".into(),
        token_budget: None,
    })
    .unwrap();
    connection.set_goal_store(Some(goal));

    prompt(&mut connection, "why does the xray kernel keep restarting").await;

    let asked = requests.lock().unwrap();
    let Message::User(digest) = &asked[0].messages[0] else {
        panic!("{:?}", asked[0].messages)
    };
    let text = digest.text();
    let memories: Vec<&str> = text
        .lines()
        .skip_while(|line| !line.starts_with("memory: "))
        .skip(2)
        .take(3)
        .collect();
    assert!(memories[0].contains("yankee parser"), "{text}");
    assert!(memories[1].contains("xray kernel"), "{text}");
    assert!(memories[2].contains("alpha note"), "{text}");
}

#[tokio::test]
async fn a_refine_that_returns_no_json_fails_without_touching_the_harness() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let (mut connection, _) = connection_replying("i have nothing to add");
    connection.runtime.saver = Some(Arc::new(|_| {}));

    connection
        .send(ClientCommand::Refine(
            mush_session::refine::RefineRequest::default(),
        ))
        .await
        .unwrap();
    let failed = wait_for(&mut connection, |event| {
        matches!(event, ClientEvent::Session(SessionEvent::Failed(_)))
    })
    .await;
    let ClientEvent::Session(SessionEvent::Failed(text)) = failed else {
        unreachable!()
    };
    assert!(text.starts_with("refine:"), "{text}");
    assert!(mush_session::harness::load_merged(&connection.runtime.session_id).is_empty());
}

#[tokio::test]
async fn auto_refine_reviews_after_the_interval_and_refines_when_approved() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let (mut connection, requests) =
        connection_replying("{\"shouldRefine\": true, \"rationale\": \"worth keeping\"}");
    connection.runtime.saver = Some(Arc::new(|_| {}));
    connection.config.auto_refine = mush_session::refine::AutoRefineSettings {
        enabled: true,
        turn_interval: 1,
        compact: true,
        cooldown_ms: 0,
    };

    connection
        .send(ClientCommand::Prompt {
            text: "hello".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;

    let approved = pump_until(&mut connection, |event| {
        matches!(event, ClientEvent::Session(SessionEvent::Status(text)) if text.starts_with("auto-refine review approved"))
    })
    .await;
    assert!(matches!(approved, ClientEvent::Session(_)));
    let refined = pump_until(&mut connection, |event| {
        matches!(event, ClientEvent::Refined(_))
    })
    .await;
    let ClientEvent::Refined(result) = refined else {
        unreachable!()
    };
    assert!(result.applied_edits.is_empty());
    assert!(matches!(
        connection
            .runtime
            .conversation
            .entries()
            .last()
            .map(|entry| &entry.kind),
        Some(mush_session::tree::EntryKind::Refinement {
            source: mush_session::harness::RefinementSource::Auto,
            ..
        })
    ));
    assert!(!connection.runtime.conversation.context().iter().any(|message| {
        matches!(message, Message::User(u) if mush_session::harness::is_refinement_notice(&u.text()))
    }));
    assert_eq!(connection.assistant_turns_since_review, 0);
    let asked = requests.lock().unwrap();
    assert_eq!(asked.len(), 3, "turn, review, plan");
    let Message::User(review) = &asked[1].messages[0] else {
        unreachable!()
    };
    assert!(
        review
            .text()
            .contains("<trigger>\nturn_interval; 1 assistant turns"),
        "{}",
        review.text()
    );
}

#[tokio::test]
async fn a_subagent_never_runs_the_auto_refine_review() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let (mut connection, requests) =
        connection_replying("{\"shouldRefine\": true, \"rationale\": \"worth keeping\"}");
    connection.runtime.saver = Some(Arc::new(|_| {}));
    connection.runtime.rlm_depth = 1;
    connection.config.auto_refine = mush_session::refine::AutoRefineSettings {
        enabled: true,
        turn_interval: 1,
        compact: true,
        cooldown_ms: 0,
    };

    connection
        .send(ClientCommand::Prompt {
            text: "hello".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;
    connection.start_pending().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    while connection.try_next_event().is_some() {}

    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(connection.auto_review.is_none());
}

#[tokio::test]
async fn auto_refine_stays_quiet_when_disabled() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let (mut connection, requests) =
        connection_replying("{\"shouldRefine\": true, \"rationale\": \"worth keeping\"}");
    connection.runtime.saver = Some(Arc::new(|_| {}));
    connection.config.auto_refine = mush_session::refine::AutoRefineSettings {
        enabled: false,
        turn_interval: 1,
        compact: true,
        cooldown_ms: 0,
    };

    connection
        .send(ClientCommand::Prompt {
            text: "hello".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;
    connection.start_pending().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    while connection.try_next_event().is_some() {}

    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(connection.auto_review.is_none());
}

#[tokio::test]
async fn refine_is_refused_without_a_saved_session() {
    let (mut connection, _) = connection();
    let refused = connection
        .send(ClientCommand::Refine(
            mush_session::refine::RefineRequest::default(),
        ))
        .await;
    assert!(matches!(refused, Err(ConnectionError::Refused(_))));
}

#[tokio::test]
async fn a_compaction_runs_in_the_background_applies_and_saves() {
    let (mut connection, _) = connection();
    let messages: Vec<Message> = (0..30)
        .map(|i| user(&format!("turn {i}: {}", "x".repeat(4_000))))
        .collect();
    connection.runtime.conversation = ConversationState::from_messages(messages);
    let saved = Arc::new(Mutex::new(0usize));
    let counter = saved.clone();
    connection.runtime.saver = Some(Arc::new(move |_| {
        *counter.lock().unwrap() += 1;
    }));

    let started = connection
        .send(ClientCommand::Compact {
            focus: CompactionFocus::default(),
            auto: false,
        })
        .await
        .unwrap();
    assert!(matches!(started[..], [ClientEvent::Compacting]));
    assert!(connection.state().compacting);
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(event) = connection.next_event().await {
                break event;
            }
        }
    })
    .await
    .unwrap();

    assert!(matches!(event, ClientEvent::Compacted(_)), "{event:?}");
    assert!(!connection.state().compacting);
    assert!(connection.transcript().await.len() < 30);
    assert_eq!(*saved.lock().unwrap(), 1);
}

struct LiveKernel(Option<Vec<String>>);

#[async_trait::async_trait]
impl mush_kernel::KernelState for LiveKernel {
    async fn live_names(&self) -> Option<Result<Vec<String>, String>> {
        self.0.clone().map(Ok)
    }
}

async fn compacted_with(kernel: LiveKernel) -> Vec<Message> {
    let (mut connection, _) = connection();
    connection.runtime.conversation = ConversationState::from_messages(
        (0..30)
            .map(|i| user(&format!("turn {i}: {}", "x".repeat(400))))
            .collect(),
    );
    connection.runtime.set_kernel_state(Some(Arc::new(kernel)));
    connection
        .send(ClientCommand::Compact {
            focus: CompactionFocus::default(),
            auto: false,
        })
        .await
        .unwrap();
    wait_for(&mut connection, |event| {
        matches!(event, ClientEvent::Compacted(_))
    })
    .await;
    connection.start_pending().await;
    connection.start_pending().await;
    connection.transcript().await
}

#[tokio::test]
async fn a_manual_compaction_tells_the_model_which_python_names_are_still_live() {
    let transcript = compacted_with(LiveKernel(Some(vec!["df".into(), "load".into()]))).await;

    let notices: Vec<String> = transcript
        .iter()
        .filter_map(|message| match message {
            Message::User(user) if crate::kernel_notice::is_notice(&user.text()) => {
                Some(user.text())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        [
            "[python-state]\n\nYour Python kernel persisted through compaction; its remaining \
          variables, imports, and helpers are still available. These names are still \
          defined: df, load."
        ]
    );
    assert!(matches!(transcript.last(), Some(Message::User(user)) if user.text() == notices[0]));
}

#[tokio::test]
async fn a_compaction_with_no_running_kernel_adds_no_python_notice() {
    let transcript = compacted_with(LiveKernel(None)).await;

    assert!(!transcript.iter().any(|message| matches!(
        message,
        Message::User(user) if crate::kernel_notice::is_notice(&user.text())
    )));
}

#[tokio::test]
async fn summarise_and_navigate_puts_the_model_summary_on_the_new_branch() {
    let (mut connection, requests) = connection();
    connection.runtime.conversation =
        ConversationState::from_messages(vec![user("apples"), user("bananas"), user("cherries")]);
    let target = connection.runtime.conversation.tree().entries()[1]
        .id
        .clone();

    let started = connection
        .send(ClientCommand::SummariseAndNavigate { entry_id: target })
        .await
        .unwrap();
    assert!(matches!(
        started[..],
        [ClientEvent::Session(SessionEvent::Status(_))]
    ));

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(event @ ClientEvent::Session(SessionEvent::TranscriptReplaced { .. })) =
                connection.next_event().await
            {
                break event;
            }
        }
    })
    .await
    .unwrap();

    let ClientEvent::Session(SessionEvent::TranscriptReplaced {
        messages,
        editor_text,
        ..
    }) = event
    else {
        unreachable!()
    };
    assert_eq!(editor_text.as_deref(), Some("bananas"));
    assert_eq!(messages.len(), 3);
    assert!(matches!(&messages[0], Message::User(u) if u.text() == "apples"));
    assert!(matches!(&messages[1], Message::User(u) if u.text().contains("hi back")));
    assert!(
        matches!(&messages[2], Message::Event(event) if event.kind == mush_ai::types::EventKind::Moved)
    );
    let asked = requests.lock().unwrap();
    assert_eq!(asked.len(), 1);
    let Message::User(request) = &asked[0].messages[0] else {
        unreachable!()
    };
    let summarised = request.text();
    assert!(summarised.contains("bananas"), "{summarised}");
    assert!(summarised.contains("cherries"), "{summarised}");
    assert!(!summarised.contains("apples"), "{summarised}");
}

#[tokio::test]
async fn summarise_and_navigate_skips_the_model_when_nothing_is_left_behind() {
    let (mut connection, requests) = connection();
    let reply = Message::Assistant(
        AssistantMessage::builder()
            .content(vec![AssistantContentPart::Text(TextContent {
                text: "reply".into(),
            })])
            .model("m".into())
            .provider(mush_ai::types::Provider::Anthropic)
            .api(Api::AnthropicMessages)
            .stop_reason(StopReason::Stop)
            .timestamp_ms(Timestamp::zero())
            .build(),
    );
    connection.runtime.conversation = ConversationState::from_messages(vec![user("a"), reply]);
    let assistant = connection.runtime.conversation.tree().entries()[1]
        .id
        .clone();

    let events = connection
        .send(ClientCommand::SummariseAndNavigate {
            entry_id: assistant,
        })
        .await
        .unwrap();

    assert!(matches!(
        events[0],
        ClientEvent::Session(SessionEvent::TranscriptReplaced { .. })
    ));
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_prompt_after_aborting_a_hung_tool_starts_a_new_turn() {
    let (mut connection, requests) = crate::testing::hanging_tool_connection();
    connection
        .send(ClientCommand::Prompt {
            text: "start".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    wait_for(&mut connection, |event| {
        matches!(
            event,
            ClientEvent::Agent {
                event: AgentEvent::ToolExecStart { .. },
                ..
            }
        )
    })
    .await;

    let aborted = connection.send(ClientCommand::Abort).await.unwrap();
    assert!(
        aborted
            .iter()
            .any(|event| matches!(event, ClientEvent::TurnEnded { cancelled: true })),
        "{aborted:?}"
    );
    assert!(!connection.state().turn_active, "{:?}", connection.state());

    let started = connection
        .send(ClientCommand::Prompt {
            text: "again".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    assert!(
        started
            .iter()
            .any(|event| matches!(event, ClientEvent::TurnStarted)),
        "{started:?}"
    );
    let events = drain_turn(&mut connection).await;
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::Agent {
            event: AgentEvent::MessageEnd { .. },
            ..
        }
    )));
    let asked = requests.lock().unwrap();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(matches!(asked[1].messages.last(), Some(Message::User(u)) if u.text() == "again"));
}

#[tokio::test]
async fn aborting_with_queued_steers_sends_them_together_in_a_new_turn() {
    let (mut connection, requests) = crate::testing::hanging_tool_connection();
    connection
        .send(ClientCommand::Prompt {
            text: "start".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    wait_for(&mut connection, |event| {
        matches!(
            event,
            ClientEvent::Agent {
                event: AgentEvent::ToolExecStart { .. },
                ..
            }
        )
    })
    .await;
    for text in ["first", "second"] {
        connection
            .send(ClientCommand::Steer {
                text: text.into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
    }

    let events = connection
        .send(ClientCommand::AbortAndSendQueued)
        .await
        .unwrap();
    let ended = events
        .iter()
        .position(|event| matches!(event, ClientEvent::TurnEnded { cancelled: true }));
    let started = events
        .iter()
        .position(|event| matches!(event, ClientEvent::TurnStarted));
    assert!(
        matches!((ended, started), (Some(ended), Some(started)) if ended < started),
        "{events:?}"
    );

    drain_turn(&mut connection).await;
    assert!(connection.runtime.steering_queue.lock().await.is_empty());
    let asked = requests.lock().unwrap();
    assert_eq!(asked.len(), 2, "{asked:?}");
    let texts: Vec<String> = asked[1]
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(user.text()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["start", "first", "second"]);
}

#[tokio::test]
async fn aborting_with_no_steers_only_aborts() {
    let (mut connection, requests) = crate::testing::hanging_tool_connection();
    connection
        .send(ClientCommand::Prompt {
            text: "start".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    wait_for(&mut connection, |event| {
        matches!(
            event,
            ClientEvent::Agent {
                event: AgentEvent::ToolExecStart { .. },
                ..
            }
        )
    })
    .await;

    let events = connection
        .send(ClientCommand::AbortAndSendQueued)
        .await
        .unwrap();

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ClientEvent::TurnStarted)),
        "{events:?}"
    );
    assert!(!connection.state().turn_active, "{:?}", connection.state());
    assert!(
        !connection.state().pending_prompt,
        "{:?}",
        connection.state()
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_steer_with_no_turn_joins_the_pending_prompt() {
    let (mut connection, _) = connection();
    connection.runtime.pending_prompt = Some(PendingPrompt::text("first"));
    connection.compaction_auto = false;

    connection.steer("second".into(), Vec::new()).await;

    let pending = connection.runtime.pending_prompt.as_ref().unwrap();
    assert!(pending.text.contains("first"), "{}", pending.text);
    assert!(pending.text.contains("second"), "{}", pending.text);
}

#[tokio::test]
async fn a_steer_sent_during_a_tool_call_reaches_the_next_request() {
    let requests: crate::testing::Requests = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(crate::testing::ToolThenText {
        requests: requests.clone(),
        gate: gate.clone(),
    }));
    let model = crate::testing::echo_model();
    let mut runtime = SessionRuntime::new(ConversationState::new());
    runtime.model_id = model.id.to_string();
    let mut tools = ToolRegistry::new();
    tools.register_shared(Arc::new(crate::testing::NoteTool));
    let mut connection = InProcessConnection::new(
        runtime,
        tools,
        registry,
        crate::testing::echo_config(model),
        mush_agent::AgentInjections::default(),
    );

    connection
        .send(ClientCommand::Prompt {
            text: "start".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(connection.state().turn_active);
    connection
        .send(ClientCommand::Steer {
            text: "actually, stop and do y".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    gate.notify_one();

    let events = drain_turn(&mut connection).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            ClientEvent::Agent {
                event: AgentEvent::SteeringInjected { .. },
                ..
            }
        )),
        "{events:?}"
    );
    let asked = requests.lock().unwrap();
    assert_eq!(asked.len(), 2);
    let second = &asked[1].messages;
    assert!(
        matches!(second.last(), Some(Message::User(u)) if u.text() == "actually, stop and do y"),
        "{second:?}"
    );
    assert!(matches!(&second[second.len() - 2], Message::ToolResult(_)));
}

#[tokio::test]
async fn an_agent_message_steered_during_a_tool_call_enters_the_session_as_an_event() {
    let requests: crate::testing::Requests = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(crate::testing::ToolThenText {
        requests: requests.clone(),
        gate: gate.clone(),
    }));
    let model = crate::testing::echo_model();
    let mut runtime = SessionRuntime::new(ConversationState::new());
    runtime.model_id = model.id.to_string();
    let mut tools = ToolRegistry::new();
    tools.register_shared(Arc::new(crate::testing::NoteTool));
    let mut connection = InProcessConnection::new(
        runtime,
        tools,
        registry,
        crate::testing::echo_config(model),
        mush_agent::AgentInjections::default(),
    );
    let event = mush_ai::types::EventMessage {
        content: "[agent-message from child:auth]\n\nfound it".into(),
        timestamp_ms: Timestamp::zero(),
        kind: mush_ai::types::EventKind::Message,
        receipts: vec![mush_ai::types::EventReceipt {
            id: "agentmsg_1".into(),
            timestamp_ms: Timestamp::zero(),
            kind: mush_ai::types::EventKind::Message,
            origin: mush_ai::types::EventOrigin::Agent {
                session_id: "child".into(),
                name: "auth".into(),
                relationship: "child".into(),
            },
        }],
    };

    connection
        .send(ClientCommand::Prompt {
            text: "start".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(connection.state().turn_active);
    connection.steer_event(event.clone()).await;
    gate.notify_one();
    drain_turn(&mut connection).await;

    let asked = requests.lock().unwrap().clone();
    assert!(
        matches!(asked[1].messages.last(), Some(Message::Event(sent)) if *sent == event),
        "{:?}",
        asked[1].messages
    );
    let transcript = connection.transcript().await;
    assert!(
        transcript
            .iter()
            .any(|message| matches!(message, Message::Event(kept) if *kept == event))
    );
    assert!(
        !transcript.iter().any(
            |message| matches!(message, Message::User(user) if user.text().contains("found it"))
        )
    );
}

fn steered_child_message() -> mush_ai::types::EventMessage {
    mush_ai::types::EventMessage {
        content: "[agent-message from child:auth]\n\nfound it".into(),
        timestamp_ms: Timestamp::zero(),
        kind: mush_ai::types::EventKind::Message,
        receipts: vec![mush_ai::types::EventReceipt {
            id: "agentmsg_abort".into(),
            timestamp_ms: Timestamp::zero(),
            kind: mush_ai::types::EventKind::Message,
            origin: mush_ai::types::EventOrigin::Agent {
                session_id: "child".into(),
                name: "auth".into(),
                relationship: "child".into(),
            },
        }],
    }
}

#[tokio::test]
async fn an_agent_message_steered_into_an_aborted_turn_is_kept_without_starting_another() {
    let (mut connection, _release) = crate::testing::stalled_connection();
    connection
        .send(ClientCommand::Prompt {
            text: "start".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(connection.state().turn_active);
    let event = steered_child_message();
    connection.steer_event(event.clone()).await;

    let events = connection.send(ClientCommand::Abort).await.unwrap();

    assert!(
        events
            .iter()
            .any(|sent| matches!(sent, ClientEvent::MessageKept(kept) if *kept == event)),
        "{events:?}"
    );
    let transcript = connection.transcript().await;
    assert!(
        matches!(transcript.last(), Some(Message::Event(kept)) if *kept == event),
        "{transcript:?}"
    );
    connection.start_pending().await;
    assert!(!connection.state().turn_active, "keeping it starts no turn");
    assert!(connection.runtime.pending_prompt.is_none());
}

#[tokio::test]
async fn abort_and_send_keeps_a_steered_agent_message_ahead_of_the_queued_steers() {
    let (mut connection, _release) = crate::testing::stalled_connection();
    connection
        .send(ClientCommand::Prompt {
            text: "start".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let event = steered_child_message();
    connection.steer_event(event.clone()).await;
    connection
        .send(ClientCommand::Steer {
            text: "do y instead".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();

    connection
        .send(ClientCommand::AbortAndSendQueued)
        .await
        .unwrap();

    let transcript = connection.transcript().await;
    let kept = transcript
        .iter()
        .position(|message| matches!(message, Message::Event(kept) if *kept == event))
        .expect("the agent message is kept");
    let steer = transcript
        .iter()
        .position(|message| matches!(message, Message::User(user) if user.text() == "do y instead"))
        .expect("the steer is sent");
    assert!(kept < steer, "{transcript:?}");
    assert!(connection.state().turn_active);
}

#[tokio::test]
async fn abort_cancels_a_running_compaction_and_the_queued_prompt_goes_out() {
    let (mut connection, release) = crate::testing::stalled_connection();
    connection.runtime.conversation = ConversationState::from_messages(
        (0..30)
            .map(|i| user(&format!("turn {i}: {}", "x".repeat(400))))
            .collect(),
    );
    connection
        .send(ClientCommand::Compact {
            focus: CompactionFocus::default(),
            auto: false,
        })
        .await
        .unwrap();
    assert!(connection.state().compacting);
    connection
        .send(ClientCommand::QueuePrompt {
            text: "after the compaction".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();

    let events = connection.send(ClientCommand::Abort).await.unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::CompactionCancelled)),
        "{events:?}"
    );
    assert!(!connection.state().compacting);
    assert_eq!(connection.runtime.conversation.context().len(), 30);
    pump_until(&mut connection, |event| {
        matches!(event, ClientEvent::TurnStarted)
    })
    .await;
    release.notify_waiters();
    assert!(
        connection.runtime.conversation.context().iter().any(
            |message| matches!(message, Message::User(u) if u.text() == "after the compaction")
        )
    );
    assert!(
        !connection
            .send(ClientCommand::Abort)
            .await
            .unwrap()
            .iter()
            .any(|event| matches!(event, ClientEvent::CompactionCancelled)),
        "a second abort has no compaction to cancel"
    );
}

#[tokio::test]
async fn a_compaction_request_shares_the_prefix_of_the_live_turn() {
    let (mut connection, requests) = connection();
    connection.runtime.system_prompt = Some("you are terse".into());
    let mut tools = ToolRegistry::new();
    tools.register_shared(Arc::new(crate::testing::NoteTool));
    let registry = std::mem::replace(&mut connection.registry, ApiRegistry::new());
    connection.set_deps(tools, registry);
    connection.runtime.conversation = ConversationState::from_messages(
        (0..30)
            .map(|i| user(&format!("turn {i}: {}", "x".repeat(4_000))))
            .collect(),
    );

    connection
        .send(ClientCommand::Prompt {
            text: "hello".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;
    connection
        .send(ClientCommand::Compact {
            focus: CompactionFocus::default(),
            auto: false,
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(ClientEvent::Compacted(_)) = connection.next_event().await {
                break;
            }
        }
    })
    .await
    .unwrap();

    let asked = requests.lock().unwrap();
    let [turn, compaction] = &asked[..] else {
        panic!("{}", asked.len());
    };
    assert_eq!(compaction.system_prompt, turn.system_prompt);
    assert_eq!(
        compaction.tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
        turn.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    assert_eq!(compaction.tools[0].name, "note");
    assert_eq!(compaction.messages.len(), turn.messages.len() + 2);
    assert_eq!(
        &compaction.messages[..turn.messages.len()],
        &turn.messages[..]
    );
    assert!(matches!(
        compaction.messages.last(),
        Some(Message::User(user)) if user.text().starts_with("[context checkpoint]")
    ));
}

async fn compaction_request_with_a_summariser_window(window: u64) -> mush_ai::registry::LlmContext {
    let (mut connection, requests) = connection();
    connection.runtime.system_prompt = Some("you are terse".into());
    let mut summariser = crate::testing::echo_model();
    summariser.id = "summariser".into();
    summariser.context_window = TokenCount::new(window);
    summariser.effective_context_window = None;
    connection.config.compaction_model =
        Some((summariser, mush_ai::types::StreamOptions::default()));
    connection.runtime.conversation = ConversationState::from_messages(
        (0..30)
            .map(|i| user(&format!("turn {i}: {}", "x".repeat(4_000))))
            .collect(),
    );

    connection
        .send(ClientCommand::Compact {
            focus: CompactionFocus::default(),
            auto: false,
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(ClientEvent::Compacted(_)) = connection.next_event().await {
                break;
            }
        }
    })
    .await
    .unwrap();

    let asked = requests.lock().unwrap();
    let [compaction] = &asked[..] else {
        panic!("{}", asked.len());
    };
    compaction.clone()
}

#[tokio::test]
async fn a_summariser_with_room_for_the_summary_request_gets_it() {
    let compaction = compaction_request_with_a_summariser_window(1_000_000).await;

    assert_ne!(compaction.system_prompt.as_deref(), Some("you are terse"));
    assert_eq!(compaction.messages.len(), 1);
}

#[tokio::test]
async fn a_summariser_too_small_for_the_summary_request_hands_it_to_the_session_model() {
    let compaction = compaction_request_with_a_summariser_window(10_000).await;

    assert_eq!(compaction.system_prompt.as_deref(), Some("you are terse"));
    assert!(matches!(
        compaction.messages.last(),
        Some(Message::User(user)) if user.text().starts_with("[context checkpoint]")
    ));
}

#[tokio::test]
async fn abort_answers_the_open_tool_calls_and_clears_the_steering_queue() {
    use mush_ai::types::{AssistantContentPart, ToolCall, ToolCallId, ToolName};
    let (mut connection, _) = connection();
    let call = AssistantMessage::builder()
        .content(vec![AssistantContentPart::ToolCall(ToolCall {
            id: ToolCallId::from("call_1"),
            name: ToolName::from("ipython"),
            arguments: serde_json::json!({ "code": "1" }),
        })])
        .model("m".into())
        .provider(mush_ai::types::Provider::Anthropic)
        .api(Api::AnthropicMessages)
        .stop_reason(StopReason::ToolUse)
        .timestamp_ms(Timestamp::zero())
        .build();
    connection.runtime.conversation =
        ConversationState::from_messages(vec![user("go"), Message::Assistant(call)]);
    connection
        .runtime
        .steering_queue
        .lock()
        .await
        .push(user("later"));

    let events = connection.send(ClientCommand::Abort).await.unwrap();

    assert!(matches!(
        &events[..],
        [ClientEvent::ToolsCancelled(ids)] if ids == &[ToolCallId::from("call_1")]
    ));
    let transcript = connection.transcript().await;
    assert!(matches!(
        transcript.last(),
        Some(Message::ToolResult(result))
            if result.tool_call_id.as_str() == "call_1" && result.outcome.is_error()
    ));
    assert!(connection.runtime.steering_queue.lock().await.is_empty());
}

#[tokio::test]
async fn turns_and_idle_changes_record_the_thinking_level_in_the_tree() {
    use mush_ai::types::ThinkingLevel;
    use mush_session::tree::EntryKind;
    let (mut connection, _) = connection();
    connection
        .send(ClientCommand::SetThinking(ThinkingLevel::Medium))
        .await
        .unwrap();
    assert!(connection.runtime.conversation.is_empty());

    connection
        .send(ClientCommand::Prompt {
            text: "hi".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;

    let entries = connection.runtime.conversation.entries();
    assert!(matches!(
        &entries[0].kind,
        EntryKind::ThinkingLevelChange { thinking_level } if *thinking_level == ThinkingLevel::Medium
    ));
    assert!(matches!(
        &entries[1].kind,
        EntryKind::Message {
            message: Message::User(_)
        }
    ));

    connection
        .send(ClientCommand::SetThinking(ThinkingLevel::High))
        .await
        .unwrap();
    assert_eq!(
        connection.runtime.conversation.thinking_level(),
        Some(ThinkingLevel::High)
    );
}

#[tokio::test]
async fn abort_mid_stream_keeps_the_partial_as_an_aborted_message() {
    let (mut connection, _) = connection();
    connection.runtime.conversation = ConversationState::from_messages(vec![user("go")]);
    let start = AssistantMessage::builder()
        .model("m".into())
        .provider(mush_ai::types::Provider::Anthropic)
        .api(Api::AnthropicMessages)
        .timestamp_ms(Timestamp::zero())
        .build();
    for event in [
        AgentEvent::MessageStart { message: start },
        AgentEvent::StreamEvent {
            event: mush_ai::stream::StreamEvent::TextDelta {
                content_index: 0,
                delta: "partly".into(),
            },
        },
    ] {
        connection.runtime.apply_event(&event);
    }

    connection.send(ClientCommand::Abort).await.unwrap();

    let transcript = connection.transcript().await;
    assert!(matches!(
        transcript.last(),
        Some(Message::Assistant(message))
            if message.stop_reason == StopReason::Aborted && message.text() == "partly"
    ));
}

#[tokio::test]
async fn a_finished_turn_names_the_session_when_asked() {
    let (mut connection, _) = connection();
    connection.config.generate_title = true;
    connection
        .send(ClientCommand::Prompt {
            text: "please look at the parser".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;

    let title = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(ClientEvent::Session(SessionEvent::Title(title))) =
                connection.next_event().await
            {
                break title;
            }
        }
    })
    .await
    .unwrap();

    assert_eq!(title.as_deref(), Some("hi back"));
    assert_eq!(connection.state().title.as_deref(), Some("hi back"));
}

#[tokio::test]
async fn compacting_a_short_conversation_is_refused() {
    let (mut connection, _) = connection();

    let outcome = connection
        .send(ClientCommand::Compact {
            focus: CompactionFocus::default(),
            auto: false,
        })
        .await;

    assert!(matches!(outcome, Err(ConnectionError::TooShortToCompact)));
    assert!(!connection.state().compacting);
}

#[tokio::test]
async fn stopping_a_child_reaches_the_family_and_names_what_is_missing() {
    let (mut connection, _) = connection();
    assert!(matches!(
        connection
            .send(ClientCommand::StopChild {
                session_id: "nobody".into(),
            })
            .await,
        Err(ConnectionError::Refused(_))
    ));

    connection.set_family(Some(crate::family::Family::new(
        crate::family::Identity::root("root"),
        "root".into(),
        2,
        None,
        None,
    )));
    let Err(ConnectionError::Refused(error)) = connection
        .send(ClientCommand::StopChild {
            session_id: "nobody".into(),
        })
        .await
    else {
        panic!("an unknown child is refused")
    };
    assert!(error.contains("nobody"), "{error}");
}

#[tokio::test]
async fn shutdown_closes_the_event_stream() {
    let (mut connection, _) = connection();

    connection.send(ClientCommand::Shutdown).await.unwrap();

    assert!(connection.next_event().await.is_none());
    assert!(matches!(
        connection.send(ClientCommand::Abort).await,
        Err(ConnectionError::Closed)
    ));
}

fn bash_done(command: &str, exit_code: i32) -> mush_tools::kernel_jobs::JobNotice {
    mush_tools::kernel_jobs::JobNotice {
        done: mush_tools::kernel_jobs::BashDone {
            pid: Some(77),
            exit_code,
            command: command.into(),
        },
        origin: None,
    }
}

#[tokio::test]
async fn a_kernel_notice_becomes_a_prompt_when_idle_and_steering_when_busy() {
    let (mut connection, _requests) = connection();
    let (sinks, signals) = crate::kernel_channels::kernel_channels();
    connection.attach_kernel(Some(signals));

    (sinks.notices)(bash_done("exit 5", 5));
    connection.tick().await;

    let events: Vec<_> = std::iter::from_fn(|| connection.try_next_event()).collect();
    assert!(
        matches!(&events[..], [ClientEvent::BashDone(notice), ClientEvent::TurnStarted] if notice.done.exit_code == 5),
        "{events:?}"
    );
    let transcript = connection.transcript().await;
    assert!(matches!(
        &transcript[0],
        Message::Event(event) if event.kind == mush_ai::types::EventKind::BashDone
            && event.text().starts_with("[bash-done pid:77 exit:5]")
            && event.receipts.len() == 1
    ));

    (sinks.notices)(bash_done("second", 0));
    connection.tick().await;

    let events: Vec<_> = std::iter::from_fn(|| connection.try_next_event()).collect();
    assert!(
        matches!(&events[..], [ClientEvent::BashDone(_)]),
        "{events:?}"
    );
    let queued = connection
        .runtime
        .turns
        .meta(PRIMARY_PANE)
        .expect("the turn is running")
        .steering_queue
        .lock()
        .await
        .clone();
    assert!(
        matches!(&queued[..], [Message::Event(event)] if event.kind == mush_ai::types::EventKind::BashDone),
        "{queued:?}"
    );
}

#[tokio::test]
async fn a_kernel_notice_during_a_compaction_waits_for_it_and_keeps_the_typed_prompt() {
    let (mut connection, release) = crate::testing::stalled_connection();
    let (sinks, signals) = crate::kernel_channels::kernel_channels();
    connection.attach_kernel(Some(signals));
    connection.runtime.conversation = ConversationState::from_messages(
        (0..30)
            .map(|i| user(&format!("turn {i}: {}", "x".repeat(400))))
            .collect(),
    );
    connection
        .send(ClientCommand::Compact {
            focus: CompactionFocus::default(),
            auto: false,
        })
        .await
        .unwrap();
    connection
        .send(ClientCommand::QueuePrompt {
            text: "after the compaction".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();

    (sinks.notices)(bash_done("make", 0));
    connection.tick().await;

    assert_eq!(connection.deferred_events.len(), 1);
    let pending = connection.runtime.pending_prompt.as_ref().unwrap();
    assert_eq!(pending.text, "after the compaction");
    drop(release);
}

#[tokio::test]
async fn a_busy_kernel_question_reaches_the_client_and_the_answer_reaches_the_tool() {
    use mush_tools::ipython::BusyKernelChoice;
    let (mut connection, _requests) = connection();
    let (sinks, signals) = crate::kernel_channels::kernel_channels();
    connection.attach_kernel(Some(signals));

    let answer = (sinks.busy_prompt)();
    assert!(matches!(
        connection.try_next_event(),
        Some(ClientEvent::KernelBusy)
    ));
    assert!(connection.try_next_event().is_none());

    connection
        .send(ClientCommand::AnswerKernelBusy(BusyKernelChoice::Kill))
        .await
        .unwrap();

    assert_eq!(answer.await, BusyKernelChoice::Kill);
}

#[tokio::test]
async fn an_aborted_cell_withdraws_its_busy_question() {
    let (mut connection, _requests) = connection();
    let (sinks, signals) = crate::kernel_channels::kernel_channels();
    connection.attach_kernel(Some(signals));

    let answer = (sinks.busy_prompt)();
    assert!(matches!(
        connection.try_next_event(),
        Some(ClientEvent::KernelBusy)
    ));
    drop(answer);

    assert!(matches!(
        connection.try_next_event(),
        Some(ClientEvent::KernelBusyWithdrawn)
    ));
    assert!(connection.try_next_event().is_none());
}

#[tokio::test]
async fn tool_output_lines_arrive_as_events() {
    let (mut connection, _requests) = connection();
    let (sinks, signals) = crate::kernel_channels::kernel_channels();
    connection.attach_kernel(Some(signals));

    (sinks.output)("tc_1", "building");
    (sinks.output)("tc_2", "testing");

    let events: Vec<_> = std::iter::from_fn(|| connection.try_next_event()).collect();
    assert!(
        matches!(
            &events[..],
            [
                ClientEvent::ToolOutput { exec_id: a, line: la },
                ClientEvent::ToolOutput { exec_id: b, line: lb },
            ] if a == "tc_1" && la == "building" && b == "tc_2" && lb == "testing"
        ),
        "{events:?}"
    );
}

async fn side_answer(connection: &mut InProcessConnection) -> (String, Result<String, String>) {
    match wait_for(connection, |event| {
        matches!(event, ClientEvent::SideAnswer { .. })
    })
    .await
    {
        ClientEvent::SideAnswer { question, answer } => (question, answer),
        other => panic!("expected a side answer, got {other:?}"),
    }
}

fn tool_names(context: &mush_ai::registry::LlmContext) -> Vec<(String, String)> {
    context
        .tools
        .iter()
        .map(|tool| (tool.name.clone(), tool.description.clone()))
        .collect()
}

#[tokio::test]
async fn a_side_question_is_answered_without_touching_the_conversation() {
    let (mut connection, requests) = connection();
    connection.runtime.system_prompt = Some("base prompt".into());
    connection
        .send(ClientCommand::Prompt {
            text: "hello".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;
    let before = connection.state().context_len;

    connection
        .send(ClientCommand::SideQuestion("why slow?".into()))
        .await
        .unwrap();
    let answer = side_answer(&mut connection).await;

    assert_eq!(answer.0, "why slow?");
    assert_eq!(answer.1, Ok("hi back".into()));
    assert_eq!(connection.state().context_len, before);
    let asked = requests.lock().unwrap();
    let (turn, side) = (&asked[0], &asked[1]);
    assert_eq!(side.system_prompt.as_deref(), Some("base prompt"));
    assert_eq!(side.system_prompt, turn.system_prompt);
    assert!(!side.tools.is_empty());
    assert_eq!(tool_names(side), tool_names(turn));
    assert_eq!(side.messages[..turn.messages.len()], turn.messages[..]);
    assert_eq!(side.messages.len(), before + 1);
    assert!(
        matches!(side.messages.last(), Some(Message::User(user)) if user.text().ends_with("why slow?"))
    );
}

#[tokio::test]
async fn a_side_question_during_a_tool_call_sees_the_call_still_running() {
    let (mut connection, requests) = crate::testing::hanging_tool_connection();
    connection
        .send(ClientCommand::Prompt {
            text: "start".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    wait_for(&mut connection, |event| {
        matches!(
            event,
            ClientEvent::Agent {
                event: AgentEvent::ToolExecStart { .. },
                ..
            }
        )
    })
    .await;

    connection
        .send(ClientCommand::SideQuestion("what's going on?".into()))
        .await
        .unwrap();
    let (_, answer) = side_answer(&mut connection).await;

    assert_eq!(answer, Ok("done".into()));
    let asked = requests.lock().unwrap();
    let side = asked.last().unwrap();
    assert_eq!(tool_names(side), tool_names(&asked[0]));
    assert!(
        matches!(
            &side.messages[..],
            [
                Message::User(_),
                Message::Assistant(_),
                Message::ToolResult(running),
                Message::User(question),
            ] if running.outcome.is_success()
                && running.content.iter().any(|part| matches!(
                    part,
                    mush_ai::types::ToolResultContentPart::Text(text) if text.text.starts_with("still running")
                ))
                && question.text().ends_with("what's going on?")
        ),
        "{:?}",
        side.messages
    );
}

#[tokio::test]
async fn reload_rebuilds_the_system_prompt_through_the_worker_builder() {
    let (mut connection, _requests) = connection();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    connection.set_system_prompt_builder(Some(Arc::new({
        let calls = Arc::clone(&calls);
        move || {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            format!("prompt v{n}")
        }
    })));

    let events = connection
        .send(ClientCommand::ReloadSystemPrompt)
        .await
        .unwrap();

    assert_eq!(
        connection.runtime.system_prompt.as_deref(),
        Some("prompt v1")
    );
    assert!(matches!(
        &events[..],
        [ClientEvent::Session(SessionEvent::SystemPromptChanged(Some(prompt)))]
            if prompt == "prompt v1"
    ));
}

#[tokio::test]
async fn a_goal_keeps_prompting_until_the_kernel_completes_it() {
    let (mut connection, requests) = connection();
    let goal = Arc::new(crate::goal::GoalStore::new(None));
    connection.set_goal_store(Some(Arc::clone(&goal)));

    let events = connection
        .send(ClientCommand::Goal(crate::goal::GoalCommand::Create {
            objective: "answer twice".into(),
            token_budget: None,
        }))
        .await
        .unwrap();
    assert!(matches!(
        &events[..],
        [ClientEvent::GoalChanged(Some(goal))] if goal.objective == "answer twice"
    ));

    connection.start_pending().await;
    assert!(connection.state().turn_active);
    drain_turn(&mut connection).await;
    assert!(matches!(
        &requests.lock().unwrap()[0].messages[0],
        Message::User(user) if user.text().starts_with("[goal: continuation 1]")
    ));

    connection.start_pending().await;
    assert!(
        connection.state().turn_active,
        "the goal continues after a turn"
    );
    drain_turn(&mut connection).await;
    assert_eq!(goal.get().unwrap().continuations, 2);

    goal.apply(crate::goal::GoalCommand::Complete {
        summary: Some("both answered".into()),
    })
    .unwrap();
    connection.start_pending().await;
    assert!(!connection.state().turn_active);
    assert!(matches!(
        connection.try_next_event(),
        Some(ClientEvent::GoalChanged(Some(goal))) if goal.status == crate::goal::GoalStatus::Complete
    ));
}

struct BusyKernel(std::sync::atomic::AtomicBool);

#[async_trait::async_trait]
impl mush_kernel::KernelState for BusyKernel {
    fn background_work(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

struct RestoringKernel {
    outcome: Option<Result<mush_kernel::RestoreReport, String>>,
    gate: tokio::sync::Notify,
    restores: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl mush_kernel::KernelState for RestoringKernel {
    async fn restore_from(
        &self,
        _path: &std::path::Path,
    ) -> Option<Result<mush_kernel::RestoreReport, String>> {
        self.restores
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.gate.notified().await;
        self.outcome.clone()
    }
}

fn restoring_kernel(outcome: Result<mush_kernel::RestoreReport, String>) -> Arc<RestoringKernel> {
    Arc::new(RestoringKernel {
        outcome: Some(outcome),
        gate: tokio::sync::Notify::new(),
        restores: std::sync::atomic::AtomicUsize::new(0),
    })
}

fn resumed_worker(
    kernel: &Arc<RestoringKernel>,
    snapshot: bool,
) -> (InProcessConnection, crate::testing::Requests) {
    let requests: crate::testing::Requests = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(crate::testing::Echo {
        requests: requests.clone(),
        reply: "ok".into(),
    }));
    let mut worker =
        crate::WorkerConfig::new(crate::testing::echo_config(crate::testing::echo_model()));
    if snapshot {
        let path = mush_session::SessionStore::kernel_state_path(&worker.session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"snapshot").unwrap();
    }
    worker.kernel_state = Some(Arc::clone(kernel) as Arc<dyn mush_kernel::KernelState>);
    (worker.connect(&ToolRegistry::new(), &registry), requests)
}

fn user_texts(context: &mush_ai::registry::LlmContext) -> Vec<String> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(user.text()),
            _ => None,
        })
        .collect()
}

async fn prompt_after_restore(
    connection: &mut InProcessConnection,
    kernel: &RestoringKernel,
    text: &str,
) -> String {
    connection
        .send(ClientCommand::Prompt {
            text: text.into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    let waited = !connection.state().turn_active;
    kernel.gate.notify_one();
    let ClientEvent::Session(SessionEvent::Status(status)) = wait_for(connection, |event| {
        matches!(event, ClientEvent::Session(SessionEvent::Status(_)))
    })
    .await
    else {
        unreachable!()
    };
    connection.start_pending().await;
    drain_turn(connection).await;
    assert!(waited, "the turn waits for the restore");
    status
}

#[tokio::test]
async fn a_resumed_kernel_names_what_came_back_once_before_the_first_request() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let kernel = restoring_kernel(Ok(mush_kernel::RestoreReport {
        restored: vec!["df".into(), "helper".into()],
        failed: vec!["sock".into()],
    }));
    let (mut connection, requests) = resumed_worker(&kernel, true);

    let status = prompt_after_restore(&mut connection, &kernel, "carry on").await;
    connection
        .send(ClientCommand::Prompt {
            text: "and again".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;

    let requests = requests.lock().unwrap().clone();
    let first = user_texts(&requests[0]);
    assert_eq!(
        first,
        [
            format!(
                "[python-state-restored]\n\nYour Python kernel state was revived from your \
                 previous session. These names are available again: df, helper.\nThese could \
                 not be restored and must be recreated if needed: sock.\n{}",
                mush_kernel::RESTART_RESETS
            ),
            "carry on".to_string()
        ]
    );
    let notices = |texts: &[String]| {
        texts
            .iter()
            .filter(|text| crate::kernel_notice::is_notice(text))
            .count()
    };
    assert_eq!(notices(&user_texts(&requests[1])), 1);
    assert_eq!(status, "python state restored");
    assert_eq!(kernel.restores.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_failed_resume_restore_tells_the_model_the_kernel_starts_fresh() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let kernel = restoring_kernel(Err("kernel exited".into()));
    let (mut connection, requests) = resumed_worker(&kernel, true);

    let status = prompt_after_restore(&mut connection, &kernel, "carry on").await;

    let first = user_texts(&requests.lock().unwrap()[0]);
    assert_eq!(
        first,
        [
            format!(
                "[python-state-restored]\n\nYour previous Python kernel state could not be \
                 revived; the kernel is starting fresh, so re-create any variables, imports, or \
                 loaded data you need.\n{}",
                mush_kernel::RESTART_RESETS
            ),
            "carry on".to_string()
        ]
    );
    assert_eq!(status, "python state could not be restored");
}

#[tokio::test]
async fn a_resume_without_a_snapshot_starts_at_once_with_no_notice() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let kernel = restoring_kernel(Ok(mush_kernel::RestoreReport::default()));
    let (mut connection, requests) = resumed_worker(&kernel, false);

    connection
        .send(ClientCommand::Prompt {
            text: "fresh".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    assert!(connection.state().turn_active);
    drain_turn(&mut connection).await;

    assert_eq!(user_texts(&requests.lock().unwrap()[0]), ["fresh"]);
    assert_eq!(kernel.restores.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_errored_turn_fails_the_goal_and_stops_autonomous_mode() {
    let (mut connection, _requests) = connection();
    connection.set_deps(ToolRegistry::new(), ApiRegistry::new());
    let goal = Arc::new(crate::goal::GoalStore::new(None));
    connection.set_goal_store(Some(Arc::clone(&goal)));
    let autonomous = Arc::new(crate::autonomous::Autonomous::new(std::env::temp_dir()));
    connection.set_autonomous(Some(Arc::clone(&autonomous)));
    connection
        .send(ClientCommand::Autonomous(
            crate::autonomous::AutonomousCommand::On {
                limits: crate::autonomous::Limits::unlimited(),
                gates: crate::autonomous::Gates::default(),
                subagent_keep_alive_ms: 0,
            },
        ))
        .await
        .unwrap();
    connection
        .send(ClientCommand::Goal(crate::goal::GoalCommand::Create {
            objective: "never reachable".into(),
            token_budget: None,
        }))
        .await
        .unwrap();

    connection.start_pending().await;
    assert!(connection.state().turn_active);
    drain_turn(&mut connection).await;

    let failed = goal.get().unwrap();
    assert_eq!(failed.status, crate::goal::GoalStatus::Failed);
    assert!(failed.error.is_some());
    assert!(!autonomous.due());
    connection.start_pending().await;
    assert!(!connection.state().turn_active);
}

#[tokio::test]
async fn a_child_change_after_the_family_went_quiet_is_still_reported() {
    let (mut connection, _requests) = connection();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("subagents.json");
    std::fs::write(
        &path,
        serde_json::json!({"children": [{
            "id": "sub-1", "name": "left", "parent": null, "depth": 1,
            "session_id": "s", "model_id": "m", "status": "idle",
            "spawned_at": 0, "updated_at": 0, "turns": 1,
            "usage": mush_ai::types::Usage::default(),
            "cost": mush_ai::types::Cost::default(),
        }]})
        .to_string(),
    )
    .unwrap();
    let family = crate::family::Family::new(
        crate::family::Identity::root("root"),
        "root".into(),
        2,
        None,
        Some(path),
    );
    connection.set_family(Some(Arc::clone(&family)));

    let updated = |event: Option<ClientEvent>| match event {
        Some(ClientEvent::ChildUpdated(child)) => Some(*child),
        _ => None,
    };
    assert_eq!(
        updated(connection.try_next_event()).map(|child| child.id),
        Some("sub-1".into())
    );
    assert!(updated(connection.try_next_event()).is_none());

    let child = crate::family::Identity {
        id: Some("sub-1".into()),
        name: "left".into(),
        depth: 1,
    };
    family.progress_note(&child, "halfway").unwrap();

    assert_eq!(
        updated(connection.try_next_event()).and_then(|child| child.progress_note),
        Some("halfway".into())
    );
    assert!(updated(connection.try_next_event()).is_none());
}

#[tokio::test]
async fn setting_the_max_depth_reaches_the_family_and_the_system_prompt() {
    let (mut connection, _requests) = connection();
    let family = crate::family::Family::new(
        crate::family::Identity::root("root"),
        "root".into(),
        2,
        None,
        None,
    );
    connection.set_family(Some(Arc::clone(&family)));
    connection.set_system_prompt_builder(Some(Arc::new({
        let family = Arc::clone(&family);
        move || format!("max depth {}", family.max_depth())
    })));

    connection
        .send(ClientCommand::SetRlmMaxDepth(3))
        .await
        .unwrap();
    assert_eq!(family.max_depth(), 3);
    assert!(connection.runtime.system_prompt.is_none());

    let events = connection
        .send(ClientCommand::SetRlmMaxDepth(0))
        .await
        .unwrap();
    assert_eq!(family.max_depth(), 0);
    assert_eq!(
        connection.runtime.system_prompt.as_deref(),
        Some("max depth 0")
    );
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::Session(SessionEvent::SystemPromptChanged(Some(_)))
    )));
    assert_eq!(
        connection.state().rlm_max_depth_source,
        crate::session::RlmMaxDepthSource::Chat
    );

    connection
        .send(ClientCommand::Session(SessionCommand::New))
        .await
        .unwrap();
    assert_eq!(family.max_depth(), 2);
    assert_eq!(connection.state().rlm_max_depth, 2);
    assert_eq!(
        connection.state().rlm_max_depth_source,
        crate::session::RlmMaxDepthSource::Default
    );
    assert_eq!(
        connection.runtime.system_prompt.as_deref(),
        Some("max depth 2")
    );
}

struct ModelKernel(std::sync::Mutex<Vec<(String, bool)>>);

#[async_trait::async_trait]
impl mush_kernel::KernelState for ModelKernel {
    fn set_model(&self, id: &str, accepts_images: bool) {
        self.0
            .lock()
            .unwrap()
            .push((id.to_string(), accepts_images));
    }
}

#[tokio::test]
async fn a_turn_tells_the_kernel_which_model_it_runs_on() {
    let (mut connection, _requests) = connection();
    let kernel = Arc::new(ModelKernel(std::sync::Mutex::new(Vec::new())));
    connection.runtime.set_kernel_state(Some(
        Arc::clone(&kernel) as Arc<dyn mush_kernel::KernelState>
    ));
    let model = connection.runtime.model(&connection.config.default_model);

    connection
        .send(ClientCommand::Prompt {
            text: "hello".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;

    assert_eq!(
        *kernel.0.lock().unwrap(),
        [(
            model.id.to_string(),
            model.input.contains(&mush_ai::types::InputModality::Image)
        )]
    );
}

#[tokio::test]
async fn a_goal_holds_while_background_bash_runs() {
    let (mut connection, _requests) = connection();
    let kernel = Arc::new(BusyKernel(std::sync::atomic::AtomicBool::new(true)));
    connection.runtime.set_kernel_state(Some(
        Arc::clone(&kernel) as Arc<dyn mush_kernel::KernelState>
    ));
    let goal = Arc::new(crate::goal::GoalStore::new(None));
    connection.set_goal_store(Some(Arc::clone(&goal)));
    connection
        .send(ClientCommand::Goal(crate::goal::GoalCommand::Create {
            objective: "wait for the build".into(),
            token_budget: None,
        }))
        .await
        .unwrap();

    connection.start_pending().await;
    assert!(!connection.state().turn_active);
    assert_eq!(goal.get().unwrap().continuations, 0);

    kernel.0.store(false, std::sync::atomic::Ordering::SeqCst);
    connection.start_pending().await;
    assert!(connection.state().turn_active);
    assert_eq!(goal.get().unwrap().continuations, 1);
}

#[tokio::test]
async fn a_due_heartbeat_becomes_a_prompt_when_idle_and_a_steer_when_busy() {
    let (mut connection, requests) = connection();
    let scheduler = Arc::new(crate::schedule::Scheduler::new(None));
    connection.set_scheduler(Some(Arc::clone(&scheduler)));
    let events = connection
        .send(ClientCommand::Schedule(
            crate::schedule::ScheduleCommand::Heartbeat {
                expression: "every 10s".into(),
                prompt: "check the build".into(),
                delivery: crate::schedule::Delivery::Steer,
            },
        ))
        .await
        .unwrap();
    assert!(matches!(&events[..], [ClientEvent::ScheduleChanged(jobs)] if jobs.len() == 1));

    connection.start_pending().await;
    assert!(!connection.state().turn_active, "nothing is due yet");

    let far = mush_ai::types::Timestamp::now().as_ms() + 11_000;
    let due = scheduler.take_due(far);
    assert_eq!(due.len(), 1);
    connection.deferred_events.push_back(due[0].event());
    connection.start_pending().await;
    assert!(connection.state().turn_active);
    let injected = std::iter::from_fn(|| connection.try_next_event()).find(|event| {
        matches!(event, ClientEvent::MessageInjected(event) if event.kind == mush_ai::types::EventKind::Schedule)
    });
    assert!(injected.is_some());
    drain_turn(&mut connection).await;
    assert!(matches!(
        &requests.lock().unwrap()[0].messages[0],
        Message::Event(event) if event.kind == mush_ai::types::EventKind::Schedule
            && event.text() == "[heartbeat: every 10s run#1]\n\ncheck the build"
    ));

    connection
        .send(ClientCommand::Schedule(
            crate::schedule::ScheduleCommand::HeartbeatClear,
        ))
        .await
        .unwrap();
    assert!(scheduler.heartbeat().is_none());
    assert!(matches!(
        connection
            .send(ClientCommand::Schedule(
                crate::schedule::ScheduleCommand::HeartbeatPause
            ))
            .await,
        Err(ConnectionError::Refused(_))
    ));
}

#[tokio::test]
async fn a_new_session_rebinds_the_goal_and_schedule_files() {
    let dir = tempfile::tempdir().unwrap();
    let (mut connection, _requests) = connection();
    let goal = Arc::new(crate::goal::GoalStore::new(Some(
        dir.path().join("a/goal.json"),
    )));
    goal.apply(crate::goal::GoalCommand::Create {
        objective: "old".into(),
        token_budget: None,
    })
    .unwrap();
    connection.set_goal_store(Some(Arc::clone(&goal)));
    connection.runtime.saver = Some(Arc::new(|_| {}));

    connection
        .send(ClientCommand::Session(SessionCommand::New))
        .await
        .unwrap();

    assert!(
        goal.get().is_none(),
        "the new session starts without a goal"
    );
    assert!(dir.path().join("a/goal.json").exists());
}

struct ToolCaller;

#[async_trait::async_trait]
impl mush_ai::registry::ApiProvider for ToolCaller {
    fn api(&self) -> Api {
        Api::AnthropicMessages
    }

    async fn stream(
        &self,
        model: &mush_ai::types::Model,
        context: &mush_ai::registry::LlmContext,
        _options: &mush_ai::types::StreamOptions,
    ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
        let already_called = context
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_)));
        let content = if already_called {
            vec![mush_ai::types::AssistantContentPart::Text(
                mush_ai::types::TextContent {
                    text: "done".into(),
                },
            )]
        } else {
            vec![mush_ai::types::AssistantContentPart::ToolCall(
                mush_ai::types::ToolCall {
                    id: mush_ai::types::ToolCallId::from("call_block"),
                    name: mush_ai::types::ToolName::from("block"),
                    arguments: serde_json::json!({}),
                },
            )]
        };
        let message = AssistantMessage::builder()
            .content(content)
            .model(model.id.clone())
            .provider(model.provider.clone())
            .api(model.api)
            .stop_reason(if already_called {
                StopReason::Stop
            } else {
                StopReason::ToolUse
            })
            .timestamp_ms(Timestamp::zero())
            .build();
        Ok(Box::pin(futures::stream::iter([
            mush_ai::stream::StreamEvent::Done {
                reason: message.stop_reason,
                message,
            },
        ])))
    }
}

struct Blocker;

#[async_trait::async_trait]
impl mush_agent::tool::AgentTool for Blocker {
    fn name(&self) -> &str {
        "block"
    }
    fn label(&self) -> &str {
        "block"
    }
    fn description(&self) -> &str {
        "never returns"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: serde_json::Value) -> mush_agent::tool::ToolResult {
        std::future::pending().await
    }
    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Head
    }
}

#[tokio::test]
async fn abort_returns_while_a_tool_is_still_running() {
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(ToolCaller));
    let model = crate::testing::echo_model();
    let mut runtime = SessionRuntime::new(ConversationState::new());
    runtime.model_id = model.id.to_string();
    let mut connection = InProcessConnection::new(
        runtime,
        ToolRegistry::from_shared(vec![Arc::new(Blocker) as mush_agent::tool::SharedTool]),
        registry,
        crate::testing::echo_config(model),
        mush_agent::AgentInjections::default(),
    );
    connection
        .send(ClientCommand::Prompt {
            text: "block".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match connection.next_event().await {
                Some(ClientEvent::Agent {
                    event: AgentEvent::ToolExecStart { .. },
                    ..
                }) => break,
                Some(_) => {}
                None => panic!("closed"),
            }
        }
    })
    .await
    .expect("the tool starts");

    let aborted = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        connection.send(ClientCommand::Abort),
    )
    .await
    .expect("abort returns while the tool blocks")
    .unwrap();
    assert!(matches!(
        &aborted[..],
        [ClientEvent::ToolsCancelled(ids), ClientEvent::TurnEnded { cancelled: true }] if ids.len() == 1
    ));
    let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while connection.state().turn_active {
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(50),
                connection.next_event(),
            )
            .await;
        }
    })
    .await;
    assert!(settled.is_ok(), "the aborted turn winds down");
}

#[tokio::test]
async fn the_notes_ride_in_the_conversation_and_not_the_system_prompt() {
    let (mut connection, requests) = connection();
    connection.runtime.system_prompt = Some("base".into());
    let notes = crate::notes::Notes::new(None);
    connection.set_notes(Some(Arc::clone(&notes)));
    notes.append("remember the socket path").unwrap();
    assert_eq!(connection.system_prompt().await.as_deref(), Some("base"));

    connection
        .send(ClientCommand::Prompt {
            text: "first".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;
    let turn = requests.lock().unwrap()[0].messages.clone();
    let Message::User(carried) = &turn[0] else {
        panic!("{turn:?}")
    };
    let (fingerprint, body) = crate::notes::extract(&carried.text()).unwrap();
    assert_eq!(fingerprint, crate::notes::fingerprint(&notes.read()));
    assert!(body.ends_with("- [n1] remember the socket path"));
    assert!(matches!(&turn[1], Message::User(u) if u.text() == "first"));

    connection
        .send(ClientCommand::Prompt {
            text: "second".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;
    let turn = requests.lock().unwrap()[1].messages.clone();
    assert_eq!(
        turn.iter()
            .filter(|message| matches!(message, Message::User(u) if crate::notes::extract(&u.text()).is_some()))
            .count(),
        1,
        "unchanged notes are not re-delivered"
    );

    notes.append("and the pid file").unwrap();
    connection
        .send(ClientCommand::Prompt {
            text: "third".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;
    let turn = requests.lock().unwrap()[2].messages.clone();
    assert_eq!(
        turn.iter()
            .filter(|message| matches!(message, Message::User(u) if crate::notes::extract(&u.text()).is_some()))
            .count(),
        2,
        "a changed note lands at the tail"
    );
    let Message::User(latest) = &turn[turn.len() - 2] else {
        panic!("{turn:?}")
    };
    let (fingerprint, body) = crate::notes::extract(&latest.text()).unwrap();
    assert_eq!(fingerprint, crate::notes::fingerprint(&notes.read()));
    assert!(body.ends_with("- [n2] and the pid file"));
    assert!(!body.contains("socket path"), "only the change is re-shown");
}

#[tokio::test]
async fn autonomous_mode_continues_until_its_limit_and_reports_the_stop() {
    let (mut connection, requests) = connection();
    let autonomous = Arc::new(crate::autonomous::Autonomous::new(std::env::temp_dir()));
    connection.set_autonomous(Some(Arc::clone(&autonomous)));
    connection
        .send(ClientCommand::Autonomous(
            crate::autonomous::AutonomousCommand::On {
                limits: crate::autonomous::Limits {
                    max_continuations: Some(1),
                    ..crate::autonomous::Limits::unlimited()
                },
                gates: crate::autonomous::Gates::default(),
                subagent_keep_alive_ms: 0,
            },
        ))
        .await
        .unwrap();
    connection
        .send(ClientCommand::Prompt {
            text: "start".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;

    connection.start_pending().await;
    assert!(
        connection.state().turn_active,
        "one continuation follows the first turn"
    );
    drain_turn(&mut connection).await;
    assert!(matches!(
        &requests.lock().unwrap()[1].messages.last(),
        Some(Message::User(user)) if user.text().starts_with("[autonomous-continuation 1]")
    ));

    connection.start_pending().await;
    assert!(!connection.state().turn_active, "the limit stops the run");
    let stopped =
        std::iter::from_fn(|| connection.try_next_event()).find_map(|event| match event {
            ClientEvent::AutonomousChanged(status) if status.stopped.is_some() => Some(status),
            _ => None,
        });
    assert_eq!(
        stopped.unwrap().stopped.as_deref(),
        Some("1 continuations used")
    );
}

#[tokio::test]
async fn a_failing_gate_becomes_the_next_prompt_and_a_passing_one_ends_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let (mut connection, requests) = connection();
    let autonomous = Arc::new(crate::autonomous::Autonomous::new(dir.path().to_path_buf()));
    connection.set_autonomous(Some(Arc::clone(&autonomous)));
    connection
        .send(ClientCommand::Autonomous(
            crate::autonomous::AutonomousCommand::On {
                limits: crate::autonomous::Limits::unlimited(),
                gates: crate::autonomous::Gates {
                    commands: vec!["test -f done".into()],
                    max_retries: 3,
                    timeout_ms: 5_000,
                },
                subagent_keep_alive_ms: 0,
            },
        ))
        .await
        .unwrap();
    connection
        .send(ClientCommand::Prompt {
            text: "start".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !connection.state().turn_active {
        assert!(
            std::time::Instant::now() < deadline,
            "the gate failure prompts a continuation"
        );
        connection.start_pending().await;
        let _ = connection.try_next_event();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    drain_turn(&mut connection).await;
    assert!(matches!(
        &requests.lock().unwrap()[1].messages.last(),
        Some(Message::User(user)) if user.text().starts_with("[autonomous-continuation: gate failed]")
            && user.text().contains("`test -f done` exited with 1")
    ));

    std::fs::write(dir.path().join("done"), "").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        connection.start_pending().await;
        let stopped =
            std::iter::from_fn(|| connection.try_next_event()).find_map(|event| match event {
                ClientEvent::AutonomousChanged(status) if status.stopped.is_some() => Some(status),
                _ => None,
            });
        if let Some(status) = stopped {
            assert_eq!(status.stopped.as_deref(), Some("all gates passed"));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the passing gate ends the run"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!connection.state().turn_active);
}

#[tokio::test]
async fn compact_status_measures_against_the_window_the_compactor_uses() {
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(crate::testing::Echo {
        requests: Arc::new(Mutex::new(Vec::new())),
        reply: "hi".into(),
    }));
    let mut model = crate::testing::echo_model();
    model.id = "test/effective-window".into();
    model.effective_context_window = Some(TokenCount::new(100_000));
    let mut runtime = SessionRuntime::new(ConversationState::new());
    runtime.model_id = model.id.to_string();
    let mut connection = InProcessConnection::new(
        runtime,
        ToolRegistry::new(),
        registry,
        crate::testing::echo_config(model),
        mush_agent::AgentInjections::default(),
    );
    let commands = Arc::new(HostCommands::default());
    connection.set_host_commands(Some(Arc::clone(&commands)));

    connection
        .send(ClientCommand::Prompt {
            text: "hello".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;

    assert_eq!(commands.context_usage().context_window, Some(100_000));
}

#[tokio::test]
async fn a_host_command_runs_once_the_turn_is_over() {
    let (mut connection, _requests) = connection();
    let commands = Arc::new(HostCommands::default());
    connection.set_host_commands(Some(Arc::clone(&commands)));
    connection
        .send(ClientCommand::Prompt {
            text: "hello".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    commands.push(ClientCommand::Compact {
        focus: CompactionFocus::default(),
        auto: false,
    });
    connection.start_pending().await;
    assert_eq!(commands.pending(), 1, "held while the turn runs");

    drain_turn(&mut connection).await;
    connection.start_pending().await;

    assert_eq!(commands.pending(), 0);
    let status = std::iter::from_fn(|| connection.try_next_event()).find_map(|event| match event {
        ClientEvent::Session(SessionEvent::Status(text)) => Some(text),
        _ => None,
    });
    assert_eq!(status.as_deref(), Some("conversation too short to compact"));
}

#[tokio::test]
async fn a_refused_compaction_from_the_agent_lands_in_the_conversation() {
    let (mut connection, _requests) = connection();
    let commands = Arc::new(HostCommands::default());
    connection.set_host_commands(Some(Arc::clone(&commands)));
    connection.runtime.conversation =
        ConversationState::from_messages(vec![user("one"), user("two")]);

    commands.push(ClientCommand::Compact {
        focus: mush_session::compact::CompactionFocus::default(),
        auto: false,
    });
    connection.run_host_commands().await;

    let last = connection.runtime.conversation.context().pop().unwrap();
    let Message::User(message) = last else {
        panic!("expected the outcome as a user message")
    };
    let text = message.text();
    assert!(text.starts_with(mush_session::compact::COMPACTION_OUTCOME_PREFIX));
    assert!(text.contains("skipped"), "{text}");
    assert!(text.contains("too short"), "{text}");
}

#[tokio::test]
async fn a_second_compaction_request_in_one_turn_is_refused() {
    let commands = HostCommands::default();
    assert!(!commands.compaction_queued());
    commands.push(ClientCommand::Compact {
        focus: mush_session::compact::CompactionFocus::default(),
        auto: false,
    });
    assert!(commands.compaction_queued());
}

fn a_pause() -> mush_session::Pause {
    mush_session::Pause {
        at: mush_ai::types::Timestamp::now(),
        reason: Some("rebuilding".into()),
        by: Some("cli".into()),
    }
}

#[tokio::test]
async fn a_pause_carried_by_the_worker_config_reaches_the_runtime() {
    let mut worker =
        crate::WorkerConfig::new(crate::testing::echo_config(crate::testing::echo_model()));
    worker.paused = Some(a_pause());
    let connection = worker.connect(
        &mush_agent::ToolRegistry::new(),
        &mush_ai::registry::ApiRegistry::new(),
    );
    assert!(connection.state().paused.is_some());
}

#[tokio::test]
async fn a_resumed_worker_restores_the_thinking_level_its_branch_ran_with() {
    use mush_ai::types::ThinkingLevel;
    let model = crate::testing::echo_model();
    let connect = |conversation: ConversationState| {
        let mut worker = crate::WorkerConfig::new(crate::testing::echo_config(model.clone()));
        worker.conversation = conversation;
        worker.thinking_level = ThinkingLevel::High;
        worker.connect(
            &mush_agent::ToolRegistry::new(),
            &mush_ai::registry::ApiRegistry::new(),
        )
    };

    let mut recorded = ConversationState::from_messages(vec![user("go")]);
    recorded.record_thinking_level(&ThinkingLevel::Low);
    assert_eq!(
        connect(recorded).state().thinking_level,
        model
            .clamp_thinking(&ThinkingLevel::Low)
            .normalize_visible()
    );
    assert_eq!(
        connect(ConversationState::from_messages(vec![user("go")]))
            .state()
            .thinking_level,
        ThinkingLevel::High
    );
}

#[tokio::test]
async fn a_paused_agent_holds_a_queued_prompt_until_it_resumes() {
    let (mut connection, _requests) = connection();
    connection
        .send(ClientCommand::SetPaused(Some(a_pause())))
        .await
        .unwrap();

    connection
        .send(ClientCommand::QueuePrompt {
            text: "work".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    assert!(!connection.start_pending().await);
    assert!(!connection.turn_active());
    assert!(connection.runtime.pending_prompt.is_some());

    connection
        .send(ClientCommand::SetPaused(None))
        .await
        .unwrap();
    assert!(connection.start_pending().await);
    assert!(connection.turn_active());
}

#[tokio::test]
async fn a_session_taken_over_elsewhere_stops_its_turn_and_refuses_new_work() {
    let (mut connection, _requests) = connection();
    let (sync, receiver) = tokio::sync::watch::channel(mush_session::SyncState::default());
    connection.follow_sync(receiver);
    connection
        .send(ClientCommand::Prompt {
            text: "go".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    assert!(connection.turn_active());

    sync.send_replace(mush_session::SyncState {
        taken_by: Some("esme".into()),
        problem: None,
    });
    let noticed = connection.try_next_event();
    assert!(
        matches!(&noticed, Some(ClientEvent::SyncChanged(state)) if state.taken_by.as_deref() == Some("esme")),
        "{noticed:?}"
    );
    assert!(!connection.start_pending().await);
    assert!(!connection.turn_active());
    assert_eq!(connection.state().sync.taken_by.as_deref(), Some("esme"));

    let refused = connection
        .send(ClientCommand::Prompt {
            text: "more".into(),
            images: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("read-only"), "{refused}");
    assert!(
        connection
            .send(ClientCommand::SetModel("other".into()))
            .await
            .is_err()
    );
    assert!(connection.send(ClientCommand::Abort).await.is_ok());
    assert!(!connection.turn_active());
}

#[tokio::test]
async fn a_sync_problem_reaches_the_client_without_stopping_work() {
    let (mut connection, _requests) = connection();
    let (sync, receiver) = tokio::sync::watch::channel(mush_session::SyncState::default());
    connection.follow_sync(receiver);
    sync.send_replace(mush_session::SyncState {
        taken_by: None,
        problem: Some("the sync server is unreachable".into()),
    });
    assert!(matches!(
        connection.try_next_event(),
        Some(ClientEvent::SyncChanged(state)) if state.problem.is_some()
    ));
    assert!(connection.try_next_event().is_none());
    connection
        .send(ClientCommand::Prompt {
            text: "still works".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    assert!(connection.turn_active());
}

#[tokio::test]
async fn a_pause_reaches_the_client_and_a_prompt_lifts_it() {
    let (mut connection, _requests) = connection();
    let events = connection
        .send(ClientCommand::SetPaused(Some(a_pause())))
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::PausedChanged(Some(_)))),
        "{events:?}"
    );
    assert!(connection.state().paused.is_some());

    let events = connection
        .send(ClientCommand::Prompt {
            text: "go".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ClientEvent::PausedChanged(None))),
        "{events:?}"
    );
    assert!(connection.state().paused.is_none());
    assert!(connection.turn_active());
}

#[tokio::test]
async fn the_session_account_is_checked_against_the_logins_and_saved() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let work = mush_ai::types::AccountLabel::parse("work").unwrap();
    let (mut connection, _requests) = connection();

    let refused = connection
        .send(ClientCommand::SetAccount(Some(work.clone())))
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("run /login"), "{refused}");

    let creds = || mush_ai::oauth::OAuthCredentials {
        access_token: "access".into(),
        refresh_token: "refresh".into(),
        expires_at: u64::MAX,
        account_id: None,
    };
    mush_ai::oauth::save_login("anthropic", None, creds()).unwrap();
    mush_ai::oauth::save_login("anthropic", Some(work.clone()), creds()).unwrap();

    let unknown = connection
        .send(ClientCommand::SetAccount(Some(
            mush_ai::types::AccountLabel::parse("nope").unwrap(),
        )))
        .await
        .unwrap_err();
    assert!(unknown.to_string().contains("default, work"), "{unknown}");

    connection
        .send(ClientCommand::SetAccount(Some(work.clone())))
        .await
        .unwrap();
    let state = connection.state();
    assert_eq!(state.accounts.get("anthropic"), Some(&work));
    let model = connection.runtime.model(&connection.config.default_model);
    assert_eq!(connection.runtime.account_for(&model), Some(work.clone()));
    assert_eq!(
        connection.runtime.models.accounts().get("anthropic"),
        Some(&work),
        "children inherit the switch"
    );
    assert_eq!(
        connection.runtime.snapshot().accounts.get("anthropic"),
        Some(&work)
    );
}

#[tokio::test]
async fn the_subagent_model_is_session_state_and_reaches_the_client() {
    let (mut connection, _requests) = connection();
    assert!(connection.state().subagent_model.is_none());
    assert!(connection.runtime.models.for_child(None).is_none());

    connection
        .send(ClientCommand::SetSubagentModel(Some("cheap".into())))
        .await
        .unwrap();

    assert_eq!(connection.state().subagent_model.as_deref(), Some("cheap"));
    assert_eq!(
        connection.runtime.models.for_child(None).as_deref(),
        Some("cheap")
    );
    assert_eq!(
        connection
            .runtime
            .models
            .for_child(Some("named"))
            .as_deref(),
        Some("named"),
        "a spawn that names a model still wins"
    );

    connection
        .send(ClientCommand::SetSubagentModel(None))
        .await
        .unwrap();
    assert!(connection.state().subagent_model.is_none());
    assert!(connection.runtime.models.for_child(None).is_none());
}

#[tokio::test]
async fn changing_the_session_model_moves_what_a_spawn_inherits() {
    let (mut connection, _requests) = connection();
    connection.runtime.models.set_current("at-start".into());

    connection
        .send(ClientCommand::SetModel("now".into()))
        .await
        .unwrap();

    assert_eq!(connection.runtime.models.current(), "now");
}

struct Window {
    requests: crate::testing::Requests,
    chars_per_token: usize,
    limit: usize,
}

impl Window {
    fn tokens(&self, context: &mush_ai::registry::LlmContext) -> usize {
        let system = context.system_prompt.as_deref().map_or(0, str::len);
        let tools: usize = context
            .tools
            .iter()
            .map(|tool| {
                tool.name.len() + tool.description.len() + tool.parameters.to_string().len()
            })
            .sum();
        let messages: usize = context
            .messages
            .iter()
            .map(|message| serde_json::to_string(message).map_or(0, |text| text.len()))
            .sum();
        (system + tools + messages) / self.chars_per_token
    }
}

#[async_trait::async_trait]
impl mush_ai::registry::ApiProvider for Window {
    fn api(&self) -> Api {
        Api::AnthropicMessages
    }

    async fn stream(
        &self,
        model: &mush_ai::types::Model,
        context: &mush_ai::registry::LlmContext,
        _options: &mush_ai::types::StreamOptions,
    ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
        self.requests.lock().unwrap().push(context.clone());
        let tokens = self.tokens(context);
        if tokens > self.limit {
            return Err(mush_ai::registry::ProviderError::Other(format!(
                "anthropic returned 400 Bad Request: prompt is too long: {tokens} tokens > {} maximum",
                self.limit
            )));
        }
        let message = AssistantMessage::builder()
            .content(vec![AssistantContentPart::Text(TextContent {
                text: "ok".into(),
            })])
            .model(model.id.clone())
            .provider(model.provider.clone())
            .api(model.api)
            .stop_reason(StopReason::Stop)
            .usage(mush_ai::types::Usage {
                input_tokens: TokenCount::new(tokens as u64),
                ..Default::default()
            })
            .timestamp_ms(Timestamp::now())
            .build();
        Ok(Box::pin(futures::stream::iter([
            mush_ai::stream::StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            },
        ])))
    }
}

fn answered(connection: &InProcessConnection) -> Option<String> {
    match connection.runtime.conversation.context().last() {
        Some(Message::Assistant(reply)) => Some(reply.text()),
        _ => None,
    }
}

fn is_summary_request(request: &mush_ai::registry::LlmContext) -> bool {
    matches!(
        request.messages.last(),
        Some(Message::User(user)) if user.text().starts_with("[context checkpoint]")
    )
}

fn windowed(
    chars_per_token: usize,
    history: Vec<Message>,
) -> (InProcessConnection, crate::testing::Requests) {
    let requests: crate::testing::Requests = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ApiRegistry::new();
    let mut model = crate::testing::echo_model();
    model.id = "a-model-with-a-small-window".into();
    model.context_window = TokenCount::new(20_000);
    model.effective_context_window = None;
    registry.register(Box::new(Window {
        requests: requests.clone(),
        chars_per_token,
        limit: 20_000,
    }));
    let mut config = crate::testing::echo_config(model.clone());
    config.auto_compact = true;
    config.options.max_tokens = Some(TokenCount::new(1_000));
    config.compaction = mush_session::compact::CompactionSettings {
        reserve_tokens: 1_000,
        keep_recent_tokens: 1_000,
        ..Default::default()
    };
    let mut runtime = SessionRuntime::new(ConversationState::from_messages(history));
    runtime.model_id = model.id.to_string();
    runtime.system_prompt = Some("you are a careful agent. ".repeat(600));
    let connection = InProcessConnection::new(
        runtime,
        ToolRegistry::new(),
        registry,
        config,
        mush_agent::AgentInjections::default(),
    );
    (connection, requests)
}

fn long_history(pairs: usize, chars: usize) -> Vec<Message> {
    let model = crate::testing::echo_model();
    (0..pairs)
        .flat_map(|i| {
            [
                user(&format!("step {i}: {}", "x".repeat(chars))),
                Message::Assistant(
                    AssistantMessage::builder()
                        .content(vec![AssistantContentPart::Text(TextContent {
                            text: format!("done {i}"),
                        })])
                        .model(model.id.clone())
                        .provider(model.provider.clone())
                        .api(model.api)
                        .stop_reason(StopReason::Stop)
                        .timestamp_ms(Timestamp::zero())
                        .build(),
                ),
            ]
        })
        .collect()
}

#[tokio::test]
async fn a_woken_session_counts_the_system_prompt_before_its_first_request() {
    let (mut connection, requests) = windowed(4, long_history(20, 3_000));

    prompt(&mut connection, "one short paragraph from the parent").await;

    let window = Window {
        requests: Arc::default(),
        chars_per_token: 4,
        limit: 20_000,
    };
    let asked = requests.lock().unwrap();
    let turn_requests: Vec<usize> = asked
        .iter()
        .filter(|request| !is_summary_request(request))
        .map(|request| window.tokens(request))
        .collect();
    assert_eq!(turn_requests.len(), 1, "{turn_requests:?}");
    assert!(turn_requests[0] <= 20_000, "{turn_requests:?}");
    assert!(
        matches!(
            connection.runtime.conversation.context().last(),
            Some(Message::Assistant(reply)) if reply.text() == "ok"
        ),
        "the turn answered"
    );
    assert!(
        connection
            .runtime
            .conversation
            .context()
            .iter()
            .any(|message| match message {
                Message::Event(event) => event.kind.is_summary(),
                Message::User(user) => {
                    mush_ai::types::strip_compaction_summary_prefix(&user.text()).is_some()
                }
                _ => false,
            }),
        "the turn compacted first"
    );
}

#[tokio::test]
async fn a_prompt_too_long_on_the_first_request_compacts_and_retries() {
    let (mut connection, requests) = windowed(2, long_history(20, 1_500));

    prompt(&mut connection, "one short paragraph from the parent").await;

    let asked = requests.lock().unwrap();
    assert!(asked.len() >= 2, "{}", asked.len());
    assert!(
        matches!(
            connection.runtime.conversation.context().last(),
            Some(Message::Assistant(reply)) if reply.text() == "ok"
        ),
        "the turn answered"
    );
    let last = connection.runtime.conversation.context();
    assert!(
        matches!(last.last(), Some(Message::Assistant(reply)) if reply.text() == "ok"),
        "{:?}",
        last.last()
    );
}

#[tokio::test]
async fn a_woken_session_trusts_the_size_its_last_reply_measured() {
    let mut history = long_history(10, 1_800);
    if let Some(Message::Assistant(last)) = history.last_mut() {
        last.usage.input_tokens = TokenCount::new(19_500);
    }
    let (mut connection, requests) = windowed(1, history);
    connection.runtime.system_prompt = None;

    prompt(&mut connection, "one short paragraph from the parent").await;

    let window = Window {
        requests: Arc::default(),
        chars_per_token: 1,
        limit: 20_000,
    };
    let asked = requests.lock().unwrap();
    let turn_requests: Vec<usize> = asked
        .iter()
        .filter(|request| !is_summary_request(request))
        .map(|request| window.tokens(request))
        .collect();
    assert_eq!(turn_requests.len(), 1, "{turn_requests:?}");
    assert!(turn_requests[0] <= 20_000, "{turn_requests:?}");
    assert!(
        matches!(
            connection.runtime.conversation.context().last(),
            Some(Message::Assistant(reply)) if reply.text() == "ok"
        ),
        "the turn answered"
    );
}

#[tokio::test]
async fn auto_compaction_switches_off_and_on_for_the_running_session() {
    let (mut connection, requests) = windowed(4, long_history(20, 3_000));
    assert!(connection.state().auto_compact);

    connection
        .send(ClientCommand::SetAutoCompact(false))
        .await
        .unwrap();
    assert!(!connection.state().auto_compact);
    prompt(&mut connection, "one short paragraph from the parent").await;
    assert!(
        !requests.lock().unwrap().iter().any(is_summary_request),
        "switched off, nothing compacts"
    );
    assert_eq!(answered(&connection), None);

    connection
        .send(ClientCommand::SetAutoCompact(true))
        .await
        .unwrap();
    prompt(&mut connection, "try again").await;
    assert!(requests.lock().unwrap().iter().any(is_summary_request));
    assert_eq!(answered(&connection).as_deref(), Some("ok"));
}

struct PromptOf(Arc<std::sync::atomic::AtomicU64>);

#[async_trait::async_trait]
impl mush_ai::registry::ApiProvider for PromptOf {
    fn api(&self) -> Api {
        Api::AnthropicMessages
    }

    async fn stream(
        &self,
        model: &mush_ai::types::Model,
        _context: &mush_ai::registry::LlmContext,
        _options: &mush_ai::types::StreamOptions,
    ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
        let prompt = self.0.load(std::sync::atomic::Ordering::Relaxed);
        let message = AssistantMessage::builder()
            .content(vec![AssistantContentPart::Text(TextContent {
                text: "ok".into(),
            })])
            .model(model.id.clone())
            .provider(model.provider.clone())
            .api(model.api)
            .stop_reason(StopReason::Stop)
            .usage(mush_ai::types::Usage {
                input_tokens: TokenCount::new(1_000),
                output_tokens: TokenCount::new(10),
                cache_read_tokens: TokenCount::new(prompt - 1_000),
                cache_write_tokens: TokenCount::ZERO,
            })
            .timestamp_ms(Timestamp::zero())
            .build();
        Ok(Box::pin(futures::stream::iter([
            mush_ai::stream::StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            },
        ])))
    }
}

#[tokio::test]
async fn crossing_a_price_cliff_warns_once_in_the_transcript_and_in_the_state() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let prompt_size = Arc::new(std::sync::atomic::AtomicU64::new(104_312));
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(PromptOf(prompt_size.clone())));
    let model = mush_ai::models::anthropic_models()
        .into_iter()
        .find(|model| model.id.as_str() == "claude-haiku-5-5")
        .unwrap();
    let mut runtime = SessionRuntime::new(ConversationState::new());
    runtime.model_id = model.id.to_string();
    let mut connection = InProcessConnection::new(
        runtime,
        ToolRegistry::new(),
        registry,
        crate::testing::echo_config(model),
        mush_agent::AgentInjections::default(),
    );
    let notices = |connection: &InProcessConnection| {
        connection
            .runtime
            .conversation
            .context()
            .iter()
            .filter(|message| {
                matches!(message, Message::User(user)
                    if user.text().starts_with(crate::price_cliff::PRICE_CLIFF_TAG))
            })
            .count()
    };
    let warned = |events: &[ClientEvent]| {
        events
            .iter()
            .filter(|event| matches!(event, ClientEvent::PriceCliff(_)))
            .count()
    };

    connection
        .send(ClientCommand::Prompt {
            text: "one".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    let events = drain_turn(&mut connection).await;
    assert_eq!(warned(&events), 1);
    assert_eq!(notices(&connection), 1);
    let cliff = connection.state().price_cliff.unwrap();
    assert_eq!(cliff.over_prompt_tokens, 100_000);
    assert_eq!(cliff.prompt_tokens, 104_312);

    connection
        .send(ClientCommand::Prompt {
            text: "two".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(warned(&drain_turn(&mut connection).await), 0);
    assert_eq!(
        notices(&connection),
        1,
        "still over the cliff, no second notice"
    );

    prompt_size.store(20_000, std::sync::atomic::Ordering::Relaxed);
    prompt(&mut connection, "three").await;
    assert_eq!(connection.state().price_cliff, None);

    prompt_size.store(120_000, std::sync::atomic::Ordering::Relaxed);
    connection
        .send(ClientCommand::Prompt {
            text: "four".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(
        warned(&drain_turn(&mut connection).await),
        1,
        "crossing again warns again"
    );
    assert_eq!(notices(&connection), 2);
}

type Sent = Arc<Mutex<Vec<(mush_ai::registry::LlmContext, mush_ai::types::StreamOptions)>>>;

struct Recorder(Sent);

#[async_trait::async_trait]
impl mush_ai::registry::ApiProvider for Recorder {
    fn api(&self) -> Api {
        Api::AnthropicMessages
    }

    async fn stream(
        &self,
        model: &mush_ai::types::Model,
        context: &mush_ai::registry::LlmContext,
        options: &mush_ai::types::StreamOptions,
    ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
        self.0
            .lock()
            .unwrap()
            .push((context.clone(), options.clone()));
        let message = AssistantMessage::builder()
            .content(vec![AssistantContentPart::Text(TextContent {
                text: "ok".into(),
            })])
            .model(model.id.clone())
            .provider(model.provider.clone())
            .api(model.api)
            .stop_reason(StopReason::Stop)
            .timestamp_ms(Timestamp::zero())
            .build();
        Ok(Box::pin(futures::stream::iter([
            mush_ai::stream::StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            },
        ])))
    }
}

#[tokio::test]
async fn a_200k_window_with_a_128k_reply_limit_does_not_compact_at_100k_and_the_reply_fits_the_room()
 {
    let sent: Sent = Arc::default();
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(Recorder(sent.clone())));
    let mut model = crate::testing::echo_model();
    model.id = "a-200k-model".into();
    model.context_window = TokenCount::new(200_000);
    model.effective_context_window = None;
    model.max_output_tokens = TokenCount::new(128_000);
    let mut config = crate::testing::echo_config(model.clone());
    config.auto_compact = true;
    let mut runtime =
        SessionRuntime::new(ConversationState::from_messages(long_history(22, 20_000)));
    runtime.model_id = model.id.to_string();
    let mut connection = InProcessConnection::new(
        runtime,
        ToolRegistry::new(),
        registry,
        config,
        mush_agent::AgentInjections::default(),
    );

    prompt(&mut connection, "carry on").await;

    let sent = sent.lock().unwrap();
    let prompt_tokens = mush_session::compact::estimate_tokens(&sent[0].0.messages) as u64;
    assert!(
        (100_000..190_000).contains(&prompt_tokens),
        "the history should sit past the old 100k trigger: {prompt_tokens}"
    );
    assert!(
        !sent.iter().any(|(context, _)| is_summary_request(context)),
        "nothing compacts below 95% of the window"
    );
    let cap = sent[0].1.reply_cap.expect("the reply was fitted").get();
    assert!(
        cap < 128_000 && cap + prompt_tokens <= 200_000 + 1_000,
        "cap {cap} for a {prompt_tokens} token prompt"
    );
}

#[tokio::test]
async fn a_child_on_a_model_with_a_price_cliff_compacts_before_it_and_the_root_does_not() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let model = mush_ai::models::anthropic_models()
        .into_iter()
        .find(|model| model.id.as_str() == "claude-haiku-5-5")
        .unwrap();
    let run = |depth: u32| {
        let model = model.clone();
        async move {
            let sent: Sent = Arc::default();
            let mut registry = ApiRegistry::new();
            registry.register(Box::new(Recorder(sent.clone())));
            let mut config = crate::testing::echo_config(model.clone());
            config.auto_compact = true;
            let mut runtime =
                SessionRuntime::new(ConversationState::from_messages(long_history(19, 20_000)));
            runtime.model_id = model.id.to_string();
            runtime.rlm_depth = depth;
            let mut connection = InProcessConnection::new(
                runtime,
                ToolRegistry::new(),
                registry,
                config,
                mush_agent::AgentInjections::default(),
            );
            prompt(&mut connection, "carry on").await;
            sent.lock()
                .unwrap()
                .iter()
                .any(|(context, _)| is_summary_request(context))
        }
    };

    assert!(run(1).await, "a child stays under the 100k cliff");
    assert!(!run(0).await, "the root keeps the plain threshold");
}

#[tokio::test]
async fn the_price_cliff_pin_follows_the_settings_command() {
    let (mut connection, _requests) = connection();
    assert_eq!(
        connection.state().below_price_cliff,
        mush_session::compact::PriceCliffPin::Children
    );
    connection
        .send(ClientCommand::SetPriceCliffPin(
            mush_session::compact::PriceCliffPin::All,
        ))
        .await
        .unwrap();
    assert_eq!(
        connection.state().below_price_cliff,
        mush_session::compact::PriceCliffPin::All
    );
}

#[tokio::test]
async fn a_spawning_session_hears_once_when_what_a_child_gets_changes() {
    let _dirs = crate::testing::IsolatedDirs::acquire().await;
    let (mut connection, requests) = connection();
    let family = crate::family::Family::new(
        crate::family::Identity::root("root"),
        "root".into(),
        2,
        None,
        None,
    );
    connection.set_family(Some(Arc::clone(&family)));
    connection.runtime.rlm_max_depth = 2;
    let notices = |requests: &crate::testing::Requests| {
        requests
            .lock()
            .unwrap()
            .last()
            .map(|request| {
                request
                    .messages
                    .iter()
                    .filter(|message| {
                        matches!(message, Message::User(user)
                            if user.text().starts_with(crate::models::MODELS_NOTICE_TAG))
                    })
                    .count()
            })
            .unwrap_or_default()
    };

    prompt(&mut connection, "one").await;
    assert_eq!(
        notices(&requests),
        0,
        "the first turn only records the baseline"
    );
    assert!(connection.runtime.spawn_view.is_some());

    prompt(&mut connection, "two").await;
    assert_eq!(notices(&requests), 0, "nothing changed");

    connection
        .runtime
        .models
        .set_default_subagent(Some("haiku".into()));
    prompt(&mut connection, "three").await;
    assert_eq!(notices(&requests), 1);
    let last = requests.lock().unwrap().last().unwrap().messages.clone();
    assert!(
        last.iter()
            .any(|message| matches!(message, Message::User(user)
        if user.text().contains("children now default to haiku")))
    );

    prompt(&mut connection, "four").await;
    assert_eq!(notices(&requests), 1, "the notice is not repeated");

    let saved = connection.runtime.snapshot().spawn_view.unwrap();
    assert_eq!(saved["default child model"], "haiku");
}

struct RejectsImages {
    requests: crate::testing::Requests,
    body: &'static str,
}

#[async_trait::async_trait]
impl mush_ai::registry::ApiProvider for RejectsImages {
    fn api(&self) -> Api {
        Api::AnthropicMessages
    }

    async fn stream(
        &self,
        model: &mush_ai::types::Model,
        context: &mush_ai::registry::LlmContext,
        options: &mush_ai::types::StreamOptions,
    ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
        self.requests.lock().unwrap().push(context.clone());
        if context.messages.iter().any(carries_image) {
            return Err(mush_ai::registry::ProviderError::Other(format!(
                "anthropic returned 400 Bad Request (application/json): {}",
                self.body
            )));
        }
        crate::testing::Echo {
            requests: Arc::new(Mutex::new(Vec::new())),
            reply: "seen".into(),
        }
        .stream(model, context, options)
        .await
    }
}

fn carries_image(message: &Message) -> bool {
    matches!(
        message,
        Message::User(user) if matches!(
            &user.content,
            mush_ai::types::UserContent::Parts(parts)
                if parts.iter().any(|part| matches!(part, mush_ai::types::UserContentPart::Image(_)))
        )
    )
}

const IMAGE_TOO_LARGE: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.2.content.1.image.source.base64: image exceeds 10 MB maximum: 13658052 bytes > 10485760 bytes"}}"#;

fn connection_rejecting_images(
    body: &'static str,
) -> (InProcessConnection, crate::testing::Requests) {
    let requests: crate::testing::Requests = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(RejectsImages {
        requests: requests.clone(),
        body,
    }));
    let model = crate::testing::echo_model();
    let mut runtime = SessionRuntime::new(ConversationState::new());
    runtime.model_id = model.id.to_string();
    let connection = InProcessConnection::new(
        runtime,
        ToolRegistry::new(),
        registry,
        crate::testing::echo_config(model),
        mush_agent::AgentInjections::default(),
    );
    (connection, requests)
}

fn picture() -> PendingImage {
    PendingImage {
        data: vec![0x89, b'P', b'N', b'G', 1, 2, 3],
        mime_type: mush_ai::types::ImageMimeType::Png,
        dimensions: Some((3840, 2160)),
        note: None,
    }
}

#[tokio::test]
async fn a_rejected_image_message_goes_back_to_the_composer_and_the_session_carries_on() {
    let (mut connection, requests) = connection_rejecting_images(IMAGE_TOO_LARGE);
    prompt(&mut connection, "first").await;

    connection
        .send(ClientCommand::Prompt {
            text: "[image 1] oh i meant to send this".into(),
            images: vec![picture()],
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;
    let replaced = wait_for(&mut connection, |event| {
        matches!(
            event,
            ClientEvent::Session(SessionEvent::TranscriptReplaced { .. })
        )
    })
    .await;
    let returned = wait_for(&mut connection, |event| {
        matches!(event, ClientEvent::PromptReturned(_))
    })
    .await;

    let ClientEvent::PromptReturned(returned) = returned else {
        unreachable!()
    };
    assert_eq!(returned.text, "[image 1] oh i meant to send this");
    assert_eq!(returned.images.len(), 1);
    assert_eq!(returned.images[0].data, picture().data);
    assert!(returned.error.contains("image exceeds 10 MB maximum"));
    let ClientEvent::Session(SessionEvent::TranscriptReplaced { messages, .. }) = replaced else {
        unreachable!()
    };
    assert_eq!(messages.len(), 2);
    assert!(!messages.iter().any(carries_image));
    assert!(
        connection
            .runtime
            .conversation
            .entries()
            .iter()
            .any(|entry| matches!(
                &entry.kind,
                mush_session::EntryKind::Message { message } if carries_image(message)
            )),
        "the rejected message stays in the tree, off the branch"
    );

    prompt(&mut connection, "oop").await;

    let transcript = connection.transcript().await;
    let texts: Vec<String> = transcript
        .iter()
        .map(|message| match message {
            Message::User(user) => user.text(),
            Message::Assistant(reply) => reply.text(),
            _ => String::new(),
        })
        .collect();
    assert_eq!(texts, ["first", "seen", "oop", "seen"]);
    let last_request = requests.lock().unwrap().last().cloned().unwrap();
    assert!(!last_request.messages.iter().any(carries_image));
}

#[tokio::test]
async fn a_rejection_that_is_not_about_the_image_keeps_the_message() {
    let (mut connection, _) = connection_rejecting_images(
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: 64000 > 32000"}}"#,
    );

    connection
        .send(ClientCommand::Prompt {
            text: "[image 1] look".into(),
            images: vec![picture()],
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;

    while let Some(event) = connection.try_next_event() {
        assert!(
            !matches!(event, ClientEvent::PromptReturned(_)),
            "{event:?}"
        );
    }
    let transcript = connection.transcript().await;
    assert!(transcript.iter().any(carries_image));
}

fn root_with_family() -> (
    InProcessConnection,
    crate::testing::Requests,
    Arc<crate::family::Family>,
) {
    let (mut connection, requests) = connection();
    let family = crate::family::Family::new(
        crate::family::Identity::root("lead"),
        "root-s".into(),
        2,
        None,
        None,
    );
    connection.set_family(Some(Arc::clone(&family)));
    (connection, requests, family)
}

fn root_decision(family: &crate::family::Family, id: &str) -> crate::decisions::DecisionKey {
    family
        .ask_user(
            &crate::family::Identity::root("lead"),
            crate::decisions::Ask {
                id: id.into(),
                question: format!("{id}?"),
                options: ["yes", "no"]
                    .into_iter()
                    .map(|label| crate::decisions::DecisionOption {
                        label: label.into(),
                        ..Default::default()
                    })
                    .collect(),
                lean: Some(crate::decisions::Lean::Index(0)),
                ..Default::default()
            },
        )
        .unwrap()
        .decision
        .key
}

#[tokio::test]
async fn the_decision_list_reaches_the_client_when_it_changes() {
    let (mut connection, _requests, family) = root_with_family();
    assert!(
        !matches!(
            connection.try_next_event(),
            Some(ClientEvent::DecisionsChanged(_))
        ),
        "an empty list stays quiet at first"
    );
    root_decision(&family, "push");
    let event = connection.try_next_event();
    let Some(ClientEvent::DecisionsChanged(list)) = event else {
        panic!("expected the decision list, got {event:?}");
    };
    assert_eq!(list.len(), 1);
    assert!(connection.try_next_event().is_none());

    let events = connection
        .send(ClientCommand::Decisions(
            crate::decisions::DecisionsCommand::Refresh,
        ))
        .await
        .unwrap();
    assert!(matches!(
        &events[..],
        [ClientEvent::DecisionsChanged(list)] if list.len() == 1
    ));
}

#[tokio::test]
async fn answering_the_roots_own_decision_arrives_as_the_users_message() {
    let (mut connection, requests, family) = root_with_family();
    let key = root_decision(&family, "push");
    root_decision(&family, "tag");
    connection
        .send(ClientCommand::Decisions(
            crate::decisions::DecisionsCommand::Draft {
                key: key.clone(),
                answer: Some(crate::decisions::Answer::Pick {
                    option: 1,
                    label: String::new(),
                    note: None,
                }),
            },
        ))
        .await
        .unwrap();

    let events = connection
        .send(ClientCommand::Decisions(
            crate::decisions::DecisionsCommand::Send { keys: vec![key] },
        ))
        .await
        .unwrap();
    let text = events
        .iter()
        .find_map(|event| match event {
            ClientEvent::UserInput {
                text,
                queued: false,
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .expect("the answer shows as the user's message");
    assert_eq!(
        text,
        "Answers to your decisions:\n\n[push] push?\nb) no\n\nStill open with me: [tag]."
    );
    let user_input = events
        .iter()
        .position(|event| matches!(event, ClientEvent::UserInput { .. }));
    let started = events
        .iter()
        .position(|event| matches!(event, ClientEvent::TurnStarted));
    assert!(user_input < started, "{events:?}");
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::Session(SessionEvent::Status(status)) if status == "sent 1 answer to 1 agent"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::DecisionsChanged(list) if list.len() == 1
    )));
    drain_turn(&mut connection).await;
    let asked = requests.lock().unwrap();
    assert!(asked[0].messages.iter().any(|message| {
        matches!(message, Message::User(user) if user.text().starts_with("Answers to your decisions:"))
    }));
}

#[tokio::test]
async fn a_child_decision_is_news_at_the_parents_next_turn() {
    let (mut connection, requests, family) = root_with_family();
    family
        .decisions()
        .post_news("root-s", "- auth asked the user [push]: Push?".into());
    connection.runtime.session_id = mush_ai::types::SessionId::from("root-s");
    connection.runtime.decisions = Some(Arc::clone(family.decisions()));
    connection
        .send(ClientCommand::Prompt {
            text: "carry on".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    drain_turn(&mut connection).await;
    let asked = requests.lock().unwrap();
    let texts: Vec<String> = asked[0]
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(user.text()),
            _ => None,
        })
        .collect();
    assert!(
        texts[0].starts_with(mush_session::decisions::NEWS_PREFIX),
        "{texts:?}"
    );
    assert!(texts[0].contains("- auth asked the user [push]: Push?"));
    assert_eq!(texts[1], "carry on");
    assert_eq!(family.decisions().take_news("root-s"), None);
}

#[tokio::test]
async fn betas_from_the_client_reach_the_next_request_the_summariser_and_the_state() {
    let seen: crate::testing::SeenBetas = Arc::default();
    let mut registry = ApiRegistry::new();
    registry.register(Box::new(crate::testing::RecordsBetas {
        seen: Arc::clone(&seen),
    }));
    let model = crate::testing::echo_model();
    let mut runtime = SessionRuntime::new(ConversationState::new());
    runtime.model_id = model.id.to_string();
    let mut config = crate::testing::echo_config(model.clone());
    config.compaction_model = Some((model, mush_ai::types::StreamOptions::default()));
    let mut connection = InProcessConnection::new(
        runtime,
        ToolRegistry::new(),
        registry,
        config,
        mush_agent::AgentInjections::default(),
    );
    prompt(&mut connection, "before").await;
    assert_eq!(connection.state().anthropic_betas, None);

    let betas = mush_ai::types::AnthropicBetas {
        effort: false,
        ..Default::default()
    };
    connection
        .send(ClientCommand::SetAnthropicBetas(betas.clone()))
        .await
        .unwrap();
    assert_eq!(connection.state().anthropic_betas, Some(betas.clone()));
    let summariser = connection.config.compaction_model.as_ref().unwrap();
    assert_eq!(summariser.1.anthropic_betas, Some(betas.clone()));

    prompt(&mut connection, "after").await;
    assert_eq!(*seen.lock().unwrap(), vec![None, Some(betas)]);
}
