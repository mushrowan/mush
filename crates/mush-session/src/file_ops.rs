use std::collections::BTreeSet;

use mush_ai::types::{AssistantContentPart, Message};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileOperations {
    pub read: BTreeSet<String>,
    pub modified: BTreeSet<String>,
}

const READ_OPEN: &str = "<read-files>";
const READ_CLOSE: &str = "</read-files>";
const MODIFIED_OPEN: &str = "<modified-files>";
const MODIFIED_CLOSE: &str = "</modified-files>";

impl FileOperations {
    #[must_use]
    pub fn from_messages(messages: &[Message]) -> Self {
        let mut ops = Self::default();
        for message in messages {
            ops.absorb(message);
        }
        ops.read = ops
            .read
            .iter()
            .filter(|path| !ops.modified.contains(*path))
            .cloned()
            .collect();
        ops
    }

    fn absorb(&mut self, message: &Message) {
        match message {
            Message::Assistant(assistant) => {
                for part in &assistant.content {
                    let AssistantContentPart::ToolCall(call) = part else {
                        continue;
                    };
                    let Some(path) = call.arguments.get("path").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    match call.name.as_str() {
                        "read" => {
                            self.read.insert(path.to_string());
                        }
                        "edit" | "write" | "apply_patch" => {
                            self.modified.insert(path.to_string());
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(result) => {
                let diffs = result
                    .details
                    .as_ref()
                    .and_then(|details| details.get("diffs"))
                    .and_then(|diffs| diffs.as_array());
                for diff in diffs.into_iter().flatten() {
                    if let Some(path) = diff.get("path").and_then(|v| v.as_str()) {
                        self.modified.insert(path.to_string());
                    }
                }
            }
            Message::User(user) => self.absorb_text(&user.text()),
            Message::Event(event) => self.absorb_text(&event.content),
        }
    }

    fn absorb_text(&mut self, text: &str) {
        self.read.extend(block(text, READ_OPEN, READ_CLOSE));
        self.modified
            .extend(block(text, MODIFIED_OPEN, MODIFIED_CLOSE));
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.modified.is_empty()
    }

    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.read.is_empty() {
            out.push_str("\n\n");
            out.push_str(READ_OPEN);
            out.push('\n');
            out.push_str(&self.read.iter().cloned().collect::<Vec<_>>().join("\n"));
            out.push('\n');
            out.push_str(READ_CLOSE);
        }
        if !self.modified.is_empty() {
            out.push_str("\n\n");
            out.push_str(MODIFIED_OPEN);
            out.push('\n');
            out.push_str(&self.modified.iter().cloned().collect::<Vec<_>>().join("\n"));
            out.push('\n');
            out.push_str(MODIFIED_CLOSE);
        }
        out
    }
}

fn block(text: &str, open: &str, close: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(close) else { break };
        found.extend(
            after[..end]
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string),
        );
        rest = &after[end + close.len()..];
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::*;

    fn call(name: &str, path: &str) -> Message {
        Message::Assistant(
            AssistantMessage::builder()
                .content(vec![AssistantContentPart::ToolCall(ToolCall {
                    id: ToolCallId::from("c1"),
                    name: ToolName::from(name),
                    arguments: serde_json::json!({ "path": path }),
                })])
                .model("m".into())
                .provider(Provider::Anthropic)
                .api(Api::AnthropicMessages)
                .stop_reason(StopReason::ToolUse)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    fn cell_result(paths: &[&str]) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: "ipython".into(),
            content: vec![],
            outcome: ToolOutcome::Success,
            timestamp_ms: Timestamp::zero(),
            details: Some(serde_json::json!({
                "diffs": paths.iter().map(|p| serde_json::json!({ "path": p, "diff": "" })).collect::<Vec<_>>()
            })),
        })
    }

    fn user(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    #[test]
    fn edits_from_cells_and_classic_tools_land_in_modified() {
        let ops = FileOperations::from_messages(&[
            call("read", "src/a.rs"),
            call("edit", "src/b.rs"),
            cell_result(&["src/c.rs", "src/b.rs"]),
        ]);
        assert_eq!(
            ops.modified.iter().collect::<Vec<_>>(),
            vec!["src/b.rs", "src/c.rs"]
        );
        assert_eq!(ops.read.iter().collect::<Vec<_>>(), vec!["src/a.rs"]);
    }

    #[test]
    fn a_read_that_was_later_modified_counts_as_modified_only() {
        let ops = FileOperations::from_messages(&[call("read", "x.rs"), call("write", "x.rs")]);
        assert!(ops.read.is_empty());
        assert_eq!(ops.modified.iter().collect::<Vec<_>>(), vec!["x.rs"]);
    }

    #[test]
    fn an_earlier_summary_carries_its_lists_forward() {
        let earlier = user(
            "<summary>\nwork so far\n\n<read-files>\nold/read.rs\n</read-files>\n\n<modified-files>\nold/changed.rs\n</modified-files>\n</summary>",
        );
        let ops = FileOperations::from_messages(&[earlier, cell_result(&["new.rs"])]);
        assert_eq!(ops.read.iter().collect::<Vec<_>>(), vec!["old/read.rs"]);
        assert_eq!(
            ops.modified.iter().collect::<Vec<_>>(),
            vec!["new.rs", "old/changed.rs"]
        );
    }

    #[test]
    fn render_roundtrips_through_from_messages() {
        let ops = FileOperations::from_messages(&[call("read", "a"), call("edit", "b")]);
        let rendered = ops.render();
        assert_eq!(
            rendered,
            "\n\n<read-files>\na\n</read-files>\n\n<modified-files>\nb\n</modified-files>"
        );
        assert_eq!(FileOperations::from_messages(&[user(&rendered)]), ops);
        assert_eq!(FileOperations::default().render(), "");
    }
}
