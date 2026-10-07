use std::path::Path;
use std::sync::Arc;

use globset::GlobBuilder;
use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::util::{clean_regex_pattern, inject_note, resolve_path};
use crate::walk::{self, PathMatch};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
    #[serde(default)]
    include_ignored: bool,
}

pub struct GlobTool {
    cwd: Arc<Path>,
}

impl GlobTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self { cwd }
    }
}

#[async_trait::async_trait]
impl AgentTool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn label(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Fast file pattern matching using glob syntax. Respects .gitignore. \
         Returns matching file paths sorted by modification time (newest first). Ignored matches are summarised by default. \
         Pass include_ignored: true to show ignored matches, prefixed with *. \
         The pattern is a raw glob; do not wrap it in quotes (e.g. pass `**/*.rs`, not `\"**/*.rs\"`). \
         Use this when you need to find files by name patterns like **/*.rs or src/**/*.ts."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "glob pattern to match files (e.g. **/*.rs, src/**/*.ts, *.toml). raw glob, no quote-wrapping"
                },
                "path": {
                    "type": "string",
                    "description": "directory to search in (defaults to cwd)"
                },
                "include_ignored": {
                    "type": "boolean",
                    "description": "include gitignored and hidden matches instead of summarising ignored base directories"
                }
            },
            "required": ["pattern"]
        })
    }

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Head
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<GlobArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        let search_dir = args
            .path
            .as_deref()
            .map(|path| resolve_path(&self.cwd, path))
            .unwrap_or_else(|| self.cwd.to_path_buf());

        run_glob(&self.cwd, &args.pattern, &search_dir, args.include_ignored)
    }
}

fn run_glob(cwd: &Path, pattern: &str, search_dir: &Path, include_ignored: bool) -> ToolResult {
    let cleaned = clean_regex_pattern(pattern);
    let result = glob_cleaned(cwd, &cleaned.pattern, search_dir, include_ignored);
    inject_note(result, cleaned.note)
}

fn glob_cleaned(cwd: &Path, pattern: &str, search_dir: &Path, include_ignored: bool) -> ToolResult {
    if !search_dir.is_dir() {
        return ToolResult::error(format!(
            "search path is not a directory: {}",
            search_dir.display()
        ));
    }

    let glob = match GlobBuilder::new(pattern).literal_separator(true).build() {
        Ok(glob) => glob.compile_matcher(),
        Err(error) => return ToolResult::error(format!("glob error: {error}")),
    };

    let visible = collect_matches(cwd, search_dir, &glob, false);
    let all = collect_matches(cwd, search_dir, &glob, true);
    ToolResult::text(walk::render(search_dir, visible, all, include_ignored))
}

fn collect_matches(
    cwd: &Path,
    search_dir: &Path,
    glob: &globset::GlobMatcher,
    include_ignored: bool,
) -> Vec<PathMatch> {
    let mut matches = Vec::new();
    for entry in walk::walk(search_dir, include_ignored).filter_map(Result::ok) {
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }

        let relative = entry
            .path()
            .strip_prefix(search_dir)
            .unwrap_or(entry.path());
        if !glob.is_match(relative) {
            continue;
        }

        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        matches.push((modified, PathMatch::new(cwd, search_dir, entry.into_path())));
    }

    matches.sort_by(|(left_modified, left), (right_modified, right)| {
        right_modified
            .cmp(left_modified)
            .then_with(|| left.display.cmp(&right.display))
    });
    matches.into_iter().map(|(_, entry)| entry).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_required_pattern() {
        let tool = GlobTool::new(Path::new("/tmp").into());
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "pattern"));
    }

    #[test]
    fn glob_finds_toml_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/config.toml"), "x = 1").unwrap();
        std::fs::write(dir.path().join("readme.md"), "hi").unwrap();

        let result = run_glob(dir.path(), "**/*.toml", dir.path(), false);
        let text = crate::util::extract_text(&result);
        assert!(text.contains("Cargo.toml"));
        assert!(text.contains("nested/config.toml"));
        assert!(!text.contains("readme.md"));
    }

    #[test]
    fn glob_no_results() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();

        let result = run_glob(dir.path(), "**/*.rs", dir.path(), false);
        let text = crate::util::extract_text(&result);
        assert_eq!(text, "no files found");
    }

    #[test]
    fn glob_strips_trailing_unmatched_quote_and_notes_it() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();

        let result = run_glob(dir.path(), "**/*.toml\"", dir.path(), false);
        let text = crate::util::extract_text(&result);
        assert!(
            text.contains("Cargo.toml"),
            "must run cleaned pattern and find the file (got: {text})"
        );
        assert!(
            text.contains("trailing unmatched"),
            "must surface a note about the strip (got: {text})"
        );
    }

    #[test]
    fn glob_summarises_ignored_matches_by_default() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(dir.path().join("target/debug/generated.rs"), "").unwrap();

        let result = run_glob(dir.path(), "**/*.rs", dir.path(), false);
        let text = crate::util::extract_text(&result);

        assert!(text.contains("no visible files found"), "got: {text}");
        assert!(text.contains("ignored matches omitted"), "got: {text}");
        assert!(text.contains("* target/ 1 match"), "got: {text}");
        assert!(text.contains("include_ignored: true"), "got: {text}");
        assert!(!text.contains("target/debug/generated.rs"), "got: {text}");
    }

    #[test]
    fn glob_include_ignored_marks_ignored_matches() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(dir.path().join("target/debug/generated.rs"), "").unwrap();

        let result = run_glob(dir.path(), "**/*.rs", dir.path(), true);
        let text = crate::util::extract_text(&result);

        assert!(text.contains("* target/debug/generated.rs"), "got: {text}");
        assert!(
            text.contains("Ignored matches are prefixed with *"),
            "got: {text}"
        );
    }
}
