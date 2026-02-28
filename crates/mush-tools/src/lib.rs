pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod write;

use std::path::PathBuf;

use mush_agent::tool::AgentTool;

/// create the full set of built-in tools for a given working directory
pub fn builtin_tools(cwd: PathBuf) -> Vec<Box<dyn AgentTool>> {
    vec![
        Box::new(read::ReadTool::new(cwd.clone())),
        Box::new(write::WriteTool::new(cwd.clone())),
        Box::new(edit::EditTool::new(cwd.clone())),
        Box::new(bash::BashTool::new(cwd.clone())),
        Box::new(grep::GrepTool::new(cwd.clone())),
        Box::new(find::FindTool::new(cwd.clone())),
        Box::new(ls::LsTool::new(cwd)),
    ]
}
