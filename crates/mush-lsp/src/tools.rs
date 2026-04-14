//! LSP-backed agent tools
//!
//! exposes LSP capabilities as tools the agent can call:
//! - `lsp_diagnostics`: get type errors and warnings for a file
//! - `lsp_hover`: get type/doc info at a position
//! - `lsp_references`: find all references to a symbol

use std::path::PathBuf;
use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult};
use serde_json::{Value, json};

use crate::client::format_diagnostics;
use crate::registry::LspRegistry;

/// get LSP diagnostics for a file
pub struct DiagnosticsTool {
    registry: Arc<LspRegistry>,
    cwd: PathBuf,
}

impl DiagnosticsTool {
    pub fn new(registry: Arc<LspRegistry>, cwd: PathBuf) -> Self {
        Self { registry, cwd }
    }
}

#[async_trait::async_trait]
impl AgentTool for DiagnosticsTool {
    fn name(&self) -> &str {
        "lsp_diagnostics"
    }

    fn label(&self) -> &str {
        "LSP Diagnostics"
    }

    fn description(&self) -> &str {
        "Get type errors, warnings, and other diagnostics for a file from the language server. \
         Starts the LSP server for the file's language if not already running."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to get diagnostics for"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::error("missing required parameter: path"),
        };

        let path = resolve_path(&self.cwd, path_str);
        if !path.exists() {
            return ToolResult::error(format!("file not found: {}", path.display()));
        }

        match self.registry.diagnostics_for_file(&path).await {
            Ok(diags) if diags.is_empty() => {
                ToolResult::text(format!("no diagnostics for {}", path.display()))
            }
            Ok(diags) => ToolResult::text(format_diagnostics(&path, &diags)),
            Err(e) => ToolResult::error(format!("LSP error: {e}")),
        }
    }
}

/// create all LSP tools sharing a single registry
pub fn lsp_tools(registry: Arc<LspRegistry>, cwd: PathBuf) -> Vec<Box<dyn AgentTool>> {
    vec![Box::new(DiagnosticsTool::new(registry, cwd))]
}

// same as mush_tools::util::resolve_path (can't depend on mush-tools here)
fn resolve_path(cwd: &std::path::Path, path_str: &str) -> PathBuf {
    let p = PathBuf::from(path_str);
    if p.is_absolute() { p } else { cwd.join(p) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_tool_schema() {
        let registry = Arc::new(LspRegistry::new(PathBuf::from("/tmp")));
        let tool = DiagnosticsTool::new(registry, PathBuf::from("/tmp"));
        assert_eq!(tool.name(), "lsp_diagnostics");
        let schema = tool.parameters_schema();
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("path"))
        );
    }

    #[tokio::test]
    async fn diagnostics_tool_missing_path() {
        let registry = Arc::new(LspRegistry::new(PathBuf::from("/tmp")));
        let tool = DiagnosticsTool::new(registry, PathBuf::from("/tmp"));
        let result = tool.execute(json!({})).await;
        assert!(result.outcome.is_error());
    }

    #[tokio::test]
    async fn diagnostics_tool_file_not_found() {
        let registry = Arc::new(LspRegistry::new(PathBuf::from("/tmp")));
        let tool = DiagnosticsTool::new(registry, PathBuf::from("/tmp"));
        let result = tool.execute(json!({"path": "/nonexistent/file.rs"})).await;
        assert!(result.outcome.is_error());
    }

    #[test]
    fn lsp_tools_creates_tools() {
        let registry = Arc::new(LspRegistry::new(PathBuf::from("/tmp")));
        let tools = lsp_tools(registry, PathBuf::from("/tmp"));
        assert!(!tools.is_empty());
        assert_eq!(tools[0].name(), "lsp_diagnostics");
    }
}
