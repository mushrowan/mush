use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use mush_agent::tool::{AgentTool, OutputLimit, ToolResult, parse_tool_args};
use mush_treesitter::{RenderOptions, RepoMap, SharedRepoMap};
use serde::Deserialize;
use thiserror::Error;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoMapArgs {
    path: Option<String>,
    #[serde(default, deserialize_with = "mush_agent::de::opt_uint")]
    limit: Option<usize>,
    #[serde(default)]
    summary_only: bool,
    #[serde(
        default = "default_wait_timeout_secs",
        deserialize_with = "mush_agent::de::uint"
    )]
    wait_timeout: u64,
}

const fn default_wait_timeout_secs() -> u64 {
    crate::DEFAULT_TOOL_TIMEOUT_SECS
}

#[derive(Clone)]
pub struct RepoMapShared {
    map: Arc<RwLock<SharedRepoMap>>,
    state: Arc<RwLock<RepoMapState>>,
    budget_tokens: usize,
}

struct RepoMapState {
    root: PathBuf,
    started: bool,
    build_error: Option<String>,
    _watcher: Option<mush_treesitter::RepoMapWatcher>,
}

#[derive(Debug, Error)]
enum RepoMapEnsureError {
    #[error("repo map build is still running")]
    Building,
    #[error("repo map build failed: {0}")]
    Failed(String),
}

impl RepoMapShared {
    pub fn eager(map: SharedRepoMap, budget_tokens: usize) -> Self {
        Self {
            map: Arc::new(RwLock::new(map)),
            state: Arc::new(RwLock::new(RepoMapState {
                root: PathBuf::new(),
                started: true,
                build_error: None,
                _watcher: None,
            })),
            budget_tokens,
        }
    }

    pub fn lazy(root: impl Into<PathBuf>, budget_tokens: usize) -> Self {
        Self {
            map: Arc::new(RwLock::new(Arc::new(RwLock::new(RepoMap::default())))),
            state: Arc::new(RwLock::new(RepoMapState {
                root: root.into(),
                started: false,
                build_error: None,
                _watcher: None,
            })),
            budget_tokens,
        }
    }

    pub fn map(&self) -> SharedRepoMap {
        self.map
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| Arc::new(RwLock::new(RepoMap::default())))
    }

    fn ensure_ready(&self) -> Result<(), RepoMapEnsureError> {
        let mut state = self
            .state
            .write()
            .map_err(|_| RepoMapEnsureError::Failed("repo map state lock poisoned".into()))?;

        if let Some(error) = &state.build_error {
            return Err(RepoMapEnsureError::Failed(error.clone()));
        }

        if !state.started {
            state.started = true;
            if !mush_treesitter::is_inside_repo(&state.root) {
                state.build_error = Some("not inside a git/jj repository".into());
                return Err(RepoMapEnsureError::Failed(
                    "not inside a git/jj repository".into(),
                ));
            }

            match mush_treesitter::RepoMapWatcher::start(&state.root) {
                Some(watcher) => {
                    self.swap_map(watcher.map().clone())?;
                    state._watcher = Some(watcher);
                }
                None => {
                    state.build_error = Some("failed to start repo map watcher".into());
                    return Err(RepoMapEnsureError::Failed(
                        "failed to start repo map watcher".into(),
                    ));
                }
            }
        }

        let map_is_empty = self
            .map()
            .read()
            .map_err(|_| RepoMapEnsureError::Failed("failed to read repo map".into()))?
            .files
            .is_empty();
        if map_is_empty {
            return Err(RepoMapEnsureError::Building);
        }

        Ok(())
    }

    async fn wait_until_ready(&self, timeout: Duration) -> Result<(), RepoMapEnsureError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.ensure_ready() {
                Ok(()) => return Ok(()),
                Err(RepoMapEnsureError::Building) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn swap_map(&self, fresh: SharedRepoMap) -> Result<(), RepoMapEnsureError> {
        let mut current = self
            .map
            .write()
            .map_err(|_| RepoMapEnsureError::Failed("failed to update repo map".into()))?;
        *current = fresh;
        Ok(())
    }
}

pub struct RepoMapTool {
    shared: RepoMapShared,
}

impl RepoMapTool {
    pub fn new(shared: RepoMapShared) -> Self {
        Self { shared }
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
        "Get a structural overview of the repository showing the most important files and their symbols (types, functions, traits, modules) ranked by importance. Use this instead of reading many files when you need to understand the codebase structure. If the lazy repo map is still building, this waits up to wait_timeout seconds by default. Filter to a subtree with `path`, list more files at the cost of detail with `summary_only=true`, or cap with `limit`."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "description": "optional path prefix to filter results to (e.g. 'src' or 'crates/my-crate'). matches at path-component boundaries",
                    "type": "string"
                },
                "limit": {
                    "description": "optional cap on number of files included in the output. defaults to as many as fit the budget",
                    "type": "integer"
                },
                "summary_only": {
                    "description": "if true, emit one line per file with symbol counts only (no per-symbol detail). use to fit many files in a small output, or to get a directory listing of the parsed graph",
                    "type": "boolean",
                    "default": false
                },
                "wait_timeout": {
                    "description": format!(
                        "seconds to wait for a lazy repo map build to finish before returning a still-building status (default {}, 0 for instant check)",
                        crate::DEFAULT_TOOL_TIMEOUT_SECS
                    ),
                    "type": "integer",
                    "default": crate::DEFAULT_TOOL_TIMEOUT_SECS
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args: RepoMapArgs = match parse_tool_args(args) {
            Ok(a) => a,
            Err(e) => return e,
        };

        if let Err(error) = self
            .shared
            .wait_until_ready(Duration::from_secs(args.wait_timeout))
            .await
        {
            return ToolResult::error(error.to_string());
        }

        let map_handle = self.shared.map();
        let snapshot = match map_handle.read() {
            Ok(guard) => guard.clone(),
            Err(_) => return ToolResult::error("failed to read repo map"),
        };

        if snapshot.files.is_empty() {
            return ToolResult::error(
                "repo map is not available yet (still building or no parseable files found)",
            );
        }

        let filtered = match args.path.as_deref() {
            None => snapshot,
            Some(prefix) => snapshot.filter_path(prefix),
        };

        if filtered.files.is_empty() {
            let prefix = args.path.as_deref().unwrap_or("");
            return ToolResult::text(format!(
                "no files match prefix '{prefix}' in the repo map. \
                 try a shorter prefix, or call without `path` to see \
                 the top-ranked files across the repo."
            ));
        }

        let opts = RenderOptions {
            max_chars: self.shared.budget_tokens.saturating_mul(4),
            max_files: args.limit.unwrap_or(usize::MAX),
            summary_only: args.summary_only,
        };
        ToolResult::text(filtered.render(opts))
    }

    fn output_limit(&self) -> OutputLimit {
        OutputLimit::Head
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_treesitter::{RankedFile, SymbolInfo, SymbolKind};

    fn ranked(path: &str, syms: &[(usize, SymbolKind, &str)]) -> RankedFile {
        RankedFile {
            path: path.into(),
            rank: 0.5,
            symbols: syms
                .iter()
                .map(|(line, kind, name)| SymbolInfo {
                    name: (*name).into(),
                    kind: *kind,
                    signature: (*name).into(),
                    start_line: *line,
                    end_line: *line,
                    start_byte: 0,
                    end_byte: 0,
                    file_path: None,
                })
                .collect(),
        }
    }

    fn make_tool(map: RepoMap) -> RepoMapTool {
        let shared = RepoMapShared::eager(Arc::new(RwLock::new(map)), /*budget_tokens*/ 8192);
        RepoMapTool::new(shared)
    }

    fn sample_map() -> RepoMap {
        RepoMap {
            files: vec![
                ranked(
                    "crates/mush-ai/src/types.rs",
                    &[
                        (0, SymbolKind::Variable, "ModelId"),
                        (4, SymbolKind::Variable, "Provider"),
                    ],
                ),
                ranked(
                    "crates/mush-ai/src/registry.rs",
                    &[
                        (9, SymbolKind::Variable, "ApiRegistry"),
                        (19, SymbolKind::Function, "stream"),
                    ],
                ),
                ranked(
                    "crates/mush-tools/src/read.rs",
                    &[
                        (0, SymbolKind::Variable, "ReadTool"),
                        (49, SymbolKind::Function, "execute"),
                    ],
                ),
            ],
        }
    }

    #[test]
    fn repo_map_schema_uses_shared_wait_timeout_default() {
        let tool = make_tool(sample_map());
        let schema = tool.parameters_schema();
        assert_eq!(
            schema["properties"]["wait_timeout"]["default"],
            crate::DEFAULT_TOOL_TIMEOUT_SECS
        );
    }

    #[tokio::test]
    async fn returns_full_map_when_no_path_filter() {
        let tool = make_tool(sample_map());
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.outcome.is_success());
        let text = crate::util::extract_text(&result);
        assert!(text.contains("crates/mush-ai/src/types.rs:"));
        assert!(text.contains("crates/mush-tools/src/read.rs:"));
    }

    #[tokio::test]
    async fn filters_by_path_when_files_dropped_by_budget() {
        let dir = tempfile::tempdir().unwrap();
        let crate_a = dir.path().join("crates/mush-a/src");
        let crate_b = dir.path().join("crates/mush-b/src");
        std::fs::create_dir_all(&crate_a).unwrap();
        std::fs::create_dir_all(&crate_b).unwrap();
        for i in 0..30 {
            std::fs::write(
                crate_a.join(format!("a{i}.rs")),
                format!("pub struct A{i};\npub fn a{i}() {{}}\n"),
            )
            .unwrap();
            std::fs::write(
                crate_b.join(format!("b{i}.rs")),
                format!("pub struct B{i};\npub fn b{i}() {{}}\n"),
            )
            .unwrap();
        }
        std::fs::create_dir(dir.path().join(".git")).unwrap();

        let shared = RepoMapShared::lazy(dir.path().to_path_buf(), 64);
        let tool = RepoMapTool::new(shared);
        let result = tool
            .execute(serde_json::json!({"path": "crates/mush-b", "wait_timeout": 10}))
            .await;
        let text = crate::util::extract_text(&result);
        assert!(
            text.contains("crates/mush-b/src/b"),
            "filter should surface mush-b files even when rank ordering \
             would have placed them outside the rendered prefix: {text}"
        );
        assert!(
            !text.contains("crates/mush-a/src/a"),
            "filter must not leak files from a different crate: {text}"
        );
    }

    #[tokio::test]
    async fn filters_by_path_prefix() {
        let tool = make_tool(sample_map());
        let result = tool
            .execute(serde_json::json!({"path": "crates/mush-ai"}))
            .await;
        assert!(result.outcome.is_success());
        let text = crate::util::extract_text(&result);
        assert!(text.contains("crates/mush-ai/src/types.rs:"));
        assert!(text.contains("crates/mush-ai/src/registry.rs:"));
        assert!(!text.contains("crates/mush-tools/src/read.rs:"));
    }

    #[tokio::test]
    async fn empty_map_returns_error_so_args_are_logged() {
        let tool = make_tool(RepoMap::default());
        let result = tool.execute(serde_json::json!({"wait_timeout": 0})).await;
        assert!(result.outcome.is_error());
        let text = crate::util::extract_text(&result);
        assert!(
            text.contains("repo map build is still running")
                || text.contains("not available")
                || text.contains("still building")
        );
    }

    #[tokio::test]
    async fn lazy_repo_map_waits_for_initial_build() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn ready() {}\n").unwrap();

        let shared = RepoMapShared::lazy(dir.path().to_path_buf(), 8192);
        let tool = RepoMapTool::new(shared);
        let result = tool.execute(serde_json::json!({"wait_timeout": 10})).await;
        let text = crate::util::extract_text(&result);

        assert!(result.outcome.is_success(), "expected ready map: {text}");
        assert!(
            text.contains("src/lib.rs"),
            "expected lib.rs in map: {text}"
        );
        assert!(
            !text.contains("repo map build is still running"),
            "tool should wait instead of surfacing transient building state: {text}"
        );
    }

    #[tokio::test]
    async fn no_matches_for_path_disambiguates_from_budget_truncation() {
        let tool = make_tool(sample_map());
        let result = tool
            .execute(serde_json::json!({"path": "nonexistent/dir"}))
            .await;
        assert!(result.outcome.is_success());
        let text = crate::util::extract_text(&result);
        assert!(text.contains("no files match prefix 'nonexistent/dir'"));
        assert!(text.contains("try a shorter prefix"));
    }

    #[tokio::test]
    async fn summary_only_renders_one_line_per_file_with_symbol_count() {
        let tool = make_tool(sample_map());
        let result = tool
            .execute(serde_json::json!({"summary_only": true}))
            .await;
        assert!(result.outcome.is_success());
        let text = crate::util::extract_text(&result);
        assert!(text.contains("crates/mush-ai/src/types.rs: (2 symbols"));
        assert!(text.contains("crates/mush-tools/src/read.rs: (2 symbols"));
        assert!(!text.contains("var ModelId"));
        assert!(!text.contains("fn execute"));
    }

    #[tokio::test]
    async fn limit_caps_files_emitted() {
        let tool = make_tool(sample_map());
        let result = tool.execute(serde_json::json!({"limit": 1})).await;
        assert!(result.outcome.is_success());
        let text = crate::util::extract_text(&result);
        assert!(text.contains("crates/mush-ai/src/types.rs"));
        assert!(!text.contains("crates/mush-ai/src/registry.rs"));
        assert!(text.contains("2 more files omitted"));
        assert!(text.contains("narrow with `path`"));
    }
}
