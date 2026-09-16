use mush_ai::registry::{ApiRegistry, LlmContext};
use mush_ai::types::{Message, Model, StreamOptions, Timestamp, UserContent, UserMessage};

use crate::app::App;

const SIDE_QUESTION_PROMPT: &str = "\
The user has a side question about the conversation so far. Answer it directly and briefly \
from what is already in the conversation. Do not call tools, do not continue the task, and \
do not repeat the conversation back.";

pub struct PendingSideQuestion {
    pub question: String,
    pub task: tokio::task::JoinHandle<Result<String, mush_ai::oneshot::OneshotError>>,
}

pub fn side_question_context(
    system_prompt: Option<&str>,
    mut messages: Vec<Message>,
    question: &str,
) -> LlmContext {
    messages.push(Message::User(
        UserMessage::builder()
            .content(UserContent::Text(question.to_string()))
            .timestamp_ms(Timestamp::now())
            .build(),
    ));
    let system_prompt = match system_prompt {
        Some(base) => format!("{base}\n\n{SIDE_QUESTION_PROMPT}"),
        None => SIDE_QUESTION_PROMPT.to_string(),
    };
    LlmContext {
        system_prompt: Some(system_prompt),
        messages,
        tools: vec![],
    }
}

pub fn start_side_question(
    app: &mut App,
    registry: ApiRegistry,
    model: Model,
    options: StreamOptions,
    context: LlmContext,
    question: String,
) -> PendingSideQuestion {
    app.push_system_message(format!("btw: {question}"));
    app.status = Some("answering the side question…".into());
    let task = tokio::spawn(async move {
        mush_ai::oneshot::collect_text(&registry, &model, &context, &options).await
    });
    PendingSideQuestion { question, task }
}

pub fn finish_side_question(
    app: &mut App,
    question: &str,
    outcome: Result<Result<String, mush_ai::oneshot::OneshotError>, tokio::task::JoinError>,
) {
    app.status = None;
    match outcome {
        Ok(Ok(answer)) => app.push_system_message(format!("btw: {question}\n\n{answer}")),
        Ok(Err(error)) => app.push_system_message(format!("side question failed: {error}")),
        Err(error) => app.push_system_message(format!("side question task failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_context_ends_with_the_question_and_carries_no_tools() {
        let context = side_question_context(Some("base prompt"), vec![], "why is it slow?");

        assert!(context.tools.is_empty());
        assert_eq!(context.messages.len(), 1);
        match &context.messages[0] {
            Message::User(user) => assert_eq!(user.text(), "why is it slow?"),
            other => panic!("expected a user message, got {other:?}"),
        }
        let prompt = context.system_prompt.unwrap();
        assert!(prompt.starts_with("base prompt\n\n"));
        assert!(prompt.contains("Do not call tools"));
    }

    struct AnswerProvider;

    #[async_trait::async_trait]
    impl mush_ai::registry::ApiProvider for AnswerProvider {
        fn api(&self) -> mush_ai::types::Api {
            mush_ai::types::Api::AnthropicMessages
        }

        async fn stream(
            &self,
            model: &Model,
            context: &LlmContext,
            _options: &StreamOptions,
        ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
            assert!(context.tools.is_empty());
            let message = mush_ai::types::AssistantMessage::builder()
                .model(model.id.clone())
                .provider(model.provider.clone())
                .api(model.api)
                .timestamp_ms(Timestamp::zero())
                .build();
            Ok(Box::pin(futures::stream::iter(vec![
                mush_ai::stream::StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "because it re-reads the file".into(),
                },
                mush_ai::stream::StreamEvent::Done {
                    reason: mush_ai::types::StopReason::Stop,
                    message,
                },
            ])))
        }
    }

    #[tokio::test]
    async fn a_side_question_is_answered_without_touching_the_conversation() {
        let mut session = crate::session::Session::new(App::new(
            "test".into(),
            mush_ai::types::TokenCount::new(200_000),
        ));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(AnswerProvider));
        let mut model = mush_ai::models::all_models_with_user()
            .into_iter()
            .next()
            .unwrap();
        model.api = mush_ai::types::Api::AnthropicMessages;
        model.provider = mush_ai::types::Provider::Anthropic;
        let before = session.conversation.context_len();

        let context = side_question_context(None, session.conversation.context(), "why slow?");
        let pending = start_side_question(
            &mut session.app,
            registry,
            model,
            StreamOptions::default(),
            context,
            "why slow?".into(),
        );
        session.pending_side_questions.push(pending);
        while !session.poll_side_questions().await {
            tokio::task::yield_now().await;
        }

        assert_eq!(session.conversation.context_len(), before);
        let last = session.app.messages.iter().last().unwrap();
        assert_eq!(
            last.content,
            "btw: why slow?\n\nbecause it re-reads the file"
        );
    }

    #[test]
    fn the_answer_lands_as_a_labelled_system_message() {
        let mut app = App::new("test".into(), mush_ai::types::TokenCount::new(200_000));

        finish_side_question(&mut app, "why?", Ok(Ok("because".into())));

        let last = app.messages.iter().last().unwrap();
        assert_eq!(last.content, "btw: why?\n\nbecause");
        assert!(app.status.is_none());
    }
}
