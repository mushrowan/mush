pub mod apply_patch;
pub mod background;
pub mod bash;
pub mod bash_status;
pub mod batch;
pub mod edit;
pub mod find;
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

use std::path::PathBuf;
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
pub fn builtin_tools(cwd: PathBuf, http_client: reqwest::Client) -> ToolRegistry {
    let mut tools = builtin_tools_with_options(cwd, None, false, http_client);
    add_batch_tool(&mut tools);
    tools
}

/// create built-in tools with an optional bash output sink for streaming
pub fn builtin_tools_with_sink(
    cwd: PathBuf,
    output_sink: Option<bash::OutputSink>,
    http_client: reqwest::Client,
) -> ToolRegistry {
    let mut tools = builtin_tools_with_options(cwd, output_sink, false, http_client);
    add_batch_tool(&mut tools);
    tools
}

/// create built-in tools with all options.
/// when `use_patch` is true, apply_patch replaces edit + write (for GPT models).
///
/// does NOT include the batch tool. call `add_batch_tool` after registering
/// all additional tools (skill, MCP, LSP) so batch can access them all.
pub fn builtin_tools_with_options(
    cwd: PathBuf,
    output_sink: Option<bash::OutputSink>,
    use_patch: bool,
    http_client: reqwest::Client,
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
        tools.push(Arc::new(apply_patch::ApplyPatchTool::new(cwd)));
    } else {
        tools.push(Arc::new(write::WriteTool::new(cwd.clone())));
        tools.push(Arc::new(edit::EditTool::new(cwd)));
    }

    ToolRegistry::from_shared(tools)
}

/// add the batch tool to a registry, giving it access to all currently
/// registered tools. call this after all tools have been added so batch
/// can dispatch to skill, MCP, LSP tools etc.
pub fn add_batch_tool(registry: &mut ToolRegistry) {
    registry.register_shared(Arc::new(batch::BatchTool::new(registry.clone())));
}
