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

use mush_agent::tool::AgentTool;

/// whether a model id should use the codex patch format instead of edit + write.
/// GPT models (except gpt-4 and oss variants) are trained on the patch format
/// and produce better edits with it.
pub fn uses_patch_tool(model_id: &str) -> bool {
    model_id.contains("gpt-")
        && !model_id.contains("oss")
        && !model_id.contains("gpt-4")
}

/// create the full set of built-in tools for a given working directory
pub fn builtin_tools(cwd: PathBuf) -> Vec<Box<dyn AgentTool>> {
    builtin_tools_with_options(cwd, None, false)
}

/// create built-in tools with an optional bash output sink for streaming
pub fn builtin_tools_with_sink(
    cwd: PathBuf,
    output_sink: Option<bash::OutputSink>,
) -> Vec<Box<dyn AgentTool>> {
    builtin_tools_with_options(cwd, output_sink, false)
}

/// create built-in tools with all options.
/// when `use_patch` is true, apply_patch replaces edit + write (for GPT models).
pub fn builtin_tools_with_options(
    cwd: PathBuf,
    output_sink: Option<bash::OutputSink>,
    use_patch: bool,
) -> Vec<Box<dyn AgentTool>> {
    let make_tools = |cwd: PathBuf| -> Vec<Box<dyn AgentTool>> {
        let bash_tool: Box<dyn AgentTool> = {
            let tool = bash::BashTool::new(cwd.clone());
            if let Some(ref sink) = output_sink {
                Box::new(tool.with_output_sink(sink.clone()))
            } else {
                Box::new(tool)
            }
        };

        let mut tools: Vec<Box<dyn AgentTool>> = vec![
            Box::new(read::ReadTool::new(cwd.clone())),
            bash_tool,
            Box::new(grep::GrepTool::new(cwd.clone())),
            Box::new(find::FindTool::new(cwd.clone())),
            Box::new(glob::GlobTool::new(cwd.clone())),
            Box::new(ls::LsTool::new(cwd.clone())),
            Box::new(web_search::WebSearchTool::new()),
            Box::new(web_fetch::WebFetchTool::new()),
            Box::new(notify_user::NotifyUserTool::new()),
        ];

        if use_patch {
            tools.push(Box::new(apply_patch::ApplyPatchTool::new(cwd)));
        } else {
            tools.push(Box::new(write::WriteTool::new(cwd.clone())));
            tools.push(Box::new(edit::EditTool::new(cwd)));
        }

        tools
    };

    // batch wraps its own copy of the tools so it can dispatch to them
    let inner_tools = make_tools(cwd.clone());
    let mut tools = make_tools(cwd);
    tools.push(Box::new(batch::BatchTool::new(inner_tools)));
    tools
}
