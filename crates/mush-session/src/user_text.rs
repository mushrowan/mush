use mush_ai::types::{Message, Timestamp, UserContent, UserMessage};

pub(crate) fn user_text(text: String, timestamp_ms: Timestamp) -> Message {
    Message::User(
        UserMessage::builder()
            .content(UserContent::Text(text))
            .timestamp_ms(timestamp_ms)
            .build(),
    )
}

pub(crate) struct TaggedBlock {
    pub prefix: &'static str,
    pub tag: &'static str,
}

impl TaggedBlock {
    pub(crate) fn message(&self, fingerprint: &str, body: &str) -> Message {
        let Self { prefix, tag } = self;
        user_text(
            format!("{prefix}\n\n<{tag} fingerprint=\"{fingerprint}\">\n{body}\n</{tag}>"),
            Timestamp::now(),
        )
    }

    pub(crate) fn extract(&self, text: &str) -> Option<(String, String)> {
        let rest = text
            .strip_prefix(self.prefix)?
            .trim_start()
            .strip_prefix('<')?
            .strip_prefix(self.tag)?
            .strip_prefix(" fingerprint=\"")?;
        let (fingerprint, rest) = rest.split_once("\">")?;
        let body = rest
            .trim()
            .strip_suffix('>')?
            .strip_suffix(self.tag)?
            .strip_suffix("</")?
            .trim();
        Some((fingerprint.to_string(), body.to_string()))
    }

    pub(crate) fn latest_fingerprint(&self, messages: &[Message]) -> Option<String> {
        messages.iter().rev().find_map(|message| match message {
            Message::User(user) => self
                .extract(&user.text())
                .map(|(fingerprint, _)| fingerprint),
            _ => None,
        })
    }
}
