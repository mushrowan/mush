pub mod bash;
pub mod batch;
pub mod edit;
pub mod find;
pub mod glob;
pub mod grep;
pub mod ls;
pub mod read;
pub mod web_fetch;
pub mod web_search;
pub mod write;

use std::path::PathBuf;

use mush_agent::tool::AgentTool;

/// create the full set of built-in tools for a given working directory.
/// batch tool wraps clones of the other tools so it can dispatch to them.
pub fn builtin_tools(cwd: PathBuf) -> Vec<Box<dyn AgentTool>> {
    builtin_tools_with_sink(cwd, None)
}

/// create built-in tools with an optional bash output sink for streaming
pub fn builtin_tools_with_sink(
    cwd: PathBuf,
    output_sink: Option<bash::OutputSink>,
) -> Vec<Box<dyn AgentTool>> {
    let make_bash = |cwd: PathBuf| -> Box<dyn AgentTool> {
        let tool = bash::BashTool::new(cwd);
        if let Some(ref sink) = output_sink {
            Box::new(tool.with_output_sink(sink.clone()))
        } else {
            Box::new(tool)
        }
    };

    // tools that batch can dispatch to (everything except batch itself)
    let inner_tools: Vec<Box<dyn AgentTool>> = vec![
        Box::new(read::ReadTool::new(cwd.clone())),
        Box::new(write::WriteTool::new(cwd.clone())),
        Box::new(edit::EditTool::new(cwd.clone())),
        make_bash(cwd.clone()),
        Box::new(grep::GrepTool::new(cwd.clone())),
        Box::new(find::FindTool::new(cwd.clone())),
        Box::new(glob::GlobTool::new(cwd.clone())),
        Box::new(ls::LsTool::new(cwd.clone())),
        Box::new(web_search::WebSearchTool::new()),
        Box::new(web_fetch::WebFetchTool::new()),
    ];

    let mut tools: Vec<Box<dyn AgentTool>> = vec![
        Box::new(read::ReadTool::new(cwd.clone())),
        Box::new(write::WriteTool::new(cwd.clone())),
        Box::new(edit::EditTool::new(cwd.clone())),
        make_bash(cwd.clone()),
        Box::new(grep::GrepTool::new(cwd.clone())),
        Box::new(find::FindTool::new(cwd.clone())),
        Box::new(glob::GlobTool::new(cwd.clone())),
        Box::new(ls::LsTool::new(cwd)),
        Box::new(web_search::WebSearchTool::new()),
        Box::new(web_fetch::WebFetchTool::new()),
    ];
    tools.push(Box::new(batch::BatchTool::new(inner_tools)));
    tools
}
