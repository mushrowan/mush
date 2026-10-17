use mush_ai::types::Message;

use crate::user_text::TaggedBlock;

pub const DIGEST_PREFIX: &str = "Decisions you have open with the user:";
pub const NEWS_PREFIX: &str = "Decision updates since your last turn:";

const DIGEST: TaggedBlock = TaggedBlock {
    prefix: DIGEST_PREFIX,
    tag: "decisions",
};

const NEWS: TaggedBlock = TaggedBlock {
    prefix: NEWS_PREFIX,
    tag: "decision-news",
};

#[must_use]
pub fn fingerprint(body: &str) -> String {
    crate::notes::fingerprint(body)
}

#[must_use]
pub fn digest_message(body: &str) -> Message {
    DIGEST.message(&fingerprint(body), body.trim_end())
}

#[must_use]
pub fn news_message(body: &str) -> Message {
    NEWS.message(&fingerprint(body), body.trim_end())
}

#[must_use]
pub fn extract(text: &str) -> Option<(String, String)> {
    DIGEST.extract(text).or_else(|| NEWS.extract(text))
}

#[must_use]
pub fn latest_digest_fingerprint(messages: &[Message]) -> Option<String> {
    DIGEST.latest_fingerprint(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(message: &Message) -> String {
        let Message::User(user) = message else {
            panic!("decision carriers are user messages")
        };
        user.text()
    }

    #[test]
    fn a_digest_round_trips_through_its_tag() {
        let message = digest_message("- [layout] which layout?\n");
        let (fingerprint, body) = extract(&text(&message)).unwrap();
        assert_eq!(body, "- [layout] which layout?");
        assert_eq!(fingerprint, super::fingerprint("- [layout] which layout?"));
        assert_eq!(
            latest_digest_fingerprint(std::slice::from_ref(&message)),
            Some(fingerprint)
        );
    }

    #[test]
    fn news_is_recognised_but_is_not_a_digest() {
        let message = news_message("- helper asked the user [push]");
        assert!(extract(&text(&message)).is_some());
        assert_eq!(latest_digest_fingerprint(&[message]), None);
    }

    #[test]
    fn both_carriers_are_left_out_of_a_summary() {
        for message in [digest_message("a"), news_message("b")] {
            assert!(crate::compact::is_regenerated_text(&text(&message)));
        }
    }
}
