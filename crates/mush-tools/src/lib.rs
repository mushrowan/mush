pub mod apply_patch;
pub mod background;
pub mod bash;
pub mod bash_status;
pub mod batch;
pub mod edit;
pub mod file_mutation_queue;
pub mod find;
pub mod forbidden_chars;
pub mod glob;
pub mod grep;
pub mod ipython;
pub mod kernel_jobs;
pub mod ls;
pub mod notification;
pub mod notify_user;
pub mod orphans;
pub mod read;
pub mod read_ledger;
pub mod repo_map;
pub mod skills;
pub mod util;
pub mod web_fetch;
pub mod web_search;
pub mod write;

use std::path::Path;
use std::sync::Arc;

use mush_agent::tool::{SharedTool, ToolRegistry};

pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;

#[deprecated(note = "use Model::uses_patch_tool() instead")]
pub fn uses_patch_tool(model_id: &str) -> bool {
    model_id.contains("gpt-") && !model_id.contains("oss") && !model_id.contains("gpt-4")
}

#[deprecated(note = "use Model::supports_native_parallel_calls() instead")]
pub fn supports_native_parallel_calls(model_id: &str) -> bool {
    model_id.contains("gpt-")
        || model_id.contains("codex")
        || model_id.starts_with("o1")
        || model_id.starts_with("o3")
        || model_id.starts_with("o4")
}

pub fn builtin_tools(cwd: Arc<Path>, http_client: mush_ai::http::HttpClient) -> ToolRegistry {
    let mut tools = builtin_tools_with_options(
        cwd,
        None,
        false,
        http_client,
        Default::default(),
        None,
        Default::default(),
    );
    add_batch_tool(&mut tools);
    tools
}

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
        Default::default(),
    );
    add_batch_tool(&mut tools);
    tools
}

#[allow(clippy::too_many_arguments)]
pub fn builtin_tools_with_options(
    cwd: Arc<Path>,
    output_sink: Option<bash::OutputSink>,
    use_patch: bool,
    http_client: mush_ai::http::HttpClient,
    forbidden_chars: forbidden_chars::ForbiddenChars,
    max_image_bytes: Option<usize>,
    web_search: web_search::WebSearchConfig,
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
        forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
            bash_tool,
            forbidden_chars.clone(),
            bash_content_extractor(),
            forbidden_chars::BypassVisibility::Hidden,
        ),
        Arc::new(bash_status::BashStatusTool::new(bg_registry)),
        Arc::new(grep::GrepTool::new(cwd.clone())),
        Arc::new(find::FindTool::new(cwd.clone())),
        Arc::new(glob::GlobTool::new(cwd.clone())),
        Arc::new(ls::LsTool::new(cwd.clone())),
        Arc::new(web_search::WebSearchTool::with_config(
            http_client.clone(),
            &web_search,
        )),
        Arc::new(web_fetch::WebFetchTool::new(http_client.clone())),
        Arc::new(notify_user::NotifyUserTool::new()),
    ];

    if use_patch {
        let apply: SharedTool = Arc::new(apply_patch::ApplyPatchTool::new(cwd));
        tools.push(forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
            apply,
            forbidden_chars,
            apply_patch_content_extractor(),
            forbidden_chars::BypassVisibility::Advertised,
        ));
    } else {
        let write: SharedTool = Arc::new(write::WriteTool::new(cwd.clone()));
        tools.push(forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
            write,
            forbidden_chars.clone(),
            write_content_extractor(),
            forbidden_chars::BypassVisibility::Advertised,
        ));
        let edit: SharedTool = Arc::new(edit::EditTool::new(cwd));
        tools.push(forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
            edit,
            forbidden_chars,
            edit_content_extractor(),
            forbidden_chars::BypassVisibility::Advertised,
        ));
    }

    ToolRegistry::from_shared(tools)
}

fn bash_content_extractor() -> forbidden_chars::ContentExtractor {
    Arc::new(|args: &serde_json::Value| {
        args.get("command")
            .and_then(|v| v.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default()
    })
}

fn write_content_extractor() -> forbidden_chars::ContentExtractor {
    Arc::new(|args: &serde_json::Value| {
        args.get("content")
            .and_then(|v| v.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default()
    })
}

fn edit_content_extractor() -> forbidden_chars::ContentExtractor {
    Arc::new(|args: &serde_json::Value| {
        let top = args.get("newText").and_then(|v| v.as_str());
        let batch = args
            .get("edits")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|edit| edit.get("newText").and_then(|v| v.as_str()));
        top.into_iter().chain(batch).map(str::to_string).collect()
    })
}

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

pub fn add_batch_tool(registry: &mut ToolRegistry) {
    registry.register_shared(Arc::new(batch::BatchTool::new(registry.clone())));
}

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
            forbidden_chars::BypassVisibility::Advertised,
        ));
    } else {
        let write: SharedTool = Arc::new(write::WriteTool::new(cwd.clone()));
        registry.register_shared(forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
            write,
            forbidden_chars.clone(),
            write_content_extractor(),
            forbidden_chars::BypassVisibility::Advertised,
        ));
        let edit: SharedTool = Arc::new(edit::EditTool::new(cwd));
        registry.register_shared(forbidden_chars::ForbiddenCharsGuard::maybe_wrap(
            edit,
            forbidden_chars,
            edit_content_extractor(),
            forbidden_chars::BypassVisibility::Advertised,
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
            Default::default(),
        )
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
        unsafe {
            std::env::set_var("MUSH_DATA_DIR", data.path());
        }
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

        let saved: Vec<_> = std::fs::read_dir(data.path().join("tool-output"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let [scratch] = saved.as_slice() else {
            panic!("expected exactly one scratch file, got {saved:?}");
        };
        assert_eq!(std::fs::read_to_string(scratch).unwrap(), content);

        let msg = result_text(&result);
        assert!(
            msg.contains(&scratch.display().to_string()),
            "error should name the scratch file: {msg}"
        );
        assert!(msg.contains("line 2, column 10"), "{msg}");
        assert!(msg.contains("'→' (U+2192)"), "{msg}");
    }

    #[tokio::test]
    async fn rejected_edits_array_saves_the_failing_new_text() {
        let data = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MUSH_DATA_DIR", data.path());
        }
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

        let saved: Vec<_> = std::fs::read_dir(data.path().join("tool-output"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let [scratch] = saved.as_slice() else {
            panic!("expected exactly one scratch file, got {saved:?}");
        };
        assert_eq!(std::fs::read_to_string(scratch).unwrap(), "two — with dash");
        assert!(result_text(&result).contains(&scratch.display().to_string()));
    }

    fn result_text(result: &mush_agent::tool::ToolResult) -> String {
        match &result.content[0] {
            mush_ai::types::ToolResultContentPart::Text(t) => t.text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
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

    #[tokio::test]
    async fn bash_is_guarded_but_hides_the_bypass_knob() {
        let registry = registry_with_chars(false, ForbiddenChars::new(['—']));
        let bash = registry.get("bash").expect("bash tool registered");
        assert!(
            bash.parameters_schema()["properties"]["allow_forbidden_chars"].is_null(),
            "bash must keep the bypass field hidden from the schema"
        );
        let blocked = bash
            .execute(serde_json::json!({"command": "echo — dash"}))
            .await;
        assert!(
            !blocked.outcome.is_success(),
            "bash should block a forbidden char in the command"
        );
    }

    #[tokio::test]
    async fn bash_hidden_bypass_lets_forbidden_command_run() {
        let registry = registry_with_chars(false, ForbiddenChars::new(['—']));
        let bash = registry.get("bash").expect("bash tool registered");
        let result = bash
            .execute(serde_json::json!({
                "command": "printf %s '—'",
                "allow_forbidden_chars": true,
            }))
            .await;
        assert!(
            result.outcome.is_success(),
            "the hidden bypass should let a forbidden-char command through"
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
        let cwd: Arc<std::path::Path> = Arc::from(std::env::temp_dir().as_path());
        let mut registry = builtin_tools_with_options(
            cwd.clone(),
            None,
            false,
            http_client(),
            ForbiddenChars::empty(),
            None,
            Default::default(),
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
        let cwd: Arc<std::path::Path> = Arc::from(std::env::temp_dir().as_path());
        let mut registry = builtin_tools_with_options(
            cwd.clone(),
            None,
            true,
            http_client(),
            ForbiddenChars::empty(),
            None,
            Default::default(),
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
        let cwd: Arc<std::path::Path> = Arc::from(std::env::temp_dir().as_path());
        let mut registry = builtin_tools_with_options(
            cwd.clone(),
            None,
            false,
            http_client(),
            ForbiddenChars::empty(),
            None,
            Default::default(),
        );
        add_batch_tool(&mut registry);
        assert!(registry.get("batch").is_some());

        let gpt = model_with_id("gpt-5");
        assert!(gpt.supports_native_parallel_calls());
        align_for_model(&mut registry, &gpt, cwd.clone(), ForbiddenChars::empty());
        assert!(
            registry.get("batch").is_none(),
            "batch should be removed for native-parallel models"
        );

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
