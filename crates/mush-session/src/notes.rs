pub const NOTES_PREFIX: &str = "Notes you kept in this session:";

#[must_use]
pub fn fingerprint(text: &str) -> String {
    crate::harness::sha256_hex(text.trim_end().as_bytes())
}

#[must_use]
pub fn message(text: &str) -> mush_ai::types::Message {
    mush_ai::types::Message::User(
        mush_ai::types::UserMessage::builder()
            .content(mush_ai::types::UserContent::Text(format!(
                "{NOTES_PREFIX}\n\n<notes fingerprint=\"{}\">\nNotes you kept earlier with \
                 `rlm.note()`. They survive compaction and kernel restarts.\n\n{}\n</notes>",
                fingerprint(text),
                text.trim_end()
            )))
            .timestamp_ms(mush_ai::types::Timestamp::now())
            .build(),
    )
}

#[must_use]
pub fn extract(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix(NOTES_PREFIX)?.trim_start();
    let rest = rest.strip_prefix("<notes fingerprint=\"")?;
    let (fingerprint, rest) = rest.split_once("\">")?;
    let body = rest.trim().strip_suffix("</notes>")?.trim();
    Some((fingerprint.to_string(), body.to_string()))
}

#[must_use]
pub fn latest_fingerprint(messages: &[mush_ai::types::Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| match message {
        mush_ai::types::Message::User(user) => extract(&user.text()).map(|(fp, _)| fp),
        _ => None,
    })
}
