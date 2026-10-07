use mush_ai::types::Message;

use crate::user_text::TaggedBlock;

pub const NOTES_PREFIX: &str = "Notes you kept in this session:";

const NOTES: TaggedBlock = TaggedBlock {
    prefix: NOTES_PREFIX,
    tag: "notes",
};

#[must_use]
pub fn fingerprint(text: &str) -> String {
    crate::harness::sha256_hex(text.trim_end().as_bytes())
}

#[must_use]
pub fn message(text: &str) -> Message {
    NOTES.message(
        &fingerprint(text),
        &format!(
            "Notes you kept earlier with `rlm.note()`. They survive compaction and kernel \
             restarts.\n\n{}",
            text.trim_end()
        ),
    )
}

#[must_use]
pub fn extract(text: &str) -> Option<(String, String)> {
    NOTES.extract(text)
}

#[must_use]
pub fn latest_fingerprint(messages: &[Message]) -> Option<String> {
    NOTES.latest_fingerprint(messages)
}
