use std::path::Path;
use std::sync::Arc;

use grep_regex::RegexMatcherBuilder;
use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::util::{clean_regex_pattern, inject_note, resolve_path};
use crate::walk::{self, PathMatch};

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FindType {
    File,
    Directory,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindArgs {
    pattern: String,
    path: Option<String>,
    #[serde(rename = "type")]
    type_filter: Option<FindType>,
    #[serde(default)]
    include_ignored: bool,
}

pub struct FindTool {
    cwd: Arc<Path>,
}

impl FindTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self { cwd }
    }
}

#[async_trait::async_trait]
impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn label(&self) -> &str {
        "Find"
    }

    fn description(&self) -> &str {
        "Search for files and directories by name pattern using Rust ignore/glob rules. Respects .gitignore. \
         Returns matching paths relative to the search directory. Ignored matches are summarised by default. \
         Pass include_ignored: true to show ignored matches, prefixed with *. \
         The pattern is a raw regex; do not wrap it in quotes (e.g. pass `mise`, not `\"mise\"`). \
         For glob patterns like **/*.rs, use the glob tool instead."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "regex pattern to match file/directory names. raw regex, no quote-wrapping"
                },
                "path": {
                    "type": "string",
                    "description": "directory to search in (defaults to cwd)"
                },
                "type": {
                    "type": "string",
                    "enum": ["file", "directory"],
                    "description": "restrict results to files or directories"
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
        let args = match parse_tool_args::<FindArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        let search_path = args
            .path
            .as_deref()
            .map(|path| resolve_path(&self.cwd, path))
            .unwrap_or_else(|| self.cwd.to_path_buf());

        let cwd = Arc::clone(&self.cwd);
        crate::util::run_blocking(move || {
            run_find(
                &cwd,
                &args.pattern,
                &search_path,
                args.type_filter,
                args.include_ignored,
            )
        })
        .await
    }
}

fn run_find(
    cwd: &Path,
    pattern: &str,
    search_path: &Path,
    type_filter: Option<FindType>,
    include_ignored: bool,
) -> ToolResult {
    let cleaned = clean_regex_pattern(pattern);
    let result = find_cleaned(
        cwd,
        &cleaned.pattern,
        search_path,
        type_filter,
        include_ignored,
    );
    inject_note(result, cleaned.note)
}

fn find_cleaned(
    cwd: &Path,
    pattern: &str,
    search_path: &Path,
    type_filter: Option<FindType>,
    include_ignored: bool,
) -> ToolResult {
    if !search_path.is_dir() {
        return ToolResult::error(format!(
            "search path is not a directory: {}",
            search_path.display()
        ));
    }

    let matcher = match RegexMatcherBuilder::new().build(pattern) {
        Ok(matcher) => matcher,
        Err(error) => return ToolResult::error(format!("regex error: {error}")),
    };

    let visible = collect_matches(cwd, search_path, type_filter, &matcher, false);
    let all = collect_matches(cwd, search_path, type_filter, &matcher, true);
    ToolResult::text(walk::render(search_path, visible, all, include_ignored))
}

fn collect_matches(
    cwd: &Path,
    search_path: &Path,
    type_filter: Option<FindType>,
    matcher: &grep_regex::RegexMatcher,
    include_ignored: bool,
) -> Vec<PathMatch> {
    let mut matches = walk::collect(search_path, include_ignored, |entry| {
        if !matches_type(entry.file_type(), type_filter) {
            return None;
        }
        let name = entry.file_name().to_str()?;
        if !grep_matcher::Matcher::is_match(matcher, name.as_bytes()).unwrap_or(false) {
            return None;
        }
        Some(PathMatch::new(cwd, search_path, entry.into_path()))
    });
    matches.sort_by(|left, right| left.display.cmp(&right.display));
    matches
}

fn matches_type(file_type: Option<std::fs::FileType>, type_filter: Option<FindType>) -> bool {
    match type_filter {
        Some(FindType::File) => file_type.is_some_and(|file_type| file_type.is_file()),
        Some(FindType::Directory) => file_type.is_some_and(|file_type| file_type.is_dir()),
        None => file_type.is_some_and(|file_type| file_type.is_file() || file_type.is_dir()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_files_by_extension() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn x() {}\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hi\n").unwrap();

        let result = run_find(
            dir.path(),
            r".*\.rs$",
            dir.path(),
            Some(FindType::File),
            false,
        );
        let text = crate::util::extract_text(&result);
        assert!(text.contains("main.rs"));
        assert!(text.contains("lib.rs"));
        assert!(!text.contains("notes.txt"));
    }

    #[test]
    fn find_substring_pattern_matches_filename() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("zx.py"), "").unwrap();
        std::fs::write(dir.path().join("normal.txt"), "").unwrap();

        let result = run_find(dir.path(), "zx", dir.path(), None, false);
        let text = crate::util::extract_text(&result);
        assert!(
            text.contains("zx.py"),
            "pattern 'zx' must match 'zx.py' (got: {text})"
        );
        assert!(!text.contains("normal.txt"));
    }

    #[test]
    fn find_strips_trailing_unmatched_quote_and_notes_it() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("mise.toml"), "").unwrap();

        let result = run_find(dir.path(), "mise\"", dir.path(), None, false);
        let text = crate::util::extract_text(&result);
        assert!(
            text.contains("mise.toml"),
            "must run cleaned pattern and find the file (got: {text})"
        );
        assert!(
            text.contains("trailing unmatched"),
            "must surface a note about the strip (got: {text})"
        );
    }

    #[test]
    fn find_no_results() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();

        let result = run_find(dir.path(), r".*\.py$", dir.path(), None, false);
        let text = crate::util::extract_text(&result);
        assert_eq!(text, "no files found");
    }

    #[test]
    fn find_invalid_regex_returns_error_not_silent() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = run_find(dir.path(), "[", dir.path(), None, false);
        assert!(
            result.outcome.is_error(),
            "invalid regex must produce a tool error",
        );
        let text = crate::util::extract_text(&result);
        assert!(
            text.contains("regex"),
            "error should mention the regex parse failure: {text}",
        );
    }

    #[test]
    fn find_nonexistent_search_path_returns_error() {
        let cwd = tempfile::TempDir::new().unwrap();
        let bogus = cwd.path().join("does-not-exist");
        let result = run_find(cwd.path(), "anything", &bogus, None, false);
        assert!(
            result.outcome.is_error(),
            "nonexistent path must produce a tool error",
        );
    }

    #[test]
    fn find_summarises_ignored_matches_by_default() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(dir.path().join("target/debug/generated.rs"), "").unwrap();

        let result = run_find(
            dir.path(),
            r".*\.rs$",
            dir.path(),
            Some(FindType::File),
            false,
        );
        let text = crate::util::extract_text(&result);

        assert!(text.contains("no visible files found"), "got: {text}");
        assert!(text.contains("ignored matches omitted"), "got: {text}");
        assert!(text.contains("* target/ 1 match"), "got: {text}");
        assert!(text.contains("include_ignored: true"), "got: {text}");
        assert!(!text.contains("target/debug/generated.rs"), "got: {text}");
    }

    #[test]
    fn find_include_ignored_marks_ignored_matches() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(dir.path().join("target/debug/generated.rs"), "").unwrap();

        let result = run_find(
            dir.path(),
            r".*\.rs$",
            dir.path(),
            Some(FindType::File),
            true,
        );
        let text = crate::util::extract_text(&result);

        assert!(text.contains("* target/debug/generated.rs"), "got: {text}");
        assert!(
            text.contains("Ignored matches are prefixed with *"),
            "got: {text}"
        );
    }
}
