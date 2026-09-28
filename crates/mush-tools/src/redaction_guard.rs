use std::path::Path;
use std::sync::Arc;

use mush_agent::redact::{MARKER_PREFIX, marker_count};
use mush_agent::tool::{AgentTool, OutputLimit, SharedTool, ToolResult};
use serde_json::Value;

pub const ALLOW_REDACTION_MARKER_KEY: &str = "allow_redaction_marker";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedTool {
    Write,
    Edit,
    ApplyPatch,
}

pub struct RedactionMarkerGuard {
    inner: SharedTool,
    kind: GuardedTool,
    cwd: Arc<Path>,
}

impl RedactionMarkerGuard {
    pub fn wrap(inner: SharedTool, kind: GuardedTool, cwd: Arc<Path>) -> SharedTool {
        Arc::new(Self { inner, kind, cwd })
    }

    fn file_text(&self, args: &Value) -> String {
        args.get("path")
            .or_else(|| args.get("file_path"))
            .and_then(Value::as_str)
            .and_then(|path| {
                std::fs::read_to_string(crate::util::resolve_path(&self.cwd, path)).ok()
            })
            .unwrap_or_default()
    }

    fn replacements(&self, args: &Value) -> Vec<(String, String)> {
        match self.kind {
            GuardedTool::Write => {
                let content = str_field(args, &["content"]).unwrap_or_default();
                vec![(self.file_text(args), content)]
            }
            GuardedTool::Edit => {
                let mut pairs = Vec::new();
                if let Some(new) = str_field(args, &["newText", "new_string"]) {
                    let old = str_field(args, &["oldText", "old_string"])
                        .unwrap_or_else(|| self.file_text(args));
                    pairs.push((old, new));
                }
                for edit in args
                    .get("edits")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(new) = str_field(edit, &["newText", "new_string"]) {
                        let old = str_field(edit, &["oldText", "old_string"]).unwrap_or_default();
                        pairs.push((old, new));
                    }
                }
                pairs
            }
            GuardedTool::ApplyPatch => {
                let Some(patch) = str_field(args, &["patch_text"]) else {
                    return Vec::new();
                };
                let Ok(hunks) = crate::apply_patch::parse_patch(&patch) else {
                    return Vec::new();
                };
                let mut pairs = Vec::new();
                for hunk in hunks {
                    match hunk {
                        crate::apply_patch::Hunk::Add { contents, .. } => {
                            pairs.push((String::new(), contents));
                        }
                        crate::apply_patch::Hunk::Update { chunks, .. } => {
                            for chunk in chunks {
                                pairs
                                    .push((chunk.old_lines.join("\n"), chunk.new_lines.join("\n")));
                            }
                        }
                        crate::apply_patch::Hunk::Delete { .. } => {}
                    }
                }
                pairs
            }
        }
    }
}

fn str_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
}

#[must_use]
pub fn introduced_marker(old: &str, new: &str) -> bool {
    marker_count(new) > marker_count(old)
}

fn rejection_message() -> String {
    format!(
        "refused: the new content adds a `{MARKER_PREFIX}...]` marker. mush replaces secrets in tool \
         output with that marker, so the file most likely holds a real value there and writing the \
         marker would overwrite it. edit around the secret instead, or set \
         `{ALLOW_REDACTION_MARKER_KEY}: true` if you really mean to write the marker text"
    )
}

fn augment_schema(mut schema: Value) -> Value {
    if let Some(Value::Object(props)) = schema.get_mut("properties") {
        props.insert(
            ALLOW_REDACTION_MARKER_KEY.into(),
            serde_json::json!({
                "type": "boolean",
                "description": "write content that adds a [REDACTED] marker anyway. defaults to \
                                false. the marker stands in for a hidden secret, so only set this \
                                when the marker text itself is wanted"
            }),
        );
    }
    schema
}

#[async_trait::async_trait]
impl AgentTool for RedactionMarkerGuard {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn description_for(&self, api: mush_ai::types::Api) -> &str {
        self.inner.description_for(api)
    }
    fn parameters_schema(&self) -> Value {
        augment_schema(self.inner.parameters_schema())
    }
    fn parameters_schema_for(&self, api: mush_ai::types::Api) -> Value {
        augment_schema(self.inner.parameters_schema_for(api))
    }
    fn output_limit(&self) -> OutputLimit {
        self.inner.output_limit()
    }

    async fn execute(&self, mut args: Value) -> ToolResult {
        let bypass = args
            .get(ALLOW_REDACTION_MARKER_KEY)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Value::Object(ref mut map) = args {
            map.remove(ALLOW_REDACTION_MARKER_KEY);
        }
        if !bypass
            && self
                .replacements(&args)
                .iter()
                .any(|(old, new)| introduced_marker(old, new))
        {
            return ToolResult::error(rejection_message());
        }
        self.inner.execute(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Recorder {
        received: Mutex<Option<Value>>,
    }

    #[async_trait::async_trait]
    impl AgentTool for Recorder {
        fn name(&self) -> &str {
            "record"
        }
        fn label(&self) -> &str {
            "Record"
        }
        fn description(&self) -> &str {
            "records args"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn output_limit(&self) -> OutputLimit {
            OutputLimit::Head
        }
        async fn execute(&self, args: Value) -> ToolResult {
            *self.received.lock().unwrap() = Some(args);
            ToolResult::text("ok")
        }
    }

    fn guarded(kind: GuardedTool, dir: &Path) -> (Arc<Recorder>, SharedTool) {
        let recorder = Arc::new(Recorder {
            received: Mutex::new(None),
        });
        let tool = RedactionMarkerGuard::wrap(recorder.clone(), kind, Arc::from(dir));
        (recorder, tool)
    }

    #[tokio::test]
    async fn edit_that_swaps_a_value_for_the_marker_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, tool) = guarded(GuardedTool::Edit, dir.path());
        let result = tool
            .execute(serde_json::json!({
                "path": "a.env",
                "oldText": "RESTIC_PASSWORD=old",
                "newText": "RESTIC_PASSWORD=[REDACTED]\nEXTRA=1",
            }))
            .await;
        assert!(result.outcome.is_error());
        assert!(recorder.received.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn edit_that_keeps_an_existing_marker_passes() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, tool) = guarded(GuardedTool::Edit, dir.path());
        let result = tool
            .execute(serde_json::json!({
                "path": "redact.rs",
                "edits": [{"oldText": "MARKER = \"[REDACTED]\"", "newText": "MARKER: &str = \"[REDACTED]\""}],
            }))
            .await;
        assert!(result.outcome.is_success());
        assert!(recorder.received.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn bypass_passes_and_is_not_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let (recorder, tool) = guarded(GuardedTool::Edit, dir.path());
        let result = tool
            .execute(serde_json::json!({
                "path": "a.env",
                "oldText": "x",
                "newText": "[REDACTED]",
                ALLOW_REDACTION_MARKER_KEY: true,
            }))
            .await;
        assert!(result.outcome.is_success());
        let received = recorder.received.lock().unwrap().clone().unwrap();
        assert!(
            received.get(ALLOW_REDACTION_MARKER_KEY).is_none(),
            "{received}"
        );
    }

    #[tokio::test]
    async fn line_range_edit_compares_against_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.env"), "A=[REDACTED:A]\n").unwrap();
        let (_, tool) = guarded(GuardedTool::Edit, dir.path());
        let kept = tool
            .execute(serde_json::json!({"path": "a.env", "start_line": 1, "end_line": 1, "newText": "A=[REDACTED:A]"}))
            .await;
        assert!(kept.outcome.is_success());
        let added = tool
            .execute(serde_json::json!({"path": "a.env", "start_line": 1, "end_line": 1, "newText": "A=[REDACTED:A] B=[REDACTED]"}))
            .await;
        assert!(added.outcome.is_error());
    }

    #[tokio::test]
    async fn write_compares_against_the_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let (_, tool) = guarded(GuardedTool::Write, dir.path());
        let fresh = tool
            .execute(serde_json::json!({"path": "new.env", "content": "TOKEN=[REDACTED]\n"}))
            .await;
        assert!(fresh.outcome.is_error());
        std::fs::write(dir.path().join("doc.md"), "values show as [REDACTED]\n").unwrap();
        let rewrite = tool
            .execute(
                serde_json::json!({"path": "doc.md", "content": "values now show as [REDACTED]\n"}),
            )
            .await;
        assert!(rewrite.outcome.is_success());
    }

    #[tokio::test]
    async fn patch_adding_a_marker_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (_, tool) = guarded(GuardedTool::ApplyPatch, dir.path());
        let patch = "*** Begin Patch\n*** Add File: a.env\n+TOKEN=[REDACTED]\n*** End Patch";
        let result = tool.execute(serde_json::json!({"patch_text": patch})).await;
        assert!(result.outcome.is_error());
    }

    #[test]
    fn schema_advertises_the_bypass() {
        let dir = tempfile::tempdir().unwrap();
        let (_, tool) = guarded(GuardedTool::Write, dir.path());
        assert!(tool.parameters_schema()["properties"][ALLOW_REDACTION_MARKER_KEY].is_object());
    }
}
