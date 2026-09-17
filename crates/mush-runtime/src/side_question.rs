use mush_ai::registry::LlmContext;
use mush_ai::types::{Message, Timestamp, UserContent, UserMessage};

const SIDE_QUESTION_PROMPT: &str = "\
The user has a side question about the conversation so far. Answer it directly and briefly \
from what is already in the conversation. Do not call tools, do not continue the task, and \
do not repeat the conversation back.";

#[must_use]
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
}
