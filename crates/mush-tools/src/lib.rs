pub mod apply_patch;
pub mod attachment;
pub mod bash;
pub mod edit;
pub mod file_mutation_queue;
pub mod forbidden_chars;
pub mod ipython;
pub mod kernel_jobs;
pub mod notification;
pub mod orphans;
pub mod read;
pub mod read_ledger;
pub mod redaction_guard;
pub mod util;
pub mod web_search;
pub mod write;

#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
use mush_agent::tool::{SharedTool, ToolRegistry};

#[cfg(test)]
pub(crate) mod test_env {
    use std::ffi::OsString;
    use std::path::Path;

    static DATA_DIR_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub(crate) struct DataDir {
        previous: Option<OsString>,
        _lock: tokio::sync::MutexGuard<'static, ()>,
    }

    impl DataDir {
        pub(crate) async fn set(path: &Path) -> Self {
            let lock = DATA_DIR_LOCK.lock().await;
            let previous = std::env::var_os("MUSH_DATA_DIR");
            unsafe {
                std::env::set_var("MUSH_DATA_DIR", path);
            }
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for DataDir {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var("MUSH_DATA_DIR", previous);
                } else {
                    std::env::remove_var("MUSH_DATA_DIR");
                }
            }
        }
    }
}

#[cfg(test)]
fn apply_patch_tool(
    cwd: Arc<Path>,
    forbidden_chars: forbidden_chars::ForbiddenChars,
) -> SharedTool {
    let apply: SharedTool = Arc::new(apply_patch::ApplyPatchTool::new(cwd.clone()));
    forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
        redaction_guard::RedactionMarkerGuard::wrap(
            apply,
            redaction_guard::GuardedTool::ApplyPatch,
            cwd,
        ),
        forbidden_chars,
        apply_patch_content_extractor(),
        forbidden_chars::BypassVisibility::Advertised,
    )
}

#[cfg(test)]
fn write_tool(cwd: Arc<Path>, forbidden_chars: forbidden_chars::ForbiddenChars) -> SharedTool {
    let write: SharedTool = Arc::new(write::WriteTool::new(cwd.clone()));
    forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
        redaction_guard::RedactionMarkerGuard::wrap(
            write,
            redaction_guard::GuardedTool::Write,
            cwd,
        ),
        forbidden_chars,
        field_extractor("content"),
        forbidden_chars::BypassVisibility::Advertised,
    )
}

#[cfg(test)]
fn edit_tool(cwd: Arc<Path>, forbidden_chars: forbidden_chars::ForbiddenChars) -> SharedTool {
    let edit: SharedTool = Arc::new(edit::EditTool::new(cwd.clone()));
    forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
        redaction_guard::RedactionMarkerGuard::wrap(edit, redaction_guard::GuardedTool::Edit, cwd),
        forbidden_chars,
        edit_content_extractor(),
        forbidden_chars::BypassVisibility::Advertised,
    )
}

#[cfg(test)]
fn field_extractor(field: &'static str) -> forbidden_chars::ContentExtractor {
    Arc::new(move |args: &serde_json::Value| {
        args.get(field)
            .and_then(|v| v.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default()
    })
}

#[cfg(test)]
fn edit_content_extractor() -> forbidden_chars::ContentExtractor {
    fn new_text(args: &serde_json::Value) -> Option<&str> {
        ["newText", "new_string"]
            .iter()
            .find_map(|key| args.get(*key).and_then(|v| v.as_str()))
    }
    Arc::new(|args: &serde_json::Value| {
        let batch = args
            .get("edits")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(new_text);
        new_text(args)
            .into_iter()
            .chain(batch)
            .map(str::to_string)
            .collect()
    })
}

#[cfg(test)]
fn apply_patch_content_extractor() -> forbidden_chars::ContentExtractor {
    Arc::new(|args: &serde_json::Value| {
        let Some(patch) = args.get("patch_text").and_then(|v| v.as_str()) else {
            return Vec::new();
        };
        let Ok(hunks) = apply_patch::parse_patch(patch) else {
            return Vec::new();
        };
        let mut content = Vec::new();
        for hunk in hunks {
            match hunk {
                apply_patch::Hunk::Add { contents, .. } => content.push(contents),
                apply_patch::Hunk::Update { chunks, .. } => {
                    for chunk in chunks {
                        if !chunk.new_lines.is_empty() {
                            content.push(chunk.new_lines.join("\n"));
                        }
                    }
                }
                apply_patch::Hunk::Delete { .. } => {}
            }
        }
        content
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use forbidden_chars::ForbiddenChars;

    fn registry_with_chars(use_patch: bool, chars: ForbiddenChars) -> ToolRegistry {
        let cwd: Arc<Path> = Arc::from(std::env::temp_dir().as_path());
        let tools = if use_patch {
            vec![apply_patch_tool(cwd, chars)]
        } else {
            vec![
                write_tool(cwd.clone(), chars.clone()),
                edit_tool(cwd, chars),
            ]
        };
        ToolRegistry::from_shared(tools)
    }

    #[test]
    fn every_advertised_type_is_one_coercion_handles() {
        fn types(schema: &serde_json::Value, found: &mut Vec<String>) {
            match schema {
                serde_json::Value::Object(fields) => {
                    if let Some(name) = fields.get("type").and_then(|t| t.as_str()) {
                        found.push(name.to_string());
                    }
                    for value in fields.values() {
                        types(value, found);
                    }
                }
                serde_json::Value::Array(entries) => {
                    for entry in entries {
                        types(entry, found);
                    }
                }
                _ => {}
            }
        }

        const HANDLED: [&str; 6] = ["object", "array", "string", "integer", "number", "boolean"];
        let registry = registry_with_chars(true, ForbiddenChars::default());
        for tool in registry.iter() {
            let mut found = Vec::new();
            types(&tool.parameters_schema(), &mut found);
            for name in found {
                assert!(
                    HANDLED.contains(&name.as_str()),
                    "{} advertises {name}, which mush_agent::de::coerce_to_schema ignores",
                    tool.name()
                );
            }
        }
    }

    #[tokio::test]
    async fn write_call_is_guarded_by_forbidden_chars() {
        let registry = registry_with_chars(false, ForbiddenChars::new(['—']));
        let write = registry.get("write").expect("write tool registered");
        let result = write
            .execute(serde_json::json!({
                "path": "/tmp/should-not-be-written.txt",
                "content": "no — em dash allowed",
            }))
            .await;
        assert!(
            !result.outcome.is_success(),
            "write should be blocked by the guard"
        );
    }

    #[tokio::test]
    async fn edit_call_is_guarded_by_forbidden_chars() {
        let registry = registry_with_chars(false, ForbiddenChars::new(['—']));
        let edit = registry.get("edit").expect("edit tool registered");
        let result = edit
            .execute(serde_json::json!({
                "path": "/tmp/whatever.txt",
                "oldText": "old",
                "newText": "new — content",
            }))
            .await;
        assert!(
            !result.outcome.is_success(),
            "edit should be blocked by the guard before reaching the file"
        );
    }

    #[tokio::test]
    async fn edit_is_guarded_under_claude_code_names() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "one\ntwo\n").unwrap();
        let registry = registry_with_chars(false, ForbiddenChars::new(['—']));
        let edit = registry.get("edit").expect("edit tool registered");

        for args in [
            serde_json::json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "one",
                "new_string": "one — dash",
            }),
            serde_json::json!({
                "path": target.to_str().unwrap(),
                "edits": [{"old_string": "two", "new_string": "two — dash"}],
            }),
        ] {
            let result = edit.execute(args).await;
            assert!(!result.outcome.is_success(), "{result:?}");
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "one\ntwo\n");
    }

    #[tokio::test]
    async fn apply_patch_call_is_guarded_by_forbidden_chars() {
        let registry = registry_with_chars(true, ForbiddenChars::new(['—']));
        let apply = registry
            .get("apply_patch")
            .expect("apply_patch tool registered when use_patch is true");
        let patch = "\
*** Begin Patch
*** Add File: hello.txt
+plain hyphen line
+with — em dash
*** End Patch";
        let result = apply
            .execute(serde_json::json!({"patch_text": patch}))
            .await;
        assert!(
            !result.outcome.is_success(),
            "apply_patch should be blocked when added content has a forbidden char"
        );
    }

    #[tokio::test]
    async fn rejected_write_saves_content_and_leaves_target_untouched() {
        let data = tempfile::tempdir().unwrap();
        let _data_dir = test_env::DataDir::set(data.path()).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        let content = "fn main() {\n    // a → b\n}\n";
        let registry = registry_with_chars(
            false,
            ForbiddenChars::with_classes([], [forbidden_chars::CharClass::Arrows]),
        );
        let write = registry.get("write").expect("write tool registered");
        let result = write
            .execute(serde_json::json!({
                "path": target.to_str().unwrap(),
                "content": content,
            }))
            .await;

        assert!(!result.outcome.is_success());
        assert!(!target.exists(), "target must not be written");

        let msg = util::extract_text(&result);
        let scratch = rejected_path(&msg);
        assert_eq!(std::fs::read_to_string(scratch).unwrap(), content);
        assert!(msg.contains("line 2, column 10"), "{msg}");
        assert!(msg.contains("'→' (U+2192)"), "{msg}");
    }

    #[tokio::test]
    async fn rejected_edits_array_saves_the_failing_new_text() {
        let data = tempfile::tempdir().unwrap();
        let _data_dir = test_env::DataDir::set(data.path()).await;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "one\ntwo\n").unwrap();
        let registry = registry_with_chars(false, ForbiddenChars::new(['—']));
        let edit = registry.get("edit").expect("edit tool registered");
        let result = edit
            .execute(serde_json::json!({
                "path": target.to_str().unwrap(),
                "edits": [
                    {"oldText": "one", "newText": "1"},
                    {"oldText": "two", "newText": "two — with dash"},
                ],
            }))
            .await;

        assert!(!result.outcome.is_success());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "one\ntwo\n");

        let msg = util::extract_text(&result);
        let scratch = rejected_path(&msg);
        assert_eq!(std::fs::read_to_string(scratch).unwrap(), "two — with dash");
    }

    fn rejected_path(message: &str) -> &std::path::Path {
        std::path::Path::new(
            message
                .rsplit_once("rejected content saved to ")
                .expect("saved rejection path")
                .1,
        )
    }

    #[tokio::test]
    async fn allow_forbidden_chars_bypass_works_uniformly() {
        let registry = registry_with_chars(false, ForbiddenChars::new(['—']));
        let write = registry.get("write").expect("write tool registered");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("override.txt");
        let result = write
            .execute(serde_json::json!({
                "path": path.to_str().unwrap(),
                "content": "with — em dash",
                "allow_forbidden_chars": true,
            }))
            .await;
        assert!(
            result.outcome.is_success(),
            "allow_forbidden_chars should bypass the guard"
        );
    }

    #[tokio::test]
    async fn empty_forbidden_chars_does_not_wrap() {
        let registry = registry_with_chars(false, ForbiddenChars::empty());
        let write = registry.get("write").expect("write tool registered");
        let schema = write.parameters_schema();
        assert!(
            schema["properties"]["allow_forbidden_chars"].is_null(),
            "no validator → no override knob in the schema"
        );
    }

    #[test]
    fn guarded_schemas_advertise_override_uniformly() {
        let registry = registry_with_chars(false, ForbiddenChars::new(['—']));
        for tool_name in ["write", "edit"] {
            let tool = registry.get(tool_name).expect("tool registered");
            let schema = tool.parameters_schema();
            assert_eq!(
                schema["properties"]["allow_forbidden_chars"]["type"], "boolean",
                "{tool_name} schema should expose the override knob when the validator is configured"
            );
        }

        let registry = registry_with_chars(true, ForbiddenChars::new(['—']));
        let tool = registry.get("apply_patch").expect("apply_patch registered");
        let schema = tool.parameters_schema();
        assert_eq!(
            schema["properties"]["allow_forbidden_chars"]["type"], "boolean",
            "apply_patch schema should expose the override knob when the validator is configured"
        );
    }
}
