use crate::app::{App, MessageRole};

fn pending_text(question: &str) -> String {
    format!("btw: {question}\n\nanswering the side question\u{2026}")
}

pub fn ask_side_question(app: &mut App, question: &str) {
    app.push_system_message(pending_text(question));
}

pub fn show_side_answer(app: &mut App, question: &str, answer: Result<String, String>) {
    let text = match answer {
        Ok(answer) => format!("btw: {question}\n\n{answer}"),
        Err(error) => format!("btw: {question}\n\nside question failed: {error}"),
    };
    let pending = pending_text(question);
    if let Some(mut message) = app
        .messages
        .iter_mut()
        .rev()
        .find(|m| m.role == MessageRole::System && m.content == pending)
    {
        message.content = text;
        return;
    }
    app.push_system_message(text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new("test".into(), mush_ai::types::TokenCount::new(200_000))
    }

    #[test]
    fn the_answer_lands_as_a_labelled_system_message() {
        let mut app = app();

        show_side_answer(&mut app, "why?", Ok("because".into()));

        let last = app.messages.iter().last().unwrap();
        assert_eq!(last.content, "btw: why?\n\nbecause");
        assert!(app.status.is_none());
    }

    #[test]
    fn the_question_shows_once_and_the_answer_replaces_the_wait() {
        let mut app = app();

        ask_side_question(&mut app, "why?");
        app.flush_status();
        show_side_answer(&mut app, "why?", Ok("because".into()));

        let contents: Vec<_> = app.messages.iter().map(|m| m.content.clone()).collect();
        assert_eq!(contents, vec!["btw: why?\n\nbecause".to_string()]);
    }

    #[test]
    fn a_failure_replaces_the_wait_too() {
        let mut app = app();

        ask_side_question(&mut app, "why?");
        show_side_answer(&mut app, "why?", Err("offline".into()));

        let contents: Vec<_> = app.messages.iter().map(|m| m.content.clone()).collect();
        assert_eq!(
            contents,
            vec!["btw: why?\n\nside question failed: offline".to_string()]
        );
    }
}
