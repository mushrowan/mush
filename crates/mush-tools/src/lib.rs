pub mod apply_patch;
pub mod background;
pub mod bash;
pub mod bash_status;
pub mod batch;
pub mod edit;
pub mod find;
pub mod forbidden_chars;
pub mod glob;
pub mod grep;
pub mod ls;
pub mod notify_user;
pub mod read;
pub mod repo_map;
pub mod skills;
pub mod util;
pub mod web_fetch;
pub mod web_search;
pub mod write;

use std::path::Path;
use std::sync::Arc;

use mush_agent::tool::{SharedTool, ToolRegistry};

/// whether a model id should use the codex patch format instead of edit + write.
/// GPT models (except gpt-4 and oss variants) are trained on the patch format
/// and produce better edits with it.
#[deprecated(note = "use Model::uses_patch_tool() instead")]
pub fn uses_patch_tool(model_id: &str) -> bool {
    model_id.contains("gpt-") && !model_id.contains("oss") && !model_id.contains("gpt-4")
}

/// whether a model supports native parallel tool calls and doesn't need the
/// batch tool. GPT/codex models use the responses API which handles parallel
/// calls natively, and anthropic does too.
#[deprecated(note = "use Model::supports_native_parallel_calls() instead")]
pub fn supports_native_parallel_calls(model_id: &str) -> bool {
    model_id.contains("gpt-")
        || model_id.contains("codex")
        || model_id.starts_with("o1")
        || model_id.starts_with("o3")
        || model_id.starts_with("o4")
}

/// create the full set of built-in tools for a given working directory
pub fn builtin_tools(cwd: Arc<Path>, http_client: reqwest::Client) -> ToolRegistry {
    let mut tools = builtin_tools_with_options(cwd, None, false, http_client, Default::default());
    add_batch_tool(&mut tools);
    tools
}

/// create built-in tools with an optional bash output sink for streaming
pub fn builtin_tools_with_sink(
    cwd: Arc<Path>,
    output_sink: Option<bash::OutputSink>,
    http_client: reqwest::Client,
) -> ToolRegistry {
    let mut tools =
        builtin_tools_with_options(cwd, output_sink, false, http_client, Default::default());
    add_batch_tool(&mut tools);
    tools
}

/// create built-in tools with all options.
/// when `use_patch` is true, apply_patch replaces edit + write (for GPT models).
///
/// `forbidden_chars` is enforced uniformly across `write`, `edit`, and
/// `apply_patch` via the [`forbidden_chars::ForbiddenCharsGuard`]
/// wrapper. when the validator is empty, the guard is a no-op and the
/// inner tools are registered unwrapped.
///
/// does NOT include the batch tool. call `add_batch_tool` after registering
/// all additional tools (skill, MCP, LSP) so batch can access them all.
pub fn builtin_tools_with_options(
    cwd: Arc<Path>,
    output_sink: Option<bash::OutputSink>,
    use_patch: bool,
    http_client: reqwest::Client,
    forbidden_chars: forbidden_chars::ForbiddenChars,
) -> ToolRegistry {
    let bg_registry = background::BackgroundJobRegistry::new();

    let bash_tool: SharedTool = {
        let mut tool =
            bash::BashTool::new(cwd.clone()).with_background_registry(bg_registry.clone());
        if let Some(ref sink) = output_sink {
            tool = tool.with_output_sink(sink.clone());
        }
        Arc::new(tool)
    };

    let mut tools: Vec<SharedTool> = vec![
        Arc::new(read::ReadTool::new(cwd.clone())),
        bash_tool,
        Arc::new(bash_status::BashStatusTool::new(bg_registry)),
        Arc::new(grep::GrepTool::new(cwd.clone())),
        Arc::new(find::FindTool::new(cwd.clone())),
        Arc::new(glob::GlobTool::new(cwd.clone())),
        Arc::new(ls::LsTool::new(cwd.clone())),
        Arc::new(web_search::WebSearchTool::new(http_client.clone())),
        Arc::new(web_fetch::WebFetchTool::new(http_client.clone())),
        Arc::new(notify_user::NotifyUserTool::new()),
    ];

    if use_patch {
        let apply: SharedTool = Arc::new(apply_patch::ApplyPatchTool::new(cwd));
        tools.push(forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
            apply,
            forbidden_chars,
            apply_patch_content_extractor(),
        ));
    } else {
        let write: SharedTool = Arc::new(write::WriteTool::new(cwd.clone()));
        tools.push(forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
            write,
            forbidden_chars.clone(),
            write_content_extractor(),
        ));
        let edit: SharedTool = Arc::new(edit::EditTool::new(cwd));
        tools.push(forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
            edit,
            forbidden_chars,
            edit_content_extractor(),
        ));
    }

    ToolRegistry::from_shared(tools)
}

/// extract the content payload from a `write` call's args
fn write_content_extractor() -> forbidden_chars::ContentExtractor {
    Arc::new(|args: &serde_json::Value| {
        args.get("content")
            .and_then(|v| v.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default()
    })
}

/// extract `newText` from an `edit` call's args. only the new text is
/// validated; `oldText` is left alone so the model can still match
/// against existing forbidden content (e.g. to clean it up)
fn edit_content_extractor() -> forbidden_chars::ContentExtractor {
    Arc::new(|args: &serde_json::Value| {
        args.get("newText")
            .and_then(|v| v.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default()
    })
}

/// extract added/replacement content from an `apply_patch` call's
/// args. parses the patch and gathers every line being introduced
/// or replaced (full body for `Add`, `new_lines` for each chunk in
/// `Update`). on parse failure returns empty so the inner tool can
/// surface its own parse error
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

/// add the batch tool to a registry, giving it access to all currently
/// registered tools. call this after all tools have been added so batch
/// can dispatch to skill, MCP, LSP tools etc.
pub fn add_batch_tool(registry: &mut ToolRegistry) {
    registry.register_shared(Arc::new(batch::BatchTool::new(registry.clone())));
}

#[cfg(test)]
mod tests {
    use super::*;
    use forbidden_chars::ForbiddenChars;

    fn http_client() -> reqwest::Client {
        reqwest::Client::new()
    }

    fn registry_with_chars(use_patch: bool, chars: ForbiddenChars) -> ToolRegistry {
        builtin_tools_with_options(
            Arc::from(std::env::temp_dir().as_path()),
            None,
            use_patch,
            http_client(),
            chars,
        )
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
    async fn allow_forbidden_chars_bypass_works_uniformly() {
        // every guarded tool must honour the same per-call override key
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
        // when no chars are configured, the schema must NOT advertise
        // allow_forbidden_chars (and the inner tool's deny_unknown_fields
        // should reject it if accidentally passed). this catches
        // regressions where the wrapper is applied unconditionally.
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
