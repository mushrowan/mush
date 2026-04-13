//! grep tool - pattern search using grep-searcher + ignore library crates
//!
//! replaces the previous rg subprocess with in-process search.
//! uses `grep-regex` for pattern matching, `grep-searcher` for line-oriented
//! search with context, and `ignore` for .gitignore-respecting directory walks.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::util::{resolve_path, truncate_lines};

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GrepMode {
    #[default]
    Regex,
    Literal,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GrepOutput {
    #[default]
    Lines,
    Count,
    Files,
    Json,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
    #[serde(default)]
    mode: GrepMode,
    #[serde(default = "default_true")]
    case_sensitive: bool,
    #[serde(default)]
    whole_word: bool,
    #[serde(default)]
    context_before: u64,
    #[serde(default)]
    context_after: u64,
    #[serde(default)]
    output: GrepOutput,
    max_results: Option<usize>,
    top_n: Option<usize>,
}

const fn default_true() -> bool {
    true
}

/// per-file match cap in lines mode (same as old --max-count=50)
const MAX_MATCHES_PER_FILE: u64 = 50;

pub struct GrepTool {
    cwd: Arc<Path>,
}

impl GrepTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self { cwd }
    }
}

impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn label(&self) -> &str {
        "Grep"
    }
    fn description(&self) -> &str {
        "Search file contents using ripgrep. Respects .gitignore. \
         Returns matching lines with file paths and line numbers. \
         Use 'count' output to get per-file match counts instead of full lines. \
         Use 'files' output to get just filenames with matches. \
         Always use this tool instead of running grep or rg via the bash tool."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "description": "regex pattern to search for",
                    "type": "string"
                },
                "path": {
                    "description": "directory or file to search in (defaults to cwd)",
                    "type": "string"
                },
                "include": {
                    "description": "glob pattern for files to include (e.g. '*.rs')",
                    "type": "string"
                },
                "mode": {
                    "description": "search mode: 'regex' (default) or 'literal' for fixed string matching",
                    "type": "string",
                    "enum": ["regex", "literal"]
                },
                "case_sensitive": {
                    "description": "whether the search is case sensitive (default true)",
                    "type": "boolean"
                },
                "whole_word": {
                    "description": "only match whole words (default false)",
                    "type": "boolean"
                },
                "context_before": {
                    "description": "lines of context to show before each match (default 0)",
                    "type": "integer"
                },
                "context_after": {
                    "description": "lines of context to show after each match (default 0)",
                    "type": "integer"
                },
                "output": {
                    "description": "output format: 'lines' (default, full matching lines), 'count' (per-file match counts sorted by count desc), 'files' (just filenames with matches), 'json' (structured per-file counts as JSON array)",
                    "type": "string",
                    "enum": ["lines", "count", "files", "json"]
                },
                "max_results": {
                    "description": "max files to show in count/files output (default: all)",
                    "type": "integer"
                },
                "top_n": {
                    "description": "return only the top N files by match count (for count/json output modes). applied after sorting by count desc",
                    "type": "integer"
                }
            },
            "required": ["pattern"]
        })
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let args = match parse_tool_args::<GrepArgs>(args) {
                Ok(args) => args,
                Err(error) => return error,
            };

            if args.pattern.is_empty() {
                return ToolResult::error("pattern is required");
            }

            let search_path = args
                .path
                .as_deref()
                .map(|path| resolve_path(&self.cwd, path))
                .unwrap_or_else(|| self.cwd.to_path_buf());

            let cwd = self.cwd.clone();
            let pattern = args.pattern.clone();
            let include = args.include.clone();

            tokio::task::spawn_blocking(move || {
                run_search(
                    &cwd,
                    &pattern,
                    &search_path,
                    include.as_deref(),
                    args.mode,
                    args.case_sensitive,
                    args.whole_word,
                    args.context_before,
                    args.context_after,
                    args.output,
                    args.max_results,
                    args.top_n,
                )
            })
            .await
            .unwrap_or_else(|e| ToolResult::error(format!("task join error: {e}")))
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn run_search(
    cwd: &Path,
    pattern: &str,
    search_path: &Path,
    include: Option<&str>,
    mode: GrepMode,
    case_sensitive: bool,
    whole_word: bool,
    context_before: u64,
    context_after: u64,
    output_mode: GrepOutput,
    max_results: Option<usize>,
    top_n: Option<usize>,
) -> ToolResult {
    // strip newlines from pattern - LLMs sometimes include literal newlines
    let pattern: String = pattern
        .chars()
        .filter(|&c| c != '\n' && c != '\r')
        .collect();

    if pattern.is_empty() {
        return ToolResult::error("pattern is empty (after stripping newlines)");
    }

    // build the regex matcher
    let mut builder = RegexMatcherBuilder::new();
    if matches!(mode, GrepMode::Literal) {
        builder.fixed_strings(true);
    }
    if !case_sensitive {
        builder.case_insensitive(true);
    }
    if whole_word {
        builder.word(true);
    }
    let matcher = match builder.build(&pattern) {
        Ok(m) => m,
        Err(e) => return ToolResult::error(format!("invalid pattern: {e}")),
    };

    // collect files to search
    let files = match collect_files(cwd, search_path, include) {
        Ok(f) => f,
        Err(e) => return ToolResult::error(format!("failed to walk directory: {e}")),
    };

    if files.is_empty() {
        return ToolResult::text("No matches found.");
    }

    match output_mode {
        GrepOutput::Lines => search_lines(
            &matcher,
            &files,
            search_path,
            context_before as usize,
            context_after as usize,
        ),
        GrepOutput::Count | GrepOutput::Json => search_count(
            &matcher,
            &files,
            search_path,
            output_mode,
            max_results,
            top_n,
        ),
        GrepOutput::Files => search_files(&matcher, &files, search_path, max_results),
    }
}

/// collect files to search, respecting .gitignore
fn collect_files(
    cwd: &Path,
    search_path: &Path,
    include: Option<&str>,
) -> io::Result<Vec<PathBuf>> {
    // single file
    if search_path.is_file() {
        return Ok(vec![search_path.to_path_buf()]);
    }

    let mut walk = WalkBuilder::new(search_path);
    walk.hidden(true).git_ignore(true).parents(true);

    if let Some(glob) = include {
        let mut overrides = OverrideBuilder::new(cwd);
        // ignore crate uses `!` prefix for excludes, bare patterns for includes.
        // when an include glob is given, we need to exclude everything else
        // first, then include the pattern
        let _ = overrides.add("!*");
        let _ = overrides.add(glob);
        if let Ok(built) = overrides.build() {
            walk.overrides(built);
        }
    }

    let mut files = Vec::new();
    for entry in walk.build() {
        let entry = entry.map_err(io::Error::other)?;
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            files.push(entry.into_path());
        }
    }
    files.sort();
    Ok(files)
}

/// make a path relative to the search root for display
fn display_path(path: &Path, search_root: &Path) -> String {
    path.strip_prefix(search_root)
        .unwrap_or(path)
        .display()
        .to_string()
}

// lines mode: collect matching lines with file:line:content format

fn search_lines(
    matcher: &grep_regex::RegexMatcher,
    files: &[PathBuf],
    search_root: &Path,
    ctx_before: usize,
    ctx_after: usize,
) -> ToolResult {
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .before_context(ctx_before)
        .after_context(ctx_after)
        .build();

    let mut output_lines: Vec<String> = Vec::new();

    for file in files {
        let rel = display_path(file, search_root);
        let mut sink = LineSink {
            path: &rel,
            lines: &mut output_lines,
            match_count: 0,
            max_matches: MAX_MATCHES_PER_FILE,
            needs_separator: false,
        };
        // errors (e.g. binary files, permission denied) are silently skipped
        let _ = searcher.search_path(matcher, file, &mut sink);
    }

    if output_lines.is_empty() {
        return ToolResult::text("No matches found.");
    }

    let lines_ref: Vec<&str> = output_lines.iter().map(String::as_str).collect();
    ToolResult::text(truncate_lines(&lines_ref, "matches"))
}

/// custom sink that formats output like `file:line:content`
struct LineSink<'a> {
    path: &'a str,
    lines: &'a mut Vec<String>,
    match_count: u64,
    max_matches: u64,
    needs_separator: bool,
}

impl Sink for LineSink<'_> {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, io::Error> {
        if self.match_count >= self.max_matches {
            return Ok(false);
        }
        self.match_count += 1;

        let line_num = mat.line_number().unwrap_or(0);
        let text = std::str::from_utf8(mat.bytes())
            .unwrap_or("<binary>")
            .trim_end_matches('\n')
            .trim_end_matches('\r');

        self.lines
            .push(format!("{}:{}:{}", self.path, line_num, text));
        self.needs_separator = true;
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, io::Error> {
        let line_num = ctx.line_number().unwrap_or(0);
        let text = std::str::from_utf8(ctx.bytes())
            .unwrap_or("<binary>")
            .trim_end_matches('\n')
            .trim_end_matches('\r');

        self.lines
            .push(format!("{}-{}-{}", self.path, line_num, text));
        Ok(true)
    }

    fn context_break(&mut self, _searcher: &grep_searcher::Searcher) -> Result<bool, io::Error> {
        if self.needs_separator {
            self.lines.push("--".to_string());
            self.needs_separator = false;
        }
        Ok(true)
    }
}

// count mode: count matches per file

fn search_count(
    matcher: &grep_regex::RegexMatcher,
    files: &[PathBuf],
    search_root: &Path,
    output_mode: GrepOutput,
    max_results: Option<usize>,
    top_n: Option<usize>,
) -> ToolResult {
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .build();

    let mut counts: Vec<(String, u64)> = Vec::new();

    for file in files {
        let mut count: u64 = 0;
        let _ = searcher.search_path(
            matcher,
            file,
            grep_searcher::sinks::UTF8(|_, _| {
                count += 1;
                Ok(true)
            }),
        );
        if count > 0 {
            let rel = display_path(file, search_root);
            counts.push((rel, count));
        }
    }

    if counts.is_empty() {
        return ToolResult::text("No matches found.");
    }

    counts.sort_by_key(|e| std::cmp::Reverse(e.1));

    let total_matches: u64 = counts.iter().map(|(_, c)| c).sum();
    let total_files = counts.len();
    let effective_limit = top_n.or(max_results);
    if let Some(n) = effective_limit {
        counts.truncate(n);
    }

    if matches!(output_mode, GrepOutput::Json) {
        let items: Vec<serde_json::Value> = counts
            .iter()
            .map(|(path, count)| serde_json::json!({"file": path, "count": count}))
            .collect();
        let json = serde_json::json!({
            "total_matches": total_matches,
            "total_files": total_files,
            "files": items,
        });
        ToolResult::text(json.to_string())
    } else {
        let showing = if effective_limit.is_some() && counts.len() < total_files {
            format!(" (showing top {})", counts.len())
        } else {
            String::new()
        };
        let mut out = format!("{total_matches} matches across {total_files} files{showing}\n");
        for (path, count) in &counts {
            out.push_str(&format!("{path}: {count}\n"));
        }
        ToolResult::text(out)
    }
}

// files mode: list files with any match

fn search_files(
    matcher: &grep_regex::RegexMatcher,
    files: &[PathBuf],
    search_root: &Path,
    max_results: Option<usize>,
) -> ToolResult {
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .build();

    let mut matching_files: Vec<String> = Vec::new();

    for file in files {
        let mut found = false;
        let _ = searcher.search_path(
            matcher,
            file,
            grep_searcher::sinks::UTF8(|_, _| {
                found = true;
                Ok(false) // stop after first match
            }),
        );
        if found {
            matching_files.push(display_path(file, search_root));
        }
    }

    if matching_files.is_empty() {
        return ToolResult::text("No matches found.");
    }

    matching_files.sort();
    let total = matching_files.len();
    if let Some(n) = max_results {
        matching_files.truncate(n);
    }
    let showing = if matching_files.len() < total {
        format!(" (showing {}/{})", matching_files.len(), total)
    } else {
        String::new()
    };
    ToolResult::text(format!(
        "{total} files{showing}\n\n{}",
        matching_files.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;
    use std::fs;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[tokio::test]
    async fn grep_finds_pattern() {
        let dir = temp_dir();
        fs::write(
            dir.path().join("test.txt"),
            "hello world\nfoo bar\nhello again",
        )
        .unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello",
                "path": dir.path().to_str().unwrap()
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("hello world"));
        assert!(text.contains("hello again"));
    }

    #[tokio::test]
    async fn grep_no_matches() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "hello world").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "nonexistent",
                "path": dir.path().to_str().unwrap()
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("No matches"));
    }

    #[tokio::test]
    async fn grep_with_include() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("test.txt"), "fn other()").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "fn",
                "path": dir.path().to_str().unwrap(),
                "include": "*.rs"
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("main"));
        assert!(!text.contains("other"));
    }

    #[tokio::test]
    async fn grep_literal_mode() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "hello.world\nhelloXworld").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello.world",
                "path": dir.path().to_str().unwrap(),
                "mode": "literal"
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("hello.world"));
        // in literal mode, . is not a regex metachar, so helloXworld should not match
        assert!(!text.contains("helloXworld"));
    }

    #[tokio::test]
    async fn grep_case_insensitive() {
        let dir = temp_dir();
        fs::write(
            dir.path().join("test.txt"),
            "Hello World\nhello world\nHELLO",
        )
        .unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello",
                "path": dir.path().to_str().unwrap(),
                "case_sensitive": false
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("Hello World"));
        assert!(text.contains("hello world"));
        assert!(text.contains("HELLO"));
    }

    #[tokio::test]
    async fn grep_whole_word() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "cat\ncatch\nthe cat sat").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "cat",
                "path": dir.path().to_str().unwrap(),
                "whole_word": true
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("cat"));
        assert!(text.contains("the cat sat"));
        assert!(!text.contains("catch"));
    }

    #[tokio::test]
    async fn grep_context_lines() {
        let dir = temp_dir();
        fs::write(
            dir.path().join("test.txt"),
            "line1\nline2\nMATCH\nline4\nline5",
        )
        .unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "MATCH",
                "path": dir.path().to_str().unwrap(),
                "context_before": 1,
                "context_after": 1
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("line2"));
        assert!(text.contains("MATCH"));
        assert!(text.contains("line4"));
    }

    #[tokio::test]
    async fn grep_strips_newlines_from_pattern() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "helloworld").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello\nworld",
                "path": dir.path().to_str().unwrap()
            }))
            .await;
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("helloworld"));
    }

    #[tokio::test]
    async fn grep_count_mode() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.txt"), "hello\nhello\nhello").unwrap();
        fs::write(dir.path().join("b.txt"), "hello\nworld").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello",
                "path": dir.path().to_str().unwrap(),
                "output": "count"
            }))
            .await;
        let text = extract_text(&result);
        // sorted descending by count, with summary header
        assert!(text.contains("4 matches across 2 files"));
        assert!(text.contains("a.txt: 3"));
        assert!(text.contains("b.txt: 1"));
    }

    #[tokio::test]
    async fn grep_files_mode() {
        let dir = temp_dir();
        fs::write(dir.path().join("match.txt"), "hello world").unwrap();
        fs::write(dir.path().join("no.txt"), "goodbye").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello",
                "path": dir.path().to_str().unwrap(),
                "output": "files"
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("match.txt"));
        assert!(!text.contains("no.txt"));
    }

    #[tokio::test]
    async fn grep_count_with_max_results() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.txt"), "x\nx\nx").unwrap();
        fs::write(dir.path().join("b.txt"), "x\nx").unwrap();
        fs::write(dir.path().join("c.txt"), "x").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "x",
                "path": dir.path().to_str().unwrap(),
                "output": "count",
                "max_results": 2
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("6 matches across 3 files (showing top 2)"));
        // should have the top 2 by count, not all 3
        assert!(text.lines().filter(|l| l.contains(": ")).count() == 2);
    }

    #[tokio::test]
    async fn grep_count_with_top_n() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.txt"), "x\nx\nx\nx\nx").unwrap();
        fs::write(dir.path().join("b.txt"), "x\nx\nx").unwrap();
        fs::write(dir.path().join("c.txt"), "x\nx").unwrap();
        fs::write(dir.path().join("d.txt"), "x").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "x",
                "path": dir.path().to_str().unwrap(),
                "output": "count",
                "top_n": 2
            }))
            .await;
        let text = extract_text(&result);
        // 5+3+2+1 = 11 matches, 4 files, showing top 2
        assert!(text.contains("across 4 files"), "got: {text}");
        assert!(text.contains("showing top 2"), "got: {text}");
        assert_eq!(text.lines().filter(|l| l.contains(": ")).count(), 2);
    }

    #[tokio::test]
    async fn grep_json_with_top_n() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.txt"), "y\ny\ny").unwrap();
        fs::write(dir.path().join("b.txt"), "y\ny").unwrap();
        fs::write(dir.path().join("c.txt"), "y").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "y",
                "path": dir.path().to_str().unwrap(),
                "output": "json",
                "top_n": 1
            }))
            .await;
        let text = extract_text(&result);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["total_matches"], 6);
        assert_eq!(json["total_files"], 3);
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["count"], 3);
    }

    #[tokio::test]
    async fn grep_json_mode() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.txt"), "hello\nhello").unwrap();
        fs::write(dir.path().join("b.txt"), "hello").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello",
                "path": dir.path().to_str().unwrap(),
                "output": "json"
            }))
            .await;
        let text = extract_text(&result);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["total_matches"], 3);
        assert_eq!(json["total_files"], 2);
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        // sorted desc by count
        assert!(files[0]["count"].as_u64().unwrap() >= files[1]["count"].as_u64().unwrap());
    }

    #[tokio::test]
    async fn grep_empty_after_stripping_newlines() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "hello").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "\n\n",
                "path": dir.path().to_str().unwrap()
            }))
            .await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("pattern is empty"));
    }
}
