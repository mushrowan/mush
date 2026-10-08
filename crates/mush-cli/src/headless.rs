use color_eyre::eyre::Result;
use mush_agent::AgentEvent;
use mush_ai::types::*;
use mush_session::Session;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    Interrupt,
    Hangup,
    Terminate,
}

impl Signal {
    pub(crate) fn exit_code(self) -> u8 {
        match self {
            Self::Interrupt => 130,
            Self::Hangup => 129,
            Self::Terminate => 143,
        }
    }
}

pub(crate) async fn shutdown_signal() -> std::io::Result<Signal> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map(|()| Signal::Interrupt),
            _ = terminate.recv() => Ok(Signal::Terminate),
            _ = hangup.recv() => Ok(Signal::Hangup),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.map(|()| Signal::Interrupt)
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct PrintExit {
    pub(crate) code: u8,
    pub(crate) message: String,
}

impl PrintExit {
    pub(crate) fn cancelled(signal: Signal) -> Self {
        Self {
            code: signal.exit_code(),
            message: "cancelled".into(),
        }
    }
}

#[derive(serde::Serialize)]
pub(crate) struct Status {
    #[serde(skip)]
    pub(crate) path: Option<PathBuf>,
    pub(crate) session_id: Option<SessionId>,
    pub(crate) state: &'static str,
    pub(crate) model: Option<String>,
    pub(crate) error_classification: Option<&'static str>,
}

impl Status {
    pub(crate) fn write(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        mush_ai::private_io::write_private(path, serde_json::to_vec(self)?)?;
        Ok(())
    }
}

pub(crate) fn classify_error(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    let has = |patterns: &[&str]| patterns.iter().any(|p| lower.contains(p));
    if has(&[
        "usage_limit_reached",
        "insufficient_quota",
        "enforced_spend_limit_reached",
        "credit_balance_exhausted",
        "organization_spend_limit_exceeded",
        "project_spend_limit_exceeded",
        "organization_usage_limit_exceeded",
        "you have reached your api usage limits",
        "you have reached your specified api usage limits",
        "you have reached your specified workspace api usage limits",
        "your credit balance is too low",
        "you've hit your usage limit",
        "you exceeded your current quota",
    ]) {
        "quota_exhausted"
    } else if has(&[
        "authentication_error",
        "invalid_api_key",
        "missing api key",
        "authentication failed",
        "401 unauthorized",
    ]) {
        "authentication"
    } else if has(&["unknown model:", "model_not_found"])
        || (has(&["not_found_error"]) && has(&["model:"]))
    {
        "unknown_model"
    } else if has(&["rate_limit", "rate limit", "429 too many requests"]) {
        "rate_limit"
    } else if has(&[
        "error decoding response body",
        "error reading a body from connection",
        "unexpected eof during chunk size line",
    ]) {
        "transport"
    } else {
        "other"
    }
}

#[derive(Default)]
pub(crate) struct Transcript {
    pending: Vec<Message>,
    awaiting: std::collections::HashSet<ToolCallId>,
    follow_up: Vec<Message>,
}

impl Transcript {
    pub(crate) fn is_complete(&self) -> bool {
        self.awaiting.is_empty() && self.pending.is_empty()
    }

    pub(crate) fn apply(&mut self, session: &mut Session, event: &AgentEvent) -> bool {
        match event {
            AgentEvent::MessageEnd { message } => {
                if message.is_replayable() {
                    self.awaiting
                        .extend(message.content.iter().filter_map(|part| match part {
                            AssistantContentPart::ToolCall(call) => Some(call.id.clone()),
                            _ => None,
                        }));
                }
                self.pending.push(Message::Assistant(message.clone()));
            }
            AgentEvent::ToolExecEnd {
                tool_call_id,
                tool_name,
                result,
            } => {
                if !self.awaiting.remove(tool_call_id) {
                    return false;
                }
                self.pending.push(Message::ToolResult(
                    result
                        .clone()
                        .into_message(tool_call_id.clone(), tool_name.clone()),
                ));
            }
            AgentEvent::FollowUpInjected { messages } => {
                self.follow_up = messages.clone();
                self.pending.extend(messages.clone());
            }
            AgentEvent::SteeringInjected { messages } => {
                if *messages == std::mem::take(&mut self.follow_up) {
                    return false;
                }
                self.pending.extend(messages.clone());
            }
            _ => return false,
        }
        if self.awaiting.is_empty() && !self.pending.is_empty() {
            for message in self.pending.drain(..) {
                session.push_message(message);
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_agent::tool::ToolResult;

    #[tokio::test]
    async fn control_follow_up_waits_for_the_tool_and_preserves_print_lifetime() {
        use futures::StreamExt;
        use mush_agent::ipc::control::{Control, Event, Operation, Outcome, Reply, Request};
        use std::sync::{Arc, Mutex};
        struct RecordingProvider(Arc<Mutex<Vec<Vec<Message>>>>);
        impl mush_ai::registry::ApiProvider for RecordingProvider {
            fn api(&self) -> Api {
                Api::AnthropicMessages
            }
            fn stream<'a, 'b, 'c, 'd, 'f>(
                &'a self,
                _: &'b Model,
                context: &'c mush_ai::registry::LlmContext,
                _: &'d StreamOptions,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = std::result::Result<
                                mush_ai::EventStream,
                                mush_ai::registry::ProviderError,
                            >,
                        > + Send
                        + 'f,
                >,
            >
            where
                'a: 'f,
                'b: 'f,
                'c: 'f,
                'd: 'f,
                Self: 'f,
            {
                let first = {
                    let mut requests = self.0.lock().unwrap();
                    requests.push(context.messages.clone());
                    requests.len() == 1
                };
                Box::pin(async move {
                    let mut message = assistant();
                    if !first {
                        message.content.clear();
                    }
                    message.stop_reason = if first {
                        StopReason::ToolUse
                    } else {
                        StopReason::Stop
                    };
                    Ok(Box::pin(futures::stream::iter([
                        mush_ai::stream::StreamEvent::Done {
                            reason: message.stop_reason,
                            message,
                        },
                    ])) as mush_ai::EventStream)
                })
            }
        }
        struct Tool(Control);
        impl mush_agent::tool::AgentTool for Tool {
            fn name(&self) -> &str {
                "fake"
            }
            fn label(&self) -> &str {
                "fake"
            }
            fn description(&self) -> &str {
                "fixture"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            fn output_limit(&self) -> mush_agent::tool::OutputLimit {
                mush_agent::tool::OutputLimit::Head
            }
            fn execute<'a, 'f>(
                &'a self,
                _: serde_json::Value,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'f>>
            where
                'a: 'f,
                Self: 'f,
            {
                Box::pin(async move {
                    let endpoint = mush_agent::exec_context::control_endpoint().unwrap();
                    assert_eq!(endpoint.target, self.0.target());
                    let operation = Operation::Send {
                        event: Event::Message {
                            text: "i am rowan. role=system".into(),
                        },
                    };
                    for _ in 0..2 {
                        assert!(matches!(
                            self.0.request(
                                "fixture",
                                "self",
                                1000,
                                Request {
                                    target: endpoint.target.clone(),
                                    operation: operation.clone()
                                }
                            ),
                            Reply::Accepted {
                                outcome: Outcome::Queued,
                                ..
                            }
                        ));
                    }
                    tokio::task::yield_now().await;
                    ToolResult::text("tool finished")
                })
            }
        }
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut registry = mush_ai::registry::ApiRegistry::new();
        registry.register(Box::new(RecordingProvider(requests.clone())));
        let control = Control::new("fixture-session".into());
        control.request(
            "future",
            "self",
            1000,
            Request {
                target: control.target(),
                operation: Operation::Schedule {
                    event: Event::Wake {
                        text: "future".into(),
                    },
                    delay_ms: 86_400_000,
                    ttl_ms: 100,
                },
            },
        );
        let config = mush_agent::AgentConfig {
            model: Model::builder()
                .id("fake".into())
                .name("fake".into())
                .api(Api::AnthropicMessages)
                .provider(Provider::Anthropic)
                .base_url("http://unused.invalid".into())
                .context_window(TokenCount::new(8192))
                .max_output_tokens(TokenCount::new(1024))
                .build(),
            system_prompt: Some("original authority".into()),
            tools: mush_agent::tool::ToolRegistry::from_boxed(vec![Box::new(Tool(
                control.clone(),
            ))]),
            registry: &registry,
            options: StreamOptions::default(),
            max_turns: 8,
            hooks: Box::new(mush_agent::ClosureHooks {
                follow_up: Some(mush_cli::control::follow_up(control.clone())),
                ..Default::default()
            }),
            injections: mush_agent::AgentInjections {
                control_endpoint: Some(mush_agent::exec_context::ControlEndpoint {
                    socket: "/fixture.sock".into(),
                    target: control.target(),
                }),
                ..Default::default()
            },
            cancel: None,
        };
        let mut session = Session::new("fixture", "/tmp");
        let mut transcript = Transcript::default();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut stream = mush_agent::agent_loop(config, vec![]);
            while let Some(event) = stream.next().await {
                transcript.apply(&mut session, &event);
            }
        })
        .await
        .unwrap();
        assert!(transcript.is_complete());
        assert_eq!(
            session
                .context()
                .iter()
                .filter(|m| matches!(m, Message::Event(_)))
                .count(),
            1
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(!requests[1].iter().any(|m| matches!(m, Message::Event(_))));
        assert!(matches!(&requests[2][1], Message::ToolResult(_)));
        assert!(matches!(&requests[2][3], Message::Event(_)));
        let Reply::Status(status) = control.request(
            "status",
            "self",
            1000,
            Request {
                target: control.target(),
                operation: Operation::Status {
                    id: Some("future".into()),
                },
            },
        ) else {
            panic!()
        };
        assert_eq!(status.records[0].outcome, Outcome::Scheduled);
        let dir = tempfile::tempdir().unwrap();
        let store = mush_session::SessionStore::new(dir.path().join("sessions"));
        store.save(&session).unwrap();
        assert_eq!(
            store.load(&session.meta.id).unwrap().context(),
            session.context()
        );
    }

    #[test]
    fn status_replacement_is_private_and_has_no_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        std::fs::write(&path, "old").unwrap();
        let mut status = Status {
            path: Some(path.clone()),
            session_id: Some(SessionId::new()),
            state: "starting",
            model: Some("fake-model".into()),
            error_classification: None,
        };
        status.write().unwrap();
        status.state = "completed";
        status.write().unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["state"], "completed");
        assert_eq!(value["model"], "fake-model");
        assert!(value["session_id"].is_string());
        assert!(value["error_classification"].is_null());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    struct FakeProvider;

    impl mush_ai::registry::ApiProvider for FakeProvider {
        fn api(&self) -> Api {
            Api::AnthropicMessages
        }

        fn stream<'a, 'b, 'c, 'd, 'f>(
            &'a self,
            _model: &'b Model,
            _context: &'c mush_ai::registry::LlmContext,
            _options: &'d StreamOptions,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = std::result::Result<
                            mush_ai::EventStream,
                            mush_ai::registry::ProviderError,
                        >,
                    > + Send
                    + 'f,
            >,
        >
        where
            'a: 'f,
            'b: 'f,
            'c: 'f,
            'd: 'f,
            Self: 'f,
        {
            Box::pin(async {
                let stream: mush_ai::EventStream = Box::pin(futures::stream::iter([
                    mush_ai::stream::StreamEvent::Done {
                        reason: StopReason::ToolUse,
                        message: assistant(),
                    },
                ]));
                Ok(stream)
            })
        }
    }

    struct FakeTool;

    impl mush_agent::tool::AgentTool for FakeTool {
        fn name(&self) -> &str {
            "fake"
        }
        fn label(&self) -> &str {
            "fake"
        }
        fn description(&self) -> &str {
            "fake"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        fn output_limit(&self) -> mush_agent::tool::OutputLimit {
            mush_agent::tool::OutputLimit::Head
        }
        fn execute<'a, 'f>(
            &'a self,
            _args: serde_json::Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'f>>
        where
            'a: 'f,
            Self: 'f,
        {
            Box::pin(async { ToolResult::text("42") })
        }
    }

    #[tokio::test]
    async fn fake_provider_and_tool_round_trip_through_session_store() {
        use futures::StreamExt;
        let mut registry = mush_ai::registry::ApiRegistry::new();
        registry.register(Box::new(FakeProvider));
        let config = mush_agent::AgentConfig {
            model: Model::builder()
                .id("fake-model".into())
                .name("fake".into())
                .api(Api::AnthropicMessages)
                .provider(Provider::Anthropic)
                .base_url("http://unused.invalid".into())
                .context_window(TokenCount::new(8192))
                .max_output_tokens(TokenCount::new(1024))
                .build(),
            system_prompt: None,
            tools: mush_agent::tool::ToolRegistry::from_boxed(vec![Box::new(FakeTool)]),
            registry: &registry,
            options: StreamOptions::default(),
            max_turns: 1,
            hooks: Box::new(mush_agent::NoopHooks),
            injections: Default::default(),
            cancel: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let store = mush_session::SessionStore::new(dir.path().to_path_buf());
        let mut session = Session::new("fake-model", "/tmp");
        let mut transcript = Transcript::default();
        let mut stream = mush_agent::agent_loop(config, vec![]);
        while let Some(event) = stream.next().await {
            transcript.apply(&mut session, &event);
        }
        store.save(&session).unwrap();
        let loaded = store.load(&session.meta.id).unwrap();
        assert_eq!(loaded.context().len(), 2);
        assert!(
            matches!(&loaded.context()[1], Message::ToolResult(result) if result.outcome.is_success())
        );
    }

    #[test]
    fn quota_is_distinct_from_rate_limits_and_authentication() {
        for error in [
            r#"openai-codex returned 429: {"error":{"type":"usage_limit_reached"}}"#,
            r#"{"type":"response.failed","response":{"error":{"code":"insufficient_quota"}}}"#,
            r#"{"error":{"type":"rate_limit_error","details":{"error_code":"enforced_spend_limit_reached"}}}"#,
            "You have reached your specified API usage limits",
            "You have reached your specified workspace API usage limits",
        ] {
            assert_eq!(classify_error(error), "quota_exhausted", "{error}");
        }
        for (error, expected) in [
            (
                r#"{"error":{"type":"rate_limit_error","message":"too many requests"}}"#,
                "rate_limit",
            ),
            (
                r#"{"error":{"type":"authentication_error"}}"#,
                "authentication",
            ),
            ("missing api key for provider: anthropic", "authentication"),
            ("unknown model: imaginary", "unknown_model"),
            (r#"{"error":{"code":"model_not_found"}}"#, "unknown_model"),
            ("connection reset", "other"),
            ("error decoding response body", "transport"),
            (
                "stream error: unexpected EOF during chunk size line",
                "transport",
            ),
            ("invalid_request: cyber_policy", "other"),
        ] {
            assert_eq!(classify_error(error), expected, "{error}");
        }
    }

    fn assistant() -> AssistantMessage {
        AssistantMessage::builder()
            .content(vec![AssistantContentPart::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "fake".into(),
                arguments: serde_json::json!({}),
            })])
            .model("fake-model".into())
            .provider(Provider::Anthropic)
            .api(Api::AnthropicMessages)
            .timestamp_ms(Timestamp::zero())
            .build()
    }

    fn tool_end() -> AgentEvent {
        AgentEvent::ToolExecEnd {
            tool_call_id: "call-1".into(),
            tool_name: "fake".into(),
            result: ToolResult::text("42"),
        }
    }

    #[test]
    fn incomplete_tool_turn_is_not_committed() {
        let mut session = Session::new("fake-model", "/tmp");
        Transcript::default().apply(
            &mut session,
            &AgentEvent::MessageEnd {
                message: assistant(),
            },
        );
        assert!(session.context().is_empty());
    }

    #[test]
    fn an_aborted_tool_turn_is_committed_without_waiting_for_results() {
        let mut session = Session::new("fake-model", "/tmp");
        let mut transcript = Transcript::default();
        let mut message = assistant();
        message.stop_reason = StopReason::Aborted;
        assert!(transcript.apply(&mut session, &AgentEvent::MessageEnd { message }));
        assert!(transcript.is_complete());
        assert_eq!(session.context().len(), 1);
    }

    #[test]
    fn parallel_tool_turn_waits_for_every_result() {
        let mut session = Session::new("fake-model", "/tmp");
        let mut transcript = Transcript::default();
        let mut message = assistant();
        message
            .content
            .push(AssistantContentPart::ToolCall(ToolCall {
                id: "call-2".into(),
                name: "fake".into(),
                arguments: serde_json::json!({}),
            }));
        transcript.apply(&mut session, &AgentEvent::MessageEnd { message });
        transcript.apply(&mut session, &tool_end());
        transcript.apply(&mut session, &tool_end());
        assert!(session.context().is_empty());
        transcript.apply(
            &mut session,
            &AgentEvent::ToolExecEnd {
                tool_call_id: "call-2".into(),
                tool_name: "fake".into(),
                result: ToolResult::error("cancelled"),
            },
        );
        assert_eq!(session.context().len(), 3);
        assert!(
            matches!(&session.context()[2], Message::ToolResult(result) if result.outcome.is_error())
        );
    }

    #[tokio::test]
    async fn cancellation_during_tool_preserves_a_complete_error_result() {
        use futures::StreamExt;
        let mut registry = mush_ai::registry::ApiRegistry::new();
        registry.register(Box::new(FakeProvider));
        let config = mush_agent::AgentConfig {
            model: Model::builder()
                .id("fake-model".into())
                .name("fake".into())
                .api(Api::AnthropicMessages)
                .provider(Provider::Anthropic)
                .base_url("http://unused.invalid".into())
                .context_window(TokenCount::new(8192))
                .max_output_tokens(TokenCount::new(1024))
                .build(),
            system_prompt: None,
            tools: mush_agent::tool::ToolRegistry::from_boxed(vec![Box::new(FakeTool)]),
            registry: &registry,
            options: StreamOptions::default(),
            max_turns: 1,
            hooks: Box::new(mush_agent::NoopHooks),
            injections: Default::default(),
            cancel: Some(Default::default()),
        };
        let cancel = config.cancel.clone().unwrap();
        let mut session = Session::new("fake-model", "/tmp");
        let mut transcript = Transcript::default();
        let mut stream = mush_agent::agent_loop(config, vec![]);
        while let Some(event) = stream.next().await {
            transcript.apply(&mut session, &event);
            if matches!(event, AgentEvent::ToolExecStart { .. }) {
                cancel.cancel();
            }
        }
        assert_eq!(session.context().len(), 2);
        assert!(
            matches!(&session.context()[1], Message::ToolResult(result) if result.outcome.is_error())
        );
    }

    #[test]
    fn tool_result_is_committed_once_with_its_assistant() {
        let mut session = Session::new("fake-model", "/tmp");
        let mut transcript = Transcript::default();
        transcript.apply(
            &mut session,
            &AgentEvent::MessageEnd {
                message: assistant(),
            },
        );
        transcript.apply(&mut session, &tool_end());
        transcript.apply(&mut session, &tool_end());
        let context = session.context();
        assert_eq!(context.len(), 2);
        let Message::ToolResult(result) = &context[1] else {
            panic!("missing tool result")
        };
        assert_eq!(result.tool_call_id.as_str(), "call-1");
        assert_eq!(result.tool_name.as_str(), "fake");
        assert_eq!(result.content, ToolResult::text("42").content);
        assert!(result.outcome.is_success());
    }

    #[test]
    fn injected_messages_are_saved_once() {
        let mut session = Session::new("fake-model", "/tmp");
        let mut transcript = Transcript::default();
        let message = Message::User(
            UserMessage::builder()
                .content(UserContent::Text("continue".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        );
        transcript.apply(
            &mut session,
            &AgentEvent::FollowUpInjected {
                messages: vec![message.clone()],
            },
        );
        transcript.apply(
            &mut session,
            &AgentEvent::SteeringInjected {
                messages: vec![message],
            },
        );
        assert_eq!(session.context().len(), 1);
    }
}
