pub mod apply_patch;
pub mod bash;
pub mod batch;
pub mod edit;
pub mod find;
pub mod glob;
pub mod grep;
pub mod ls;
pub mod notify_user;
pub mod read;
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
pub fn uses_patch_tool(model_id: &str) -> bool {
    model_id.contains("gpt-") && !model_id.contains("oss") && !model_id.contains("gpt-4")
}

/// whether a model supports native parallel tool calls and doesn't need the
/// batch tool. GPT/codex models use the responses API which handles parallel
/// calls natively, and anthropic does too.
pub fn supports_native_parallel_calls(model_id: &str) -> bool {
    model_id.contains("gpt-")
        || model_id.contains("codex")
        || model_id.starts_with("o1")
        || model_id.starts_with("o3")
        || model_id.starts_with("o4")
}

/// create the full set of built-in tools for a given working directory
pub fn builtin_tools(cwd: PathBuf) -> ToolRegistry {
    builtin_tools_with_options(cwd, None, false, false)
}

/// create built-in tools with an optional bash output sink for streaming
pub fn builtin_tools_with_sink(
    cwd: PathBuf,
    output_sink: Option<bash::OutputSink>,
) -> ToolRegistry {
    builtin_tools_with_options(cwd, output_sink, false, false)
}

/// create built-in tools with all options.
/// when `use_patch` is true, apply_patch replaces edit + write (for GPT models).
/// when `skip_batch` is true, the batch tool is omitted (for models with native
/// parallel tool calls).
pub fn builtin_tools_with_options(
    cwd: PathBuf,
    output_sink: Option<bash::OutputSink>,
    use_patch: bool,
    skip_batch: bool,
) -> ToolRegistry {
    let make_tools = |cwd: PathBuf| -> ToolRegistry {
        let bash_tool: SharedTool = {
            let tool = bash::BashTool::new(cwd.clone());
            if let Some(ref sink) = output_sink {
                Arc::new(tool.with_output_sink(sink.clone()))
            } else {
                Arc::new(tool)
            }
        };

        let mut tools: Vec<SharedTool> = vec![
            Arc::new(read::ReadTool::new(cwd.clone())),
            bash_tool,
            Arc::new(grep::GrepTool::new(cwd.clone())),
            Arc::new(find::FindTool::new(cwd.clone())),
            Arc::new(glob::GlobTool::new(cwd.clone())),
            Arc::new(ls::LsTool::new(cwd.clone())),
            Arc::new(web_search::WebSearchTool::new()),
            Arc::new(web_fetch::WebFetchTool::new()),
            Arc::new(notify_user::NotifyUserTool::new()),
        ];

        if use_patch {
            tools.push(Arc::new(apply_patch::ApplyPatchTool::new(cwd)));
        } else {
            tools.push(Arc::new(write::WriteTool::new(cwd.clone())));
            tools.push(Arc::new(edit::EditTool::new(cwd)));
        }

        ToolRegistry::from_shared(tools)
    };

    if skip_batch {
        make_tools(cwd)
    } else {
        let inner_tools = make_tools(cwd.clone());
        let mut tools = make_tools(cwd);
        tools.register_shared(Arc::new(batch::BatchTool::new(inner_tools)));
        tools
    }
}
