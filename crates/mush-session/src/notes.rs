use mush_ai::types::Message;

use crate::user_text::TaggedBlock;

pub const NOTES_PREFIX: &str = "Notes you kept in this session:";
pub const NOTES_UPDATE_PREFIX: &str = "Your notes changed since they were last shown:";

const NOTES: TaggedBlock = TaggedBlock {
    prefix: NOTES_PREFIX,
    tag: "notes",
};

const NOTES_UPDATE: TaggedBlock = TaggedBlock {
    prefix: NOTES_UPDATE_PREFIX,
    tag: "notes-update",
};

#[must_use]
pub fn fingerprint(text: &str) -> String {
    crate::harness::sha256_hex(text.trim_end().as_bytes())
}

#[must_use]
pub fn message(text: &str) -> Message {
    let body = text.trim_end();
    let body = if body.is_empty() { "(no notes)" } else { body };
    NOTES.message(
        &fingerprint(text),
        &format!(
            "Notes you kept earlier with `rlm.note()`. They survive compaction and kernel \
             restarts. Each is `- [key] text`; `rlm.note(text, key=...)` replaces a key and \
             `rlm.drop_note(key)` archives one.\n\n{body}"
        ),
    )
}

#[must_use]
pub fn update_message(fingerprint: &str, changes: &str) -> Message {
    NOTES_UPDATE.message(
        fingerprint,
        &format!(
            "Only the changed notes are listed; every other note stands as last shown.\n\n{}",
            changes.trim_end()
        ),
    )
}

#[must_use]
pub fn extract(text: &str) -> Option<(String, String)> {
    NOTES.extract(text).or_else(|| NOTES_UPDATE.extract(text))
}

#[must_use]
pub fn latest_fingerprint(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| match message {
        Message::User(user) => extract(&user.text()).map(|(fingerprint, _)| fingerprint),
        _ => None,
    })
}
