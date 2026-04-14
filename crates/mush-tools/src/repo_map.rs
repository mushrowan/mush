//! repo map tool - exposes the tree-sitter repository map as an on-demand tool
//!
//! gives the agent a structural overview of the codebase (types, functions,
//! traits, line counts) without reading source files

use std::sync::{Arc, RwLock};

use mush_agent::tool::{AgentTool, OutputLimit, ToolResult, parse_tool_args};
use serde::Deserialize;

/// shared repo map text, kept up to date by the background watcher
pub type SharedMapText = Arc<RwLock<String>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoMapArgs {
    path: Option<String>,
}

pub struct RepoMapTool {
    map_text: SharedMapText,
}

impl RepoMapTool {
    pub fn new(map_text: SharedMapText) -> Self {
        Self { map_text }
    }
}

#[async_trait::async_trait]
impl AgentTool for RepoMapTool {
    fn name(&self) -> &str {
        "repo_map"
    }

    fn label(&self) -> &str {
        "RepoMap"
    }

    fn description(&self) -> &str {
        "Get a structural overview of the repository showing the most important files and their symbols (types, functions, traits, modules) ranked by importance. Use this instead of reading many files when you need to understand the codebase structure. Optionally filter to files under a specific directory."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "description": "optional directory path to filter results to (e.g. 'src' or 'crates/my-crate')",
                    "type": "string"
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args: RepoMapArgs = match parse_tool_args(args) {
            Ok(a) => a,
            Err(e) => return e,
        };

        let map_text = match self.map_text.read() {
            Ok(guard) => guard.clone(),
            Err(_) => return ToolResult::error("failed to read repo map"),
        };

        if map_text.is_empty() {
            return ToolResult::text(
                "repo map is not available yet (still building or no parseable files found)",
            );
        }

        match args.path {
            None => ToolResult::text(&map_text),
            Some(prefix) => {
                let filtered = filter_by_path(&map_text, &prefix);
                if filtered.is_empty() {
                    ToolResult::text(format!("no files found under '{prefix}' in the repo map"))
                } else {
                    ToolResult::text(filtered)
                }
            }
        }
    }

    fn output_limit(&self) -> OutputLimit {
        OutputLimit::Head
    }
}

/// filter repo map text to only include file blocks under the given path prefix.
///
/// the map format has file paths at column 0 ending with `:`, followed by
/// indented symbol lines. blank lines separate blocks.
fn filter_by_path(map_text: &str, prefix: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    let mut output = String::new();
    let mut current_block = String::new();
    let mut include_block = false;

    for line in map_text.lines() {
        if !line.starts_with(' ') && line.ends_with(':') {
            // flush previous block
            if include_block && !current_block.is_empty() {
                output.push_str(&current_block);
                output.push('\n');
            }
            current_block.clear();
            // check if this file path matches the prefix
            let path = line.trim_end_matches(':');
            include_block = path.starts_with(prefix)
                && path.as_bytes().get(prefix.len()).is_none_or(|&b| b == b'/');
        }

        current_block.push_str(line);
        current_block.push('\n');
    }

    // flush last block
    if include_block && !current_block.is_empty() {
        output.push_str(&current_block);
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool(text: &str) -> RepoMapTool {
        let shared = Arc::new(RwLock::new(text.to_string()));
        RepoMapTool::new(shared)
    }

    const SAMPLE_MAP: &str = "\
crates/mush-ai/src/types.rs:
    1│var ModelId
    5│var Provider

crates/mush-ai/src/registry.rs:
   10│var ApiRegistry
   20│fn stream

crates/mush-tools/src/read.rs:
    1│var ReadTool
   50│fn execute

";

    #[tokio::test]
    async fn returns_full_map_when_no_path_filter() {
        let tool = make_tool(SAMPLE_MAP);
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.outcome.is_success());
        let text = result_text(&result);
        assert!(text.contains("crates/mush-ai/src/types.rs:"));
        assert!(text.contains("crates/mush-tools/src/read.rs:"));
    }

    #[tokio::test]
    async fn filters_by_path_prefix() {
        let tool = make_tool(SAMPLE_MAP);
        let result = tool
            .execute(serde_json::json!({"path": "crates/mush-ai"}))
            .await;
        assert!(result.outcome.is_success());
        let text = result_text(&result);
        assert!(text.contains("crates/mush-ai/src/types.rs:"));
        assert!(text.contains("crates/mush-ai/src/registry.rs:"));
        assert!(!text.contains("crates/mush-tools/src/read.rs:"));
    }

    #[tokio::test]
    async fn returns_message_when_map_empty() {
        let tool = make_tool("");
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.outcome.is_success());
        let text = result_text(&result);
        assert!(text.contains("not available") || text.contains("still building"));
    }

    #[tokio::test]
    async fn no_matches_for_path_returns_message() {
        let tool = make_tool(SAMPLE_MAP);
        let result = tool
            .execute(serde_json::json!({"path": "nonexistent/dir"}))
            .await;
        assert!(result.outcome.is_success());
        let text = result_text(&result);
        assert!(text.contains("no files found"));
    }

    fn result_text(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|p| match p {
                mush_ai::types::ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}
