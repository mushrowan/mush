use crate::app::App;

pub fn ask_side_question(app: &mut App, question: &str) {
    app.push_system_message(format!("btw: {question}"));
    app.status = Some("answering the side question\u{2026}".into());
}

pub fn show_side_answer(app: &mut App, question: &str, answer: Result<String, String>) {
    app.status = None;
    match answer {
        Ok(answer) => app.push_system_message(format!("btw: {question}\n\n{answer}")),
        Err(error) => app.push_system_message(format!("side question failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_lands_as_a_labelled_system_message() {
        let mut app = App::new("test".into(), mush_ai::types::TokenCount::new(200_000));

        show_side_answer(&mut app, "why?", Ok("because".into()));

        let last = app.messages.iter().last().unwrap();
        assert_eq!(last.content, "btw: why?\n\nbecause");
        assert!(app.status.is_none());
    }
}
