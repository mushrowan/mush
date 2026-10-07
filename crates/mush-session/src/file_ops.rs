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
const MAX_ENTRIES: usize = 200;
const MAX_COMBINED_CHARS: usize = 6000;

#[must_use]
pub fn strip_file_lists(summary: &str) -> String {
    let mut out = summary.to_string();
    for (open, close) in [(READ_OPEN, READ_CLOSE), (MODIFIED_OPEN, MODIFIED_CLOSE)] {
        while let Some(start) = out.find(open) {
            let Some(end) = out[start..]
                .find(close)
                .map(|end| start + end + close.len())
            else {
                break;
            };
            let start = out[..start].trim_end_matches('\n').len();
            out.replace_range(start..end, "");
        }
    }
    out.trim_end().to_string()
}

pub(crate) fn is_summary_text(text: &str) -> bool {
    mush_ai::types::strip_compaction_summary_prefix(text).is_some()
        || text.starts_with(mush_ai::types::BRANCH_SUMMARY_PREFIX)
}

impl FileOperations {
    #[must_use]
    pub fn from_messages(messages: &[Message]) -> Self {
        let mut ops = Self::default();
        for message in messages {
            ops.absorb(message);
        }
        ops.read.retain(|path| !ops.modified.contains(path));
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
            Message::User(user) => {
                let text = user.text();
                if is_summary_text(&text) {
                    self.absorb_text(&text);
                }
            }
            Message::Event(event) if event.kind.is_summary() => self.absorb_text(&event.content),
            Message::Event(_) => {}
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

    fn capped_lists(&self) -> (Vec<&str>, Vec<&str>) {
        let mut read: Vec<&str> = self
            .read
            .iter()
            .filter(|path| !self.modified.contains(*path))
            .map(String::as_str)
            .take(MAX_ENTRIES)
            .collect();
        let mut modified: Vec<&str> = self
            .modified
            .iter()
            .map(String::as_str)
            .take(MAX_ENTRIES)
            .collect();
        let chars = |read: &[&str], modified: &[&str]| -> usize {
            read.iter().chain(modified).map(|path| path.len() + 1).sum()
        };
        while !read.is_empty() && chars(&read, &modified) > MAX_COMBINED_CHARS {
            read.pop();
        }
        while !modified.is_empty() && chars(&read, &modified) > MAX_COMBINED_CHARS {
            modified.pop();
        }
        (read, modified)
    }

    #[must_use]
    pub fn render(&self) -> String {
        let (read, modified) = self.capped_lists();
        let mut out = String::new();
        for (open, close, paths) in [
            (READ_OPEN, READ_CLOSE, read),
            (MODIFIED_OPEN, MODIFIED_CLOSE, modified),
        ] {
            if paths.is_empty() {
                continue;
            }
            out.push_str("\n\n");
            out.push_str(open);
            out.push('\n');
            out.push_str(&paths.join("\n"));
            out.push('\n');
            out.push_str(close);
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
        let earlier = user(&compaction_summary_text(
            "work so far\n\n<read-files>\nold/read.rs\n</read-files>\n\n<modified-files>\nold/changed.rs\n</modified-files>",
        ));
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
        assert_eq!(
            FileOperations::from_messages(&[user(&compaction_summary_text(&rendered))]),
            ops
        );
        assert_eq!(FileOperations::default().render(), "");
    }

    #[test]
    fn pasted_file_list_tags_outside_a_summary_are_ignored() {
        let pasted = user("look at this <read-files>\nsecret.rs\n</read-files>");
        assert!(FileOperations::from_messages(&[pasted]).is_empty());
    }

    #[test]
    fn stripping_removes_both_blocks_and_their_leading_newlines() {
        let summary = "## Goal\nship it\n\n<read-files>\na\n</read-files>\n\n<modified-files>\nb\n</modified-files>\n";
        assert_eq!(strip_file_lists(summary), "## Goal\nship it");
        assert_eq!(strip_file_lists("no lists"), "no lists");
    }

    #[test]
    fn the_lists_are_capped_and_reads_drop_first() {
        let mut ops = FileOperations::default();
        for i in 0..300 {
            ops.modified.insert(format!("m{i:03}"));
        }
        let (read, modified) = ops.capped_lists();
        assert!(read.is_empty());
        assert_eq!(modified.len(), MAX_ENTRIES);

        let mut ops = FileOperations::default();
        let long = "x".repeat(99);
        for i in 0..50 {
            ops.read.insert(format!("r{i:02}{long}"));
            ops.modified.insert(format!("m{i:02}{long}"));
        }
        let (read, modified) = ops.capped_lists();
        assert_eq!(modified.len(), 50);
        assert_eq!(read.len(), 8);
        assert_eq!(read.last().copied(), Some(format!("r07{long}").as_str()));
        let ops = FileOperations {
            read: BTreeSet::new(),
            modified: (0..100).map(|i| format!("m{i:02}{long}")).collect(),
        };
        assert_eq!(ops.capped_lists().1.len(), 58);
    }
}
