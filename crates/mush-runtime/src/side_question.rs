use std::collections::HashSet;

use mush_ai::oneshot::OneshotError;
use mush_ai::registry::{ApiRegistry, LlmContext};
use mush_ai::types::{
    AssistantContentPart, Message, Model, StreamOptions, TextContent, Timestamp, ToolCall,
    ToolOutcome, ToolResultContentPart, ToolResultMessage, UserContent, UserMessage,
};
use mush_session::compact::LiveContext;

const SIDE_QUESTION_PROMPT: &str = "\
[side question]

The user has a side question. Answer it directly and briefly, in plain text, from what is \
already in this conversation. Your work carries on separately: this one reply is shown to the \
user on its own and is not added to the conversation. So do not call tools, do not continue the \
task, do not promise follow-up work and do not repeat the conversation back. If the conversation \
does not hold the answer, say so.

Question: ";

const STILL_RUNNING: &str =
    "still running: this call had not returned yet when the user asked their side question";

const NUDGE: &str =
    "Answer the side question above now, in plain text. No tools are available for this reply.";

#[must_use]
pub fn side_question_context(
    live: LiveContext,
    mut messages: Vec<Message>,
    question: &str,
) -> LlmContext {
    let running = unanswered_tool_calls(&messages);
    messages.extend(running.into_iter().map(still_running));
    messages.push(user_text(format!("{SIDE_QUESTION_PROMPT}{question}")));
    LlmContext {
        system_prompt: live.system_prompt,
        messages,
        tools: live.tools,
    }
}

pub async fn answer(
    registry: &ApiRegistry,
    model: &Model,
    mut context: LlmContext,
    options: &StreamOptions,
) -> Result<String, String> {
    match mush_ai::oneshot::collect_text(registry, model, &context, options).await {
        Err(OneshotError::NoText(reply)) => {
            tracing::warn!(%reply, "a side question got no answer, nudging once");
            context.messages.push(user_text(NUDGE.into()));
            mush_ai::oneshot::collect_text(registry, model, &context, options)
                .await
                .map_err(|error| match error {
                    OneshotError::NoText(reply) => format!("{reply}, again after a nudge"),
                    error => error.to_string(),
                })
        }
        result => result.map_err(|error| error.to_string()),
    }
}

fn unanswered_tool_calls(messages: &[Message]) -> Vec<ToolCall> {
    let Some((index, assistant)) =
        messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, message)| match message {
                Message::Assistant(assistant) => Some((index, assistant)),
                _ => None,
            })
    else {
        return Vec::new();
    };
    let answered: HashSet<_> = messages[index + 1..]
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(&result.tool_call_id),
            _ => None,
        })
        .collect();
    assistant
        .content
        .iter()
        .filter_map(|part| match part {
            AssistantContentPart::ToolCall(call) if !answered.contains(&call.id) => {
                Some(call.clone())
            }
            _ => None,
        })
        .collect()
}

fn still_running(call: ToolCall) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: call.id,
        tool_name: call.name,
        content: vec![ToolResultContentPart::Text(TextContent {
            text: STILL_RUNNING.into(),
        })],
        outcome: ToolOutcome::Success,
        timestamp_ms: Timestamp::now(),
        details: None,
    })
}

fn user_text(text: String) -> Message {
    Message::User(
        UserMessage::builder()
            .content(UserContent::Text(text))
            .timestamp_ms(Timestamp::now())
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use mush_ai::registry::{ApiProvider, EventStream, ProviderError, ToolDefinition};
    use mush_ai::stream::StreamEvent;
    use mush_ai::types::{
        Api, AssistantMessage, StopReason, ThinkingContent, ToolCallId, ToolName,
    };

    use super::*;

    fn live() -> LiveContext {
        LiveContext {
            system_prompt: Some("base prompt".into()),
            tools: vec![ToolDefinition {
                name: "ipython".into(),
                description: "runs python".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
        }
    }

    fn assistant(content: Vec<AssistantContentPart>, stop_reason: StopReason) -> AssistantMessage {
        let model = crate::testing::echo_model();
        AssistantMessage::builder()
            .content(content)
            .model(model.id)
            .provider(model.provider)
            .api(model.api)
            .stop_reason(stop_reason)
            .timestamp_ms(Timestamp::zero())
            .build()
    }

    fn call(id: &str) -> AssistantContentPart {
        AssistantContentPart::ToolCall(ToolCall {
            id: ToolCallId::from(id),
            name: ToolName::from("ipython"),
            arguments: serde_json::json!({}),
        })
    }

    fn result(id: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: ToolCallId::from(id),
            tool_name: ToolName::from("ipython"),
            content: vec![ToolResultContentPart::Text(TextContent {
                text: "done".into(),
            })],
            outcome: ToolOutcome::Success,
            timestamp_ms: Timestamp::zero(),
            details: None,
        })
    }

    fn asked(context: &LlmContext) -> String {
        match context.messages.last() {
            Some(Message::User(user)) => user.text(),
            other => panic!("expected the question last, got {other:?}"),
        }
    }

    #[test]
    fn the_question_rides_on_the_turns_own_prefix() {
        let conversation = vec![
            user_text("deploy it".into()),
            Message::Assistant(assistant(
                vec![AssistantContentPart::Text(TextContent {
                    text: "deployed".into(),
                })],
                StopReason::Stop,
            )),
        ];

        let context = side_question_context(live(), conversation.clone(), "why is it slow?");

        assert_eq!(context.system_prompt.as_deref(), Some("base prompt"));
        assert_eq!(context.tools.len(), 1);
        assert_eq!(context.tools[0].name, "ipython");
        assert_eq!(context.messages.len(), conversation.len() + 1);
        let question = asked(&context);
        assert!(question.starts_with("[side question]"), "{question}");
        assert!(question.contains("do not call tools"), "{question}");
        assert!(
            question.ends_with("Question: why is it slow?"),
            "{question}"
        );
    }

    #[test]
    fn a_tool_call_still_in_flight_reads_as_running_not_failed() {
        let conversation = vec![
            user_text("deploy it".into()),
            Message::Assistant(assistant(
                vec![
                    AssistantContentPart::Thinking(ThinkingContent::Thinking {
                        thinking: "run both".into(),
                        signature: Some("sig".into()),
                    }),
                    call("call_done"),
                    call("call_running"),
                ],
                StopReason::ToolUse,
            )),
            result("call_done"),
        ];

        let context = side_question_context(live(), conversation, "what's going on?");

        let results: Vec<_> = context
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].tool_call_id, ToolCallId::from("call_running"));
        assert!(results[1].outcome.is_success());
        assert!(
            matches!(&results[1].content[..], [ToolResultContentPart::Text(text)] if text.text.starts_with("still running")),
            "{:?}",
            results[1].content
        );
        assert!(asked(&context).ends_with("what's going on?"));
    }

    struct Scripted {
        replies: Mutex<VecDeque<Result<Vec<StreamEvent>, ProviderError>>>,
        requests: Arc<Mutex<Vec<LlmContext>>>,
    }

    #[async_trait::async_trait]
    impl ApiProvider for Scripted {
        fn api(&self) -> Api {
            Api::AnthropicMessages
        }

        async fn stream(
            &self,
            _model: &Model,
            context: &LlmContext,
            _options: &StreamOptions,
        ) -> Result<EventStream, ProviderError> {
            self.requests.lock().unwrap().push(context.clone());
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("a scripted reply");
            reply.map(|events| -> EventStream { Box::pin(futures::stream::iter(events)) })
        }
    }

    fn silence() -> Result<Vec<StreamEvent>, ProviderError> {
        Ok(vec![StreamEvent::Done {
            reason: StopReason::Stop,
            message: assistant(vec![], StopReason::Stop),
        }])
    }

    fn says(text: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        Ok(vec![
            StreamEvent::TextDelta {
                content_index: 0,
                delta: text.into(),
            },
            StreamEvent::Done {
                reason: StopReason::Stop,
                message: assistant(
                    vec![AssistantContentPart::Text(TextContent {
                        text: text.into(),
                    })],
                    StopReason::Stop,
                ),
            },
        ])
    }

    async fn ask(
        replies: Vec<Result<Vec<StreamEvent>, ProviderError>>,
    ) -> (Result<String, String>, Vec<LlmContext>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(Scripted {
            replies: Mutex::new(replies.into()),
            requests: Arc::clone(&requests),
        }));
        let context = side_question_context(live(), vec![user_text("deploy it".into())], "eh?");
        let answer = answer(
            &registry,
            &crate::testing::echo_model(),
            context,
            &StreamOptions::default(),
        )
        .await;
        let requests = requests.lock().unwrap().clone();
        (answer, requests)
    }

    #[tokio::test]
    async fn an_empty_reply_is_nudged_once_and_the_second_answer_lands() {
        let (answer, requests) = ask(vec![silence(), says("the deploy is still running")]).await;

        assert_eq!(answer, Ok("the deploy is still running".into()));
        assert_eq!(requests.len(), 2);
        let nudged = &requests[1].messages;
        assert_eq!(nudged.len(), requests[0].messages.len() + 1);
        assert!(matches!(nudged.last(), Some(Message::User(user)) if user.text() == NUDGE));
    }

    #[tokio::test]
    async fn a_reply_that_stays_empty_says_what_came_back() {
        let (answer, requests) = ask(vec![silence(), silence()]).await;

        assert_eq!(
            answer,
            Err(
                "the model replied with nothing at all (it ended its turn), again after a nudge"
                    .into()
            )
        );
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test]
    async fn a_failed_request_is_reported_as_it_is_and_not_nudged() {
        let (answer, requests) = ask(vec![Err(ProviderError::LoginExpired {
            provider: "anthropic".into(),
            account: mush_ai::types::AccountLabel::default_label(),
            reason: "refresh token not found or invalid".into(),
        })])
        .await;

        let error = answer.unwrap_err();
        assert!(error.contains("/login anthropic default"), "{error}");
        assert_eq!(requests.len(), 1);
    }
}
