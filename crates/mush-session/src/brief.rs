use mush_ai::types::{Message, Timestamp};

use crate::user_text::user_text;

pub const TASK_LABEL: &str = "[task from parent]";
pub const RESTATED_LABEL: &str = "[task from parent, kept through a compaction]";
const MAX_RESTATED_CHARS: usize = 80_000;
const RESTATED_TAIL_CHARS: usize = 16_000;

#[must_use]
pub fn is_task(text: &str) -> bool {
    text.strip_prefix(TASK_LABEL)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('\n'))
}

#[must_use]
pub fn is_restated(text: &str) -> bool {
    text.starts_with(RESTATED_LABEL)
}

#[must_use]
pub fn restated(task: &str) -> Message {
    let brief = bounded(task.strip_prefix(TASK_LABEL).unwrap_or(task).trim_start());
    user_text(
        format!(
            "{RESTATED_LABEL}\n\nYour context was compacted, so this is the task your parent \
             gave you, word for word. The summary and the messages before this one show how far \
             you got: carry on from there instead of starting over.\n\n{brief}"
        ),
        Timestamp::now(),
    )
}

fn bounded(brief: &str) -> String {
    let total = brief.chars().count();
    if total <= MAX_RESTATED_CHARS {
        return brief.to_string();
    }
    let head_chars = MAX_RESTATED_CHARS - RESTATED_TAIL_CHARS;
    let head: String = brief.chars().take(head_chars).collect();
    let tail: String = brief.chars().skip(total - RESTATED_TAIL_CHARS).collect();
    format!(
        "{head}\n\n[... {} characters of the task are left out here; the summary covers \
         them ...]\n\n{tail}",
        total - head_chars - RESTATED_TAIL_CHARS
    )
}

#[must_use]
pub fn in_context(task: &str, messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::User(user) => {
            let text = user.text();
            text == task || is_restated(&text)
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(message: &Message) -> String {
        let Message::User(user) = message else {
            panic!("{message:?}")
        };
        user.text()
    }

    #[test]
    fn a_restated_task_keeps_the_brief_word_for_word_and_is_not_a_new_task() {
        let task = "[task from parent]\n\nwrite the report\nnever touch ~/.cache";

        let restated = text(&restated(task));

        assert!(is_task(task));
        assert!(is_restated(&restated));
        assert!(!is_task(&restated));
        assert!(restated.ends_with("\n\nwrite the report\nnever touch ~/.cache"));
        assert!(!is_task("[task from parents]\n\nnot one"));
        assert!(!is_task("a message that mentions [task from parent]"));
    }

    #[test]
    fn a_huge_task_is_restated_with_its_start_and_end() {
        let task = format!(
            "[task from parent]\n\nstart {} end",
            "é".repeat(2 * MAX_RESTATED_CHARS)
        );

        let restated = text(&restated(&task));

        assert!(restated.chars().count() < MAX_RESTATED_CHARS + 1_000);
        assert!(restated.contains("start "));
        assert!(restated.ends_with(" end"));
        assert!(restated.contains("characters of the task are left out here"));
    }

    #[test]
    fn the_task_is_in_context_as_itself_or_as_a_restatement() {
        let task = "[task from parent]\n\nwrite the report";
        let other = user_text("something else".into(), Timestamp::zero());

        assert!(in_context(
            task,
            &[user_text(task.into(), Timestamp::zero())]
        ));
        assert!(in_context(task, &[other.clone(), restated(task)]));
        assert!(!in_context(task, &[other]));
        assert!(!in_context(task, &[]));
    }
}
