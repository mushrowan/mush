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

/// default timeout in seconds for foreground tools that wait for work to finish.
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;

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
pub fn builtin_tools(cwd: Arc<Path>, http_client: mush_ai::http::HttpClient) -> ToolRegistry {
    let mut tools =
        builtin_tools_with_options(cwd, None, false, http_client, Default::default(), None);
    add_batch_tool(&mut tools);
    tools
}

/// create built-in tools with an optional bash output sink for streaming
pub fn builtin_tools_with_sink(
    cwd: Arc<Path>,
    output_sink: Option<bash::OutputSink>,
    http_client: mush_ai::http::HttpClient,
) -> ToolRegistry {
    let mut tools = builtin_tools_with_options(
        cwd,
        output_sink,
        false,
        http_client,
        Default::default(),
        None,
    );
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
    http_client: mush_ai::http::HttpClient,
    forbidden_chars: forbidden_chars::ForbiddenChars,
    max_image_bytes: Option<usize>,
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
        Arc::new(read::ReadTool::new(cwd.clone()).with_max_image_bytes(max_image_bytes)),
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

/// reshape the model-dependent tools (apply_patch ↔ edit + write, batch)
/// to match the active model. callers build the registry once at
/// startup against an initial model, then call this on every stream so
/// that switching models mid-session (`/model gpt-5`, `/model claude-...`)
/// flips the toolset to whatever the current model expects.
///
/// this rebuilds apply_patch / edit / write from scratch with `cwd`
/// because tool instances embed the cwd into their state and we don't
/// have a way to re-key them. batch is rebuilt last so its inner
/// snapshot of the registry sees the final aligned shape
pub fn align_for_model(
    registry: &mut ToolRegistry,
    model: &mush_ai::types::Model,
    cwd: Arc<Path>,
    forbidden_chars: forbidden_chars::ForbiddenChars,
) {
    registry.remove("apply_patch");
    registry.remove("edit");
    registry.remove("write");
    registry.remove("batch");

    if model.uses_patch_tool() {
        let apply: SharedTool = Arc::new(apply_patch::ApplyPatchTool::new(cwd));
        registry.register_shared(forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
            apply,
            forbidden_chars,
            apply_patch_content_extractor(),
        ));
    } else {
        let write: SharedTool = Arc::new(write::WriteTool::new(cwd.clone()));
        registry.register_shared(forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
            write,
            forbidden_chars.clone(),
            write_content_extractor(),
        ));
        let edit: SharedTool = Arc::new(edit::EditTool::new(cwd));
        registry.register_shared(forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
            edit,
            forbidden_chars,
            edit_content_extractor(),
        ));
    }

    if !model.supports_native_parallel_calls() {
        add_batch_tool(registry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forbidden_chars::ForbiddenChars;

    fn http_client() -> mush_ai::http::HttpClient {
        mush_ai::http::client()
    }

    fn registry_with_chars(use_patch: bool, chars: ForbiddenChars) -> ToolRegistry {
        builtin_tools_with_options(
            Arc::from(std::env::temp_dir().as_path()),
            None,
            use_patch,
            http_client(),
            chars,
            None,
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

    fn model_with_id(id: &str) -> mush_ai::types::Model {
        use mush_ai::types::{
            Api, BaseUrl, InputModality, ModelCost, ModelId, Provider, TokenCount,
        };
        mush_ai::types::Model {
            id: ModelId::from(id),
            name: id.into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: BaseUrl::from("https://example.com"),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(200_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(8192),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        }
    }

    #[test]
    fn align_for_model_swaps_edit_tools_when_switching_to_patch_model() {
        // start with a claude-style registry (edit + write, no apply_patch).
        // align it to a gpt model. apply_patch should appear, edit + write
        // should disappear. covers the bug where switching model mid-session
        // leaves the original (non-patch) tool set in place
        let cwd: Arc<std::path::Path> = Arc::from(std::env::temp_dir().as_path());
        let mut registry = builtin_tools_with_options(
            cwd.clone(),
            None,
            false,
            http_client(),
            ForbiddenChars::empty(),
            None,
        );
        add_batch_tool(&mut registry);

        assert!(registry.get("edit").is_some(), "starting state has edit");
        assert!(registry.get("write").is_some(), "starting state has write");
        assert!(
            registry.get("apply_patch").is_none(),
            "starting state has no apply_patch"
        );

        let gpt_model = model_with_id("gpt-5");
        assert!(gpt_model.uses_patch_tool());

        align_for_model(
            &mut registry,
            &gpt_model,
            cwd.clone(),
            ForbiddenChars::empty(),
        );

        assert!(
            registry.get("apply_patch").is_some(),
            "after aligning to a patch model, apply_patch should be registered"
        );
        assert!(
            registry.get("edit").is_none(),
            "edit should be gone after aligning to a patch model"
        );
        assert!(
            registry.get("write").is_none(),
            "write should be gone after aligning to a patch model"
        );
    }

    #[test]
    fn align_for_model_swaps_back_to_edit_write_for_non_patch_model() {
        // mirror of the above: start with apply_patch (gpt-style) and align
        // to a claude model. apply_patch goes, edit + write come back
        let cwd: Arc<std::path::Path> = Arc::from(std::env::temp_dir().as_path());
        let mut registry = builtin_tools_with_options(
            cwd.clone(),
            None,
            true,
            http_client(),
            ForbiddenChars::empty(),
            None,
        );

        assert!(registry.get("apply_patch").is_some());
        assert!(registry.get("edit").is_none());

        let claude_model = model_with_id("claude-sonnet-4-5");
        assert!(!claude_model.uses_patch_tool());

        align_for_model(
            &mut registry,
            &claude_model,
            cwd.clone(),
            ForbiddenChars::empty(),
        );

        assert!(registry.get("edit").is_some());
        assert!(registry.get("write").is_some());
        assert!(registry.get("apply_patch").is_none());
    }

    #[test]
    fn align_for_model_toggles_batch_tool_with_native_parallel_support() {
        // models that support native parallel tool calls (gpt, codex,
        // anthropic) shouldn't carry the batch tool. claude (anthropic)
        // does support native parallel calls, but on openrouter slugs
        // that don't, batch should remain. cover both directions
        let cwd: Arc<std::path::Path> = Arc::from(std::env::temp_dir().as_path());
        let mut registry = builtin_tools_with_options(
            cwd.clone(),
            None,
            false,
            http_client(),
            ForbiddenChars::empty(),
            None,
        );
        add_batch_tool(&mut registry);
        assert!(registry.get("batch").is_some());

        // gpt supports native parallel calls → batch should drop
        let gpt = model_with_id("gpt-5");
        assert!(gpt.supports_native_parallel_calls());
        align_for_model(&mut registry, &gpt, cwd.clone(), ForbiddenChars::empty());
        assert!(
            registry.get("batch").is_none(),
            "batch should be removed for native-parallel models"
        );

        // align to a non-native model brings batch back
        let mut openrouter_glm = model_with_id("z-ai/glm-4.6");
        openrouter_glm.provider = mush_ai::types::Provider::OpenRouter;
        assert!(!openrouter_glm.supports_native_parallel_calls());
        align_for_model(&mut registry, &openrouter_glm, cwd, ForbiddenChars::empty());
        assert!(
            registry.get("batch").is_some(),
            "batch should be re-added for non-native-parallel models"
        );
    }
}
