use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::util::{clean_regex_pattern, inject_note, resolve_path, run_blocking, truncate_lines};

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
    include_hidden: bool,
    #[serde(default, deserialize_with = "mush_agent::de::uint")]
    context_before: u64,
    #[serde(default, deserialize_with = "mush_agent::de::uint")]
    context_after: u64,
    #[serde(default)]
    output: GrepOutput,
    #[serde(default, deserialize_with = "mush_agent::de::opt_uint")]
    max_results: Option<usize>,
    #[serde(default, deserialize_with = "mush_agent::de::opt_uint")]
    top_n: Option<usize>,
}

const fn default_true() -> bool {
    true
}

const MAX_MATCHES_PER_FILE: u64 = 50;
const NO_MATCHES: &str = "No matches found.";

pub struct GrepTool {
    cwd: Arc<Path>,
}

impl GrepTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self { cwd }
    }
}

#[async_trait::async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn label(&self) -> &str {
        "Grep"
    }
    fn description(&self) -> &str {
        "Search file contents using ripgrep. Respects .gitignore and skips hidden files \
         unless include_hidden is set. \
         Returns matching lines with file paths and line numbers. \
         The pattern is a raw regex; do not wrap it in quotes (e.g. pass `fn main`, not `\"fn main\"`). \
         Use 'count' output to get per-file match counts instead of full lines. \
         Use 'files' output to get just filenames with matches. \
         Always use this tool instead of running grep or rg via the bash tool."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "description": "regex pattern to search for. raw regex, no quote-wrapping",
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
                "include_hidden": {
                    "description": "also search hidden files and directories (dotfiles). gitignored paths stay excluded (default false)",
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

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Head
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
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

        let cleaned = clean_regex_pattern(&args.pattern);
        let mut args = args;
        args.pattern = cleaned.pattern;

        let result = run_blocking(move || run_search(&search_path, &args)).await;
        inject_note(result, cleaned.note)
    }
}

fn run_search(search_path: &Path, args: &GrepArgs) -> ToolResult {
    let pattern: String = args
        .pattern
        .chars()
        .filter(|&c| c != '\n' && c != '\r')
        .collect();

    if pattern.is_empty() {
        return ToolResult::error("pattern is empty (after stripping newlines)");
    }

    let mut builder = RegexMatcherBuilder::new();
    if matches!(args.mode, GrepMode::Literal) {
        builder.fixed_strings(true);
    }
    if !args.case_sensitive {
        builder.case_insensitive(true);
    }
    if args.whole_word {
        builder.word(true);
    }
    let matcher = match builder.build(&pattern) {
        Ok(m) => m,
        Err(e) => return ToolResult::error(format!("invalid pattern: {e}")),
    };

    let files = match collect_files(search_path, args.include.as_deref(), args.include_hidden) {
        Ok(f) => f,
        Err(e) => return ToolResult::error(e.to_string()),
    };

    if files.is_empty() {
        return ToolResult::text(NO_MATCHES);
    }

    match args.output {
        GrepOutput::Lines => search_lines(
            &matcher,
            &files,
            search_path,
            args.context_before as usize,
            args.context_after as usize,
        ),
        GrepOutput::Count | GrepOutput::Json => search_count(
            &matcher,
            &files,
            search_path,
            args.output,
            args.max_results,
            args.top_n,
        ),
        GrepOutput::Files => search_files(&matcher, &files, search_path, args.max_results),
    }
}

fn collect_files(
    search_path: &Path,
    include: Option<&str>,
    include_hidden: bool,
) -> io::Result<Vec<PathBuf>> {
    if search_path.is_file() {
        return Ok(vec![search_path.to_path_buf()]);
    }

    if !search_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("path not found: {}", search_path.display()),
        ));
    }

    let mut walk = WalkBuilder::new(search_path);
    walk.hidden(!include_hidden).git_ignore(true).parents(true);

    let include_matcher = include
        .map(|glob| {
            globset::Glob::new(glob)
                .and_then(|g| globset::GlobSetBuilder::new().add(g).build())
                .map_err(|e| io::Error::other(format!("invalid include glob {glob:?}: {e}")))
        })
        .transpose()?;

    let mut files = Vec::new();
    for entry in walk.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(%error, "grep: skipping unreadable entry");
                continue;
            }
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        if let Some(ref matcher) = include_matcher {
            let path = entry.path();
            let matches =
                matcher.is_match(path) || path.file_name().is_some_and(|n| matcher.is_match(n));
            if !matches {
                continue;
            }
        }
        files.push(entry.into_path());
    }
    files.sort();
    Ok(files)
}

fn display_path(path: &Path, search_root: &Path) -> String {
    path.strip_prefix(search_root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn searcher(ctx_before: usize, ctx_after: usize) -> Searcher {
    SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .before_context(ctx_before)
        .after_context(ctx_after)
        .build()
}

fn search_lines(
    matcher: &grep_regex::RegexMatcher,
    files: &[PathBuf],
    search_root: &Path,
    ctx_before: usize,
    ctx_after: usize,
) -> ToolResult {
    let mut searcher = searcher(ctx_before, ctx_after);

    let mut output_lines: Vec<String> = Vec::new();

    for file in files {
        let rel = display_path(file, search_root);
        let mut sink = LineSink {
            path: &rel,
            lines: &mut output_lines,
            match_count: 0,
            needs_separator: false,
        };
        let _ = searcher.search_path(matcher, file, &mut sink);
    }

    if output_lines.is_empty() {
        return ToolResult::text(NO_MATCHES);
    }

    let lines_ref: Vec<&str> = output_lines.iter().map(String::as_str).collect();
    ToolResult::text(truncate_lines(&lines_ref, "matches"))
}

struct LineSink<'a> {
    path: &'a str,
    lines: &'a mut Vec<String>,
    match_count: u64,
    needs_separator: bool,
}

impl LineSink<'_> {
    fn push_line(&mut self, separator: char, line_number: Option<u64>, bytes: &[u8]) {
        let text = std::str::from_utf8(bytes)
            .unwrap_or("<binary>")
            .trim_end_matches('\n')
            .trim_end_matches('\r');
        self.lines.push(format!(
            "{}{separator}{}{separator}{text}",
            self.path,
            line_number.unwrap_or(0)
        ));
    }
}

impl Sink for LineSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        if self.match_count >= MAX_MATCHES_PER_FILE {
            return Ok(false);
        }
        self.match_count += 1;

        self.push_line(':', mat.line_number(), mat.bytes());
        self.needs_separator = true;
        Ok(true)
    }

    fn context(&mut self, _searcher: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, io::Error> {
        self.push_line('-', ctx.line_number(), ctx.bytes());
        Ok(true)
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, io::Error> {
        if self.needs_separator {
            self.lines.push("--".to_string());
            self.needs_separator = false;
        }
        Ok(true)
    }
}

fn search_count(
    matcher: &grep_regex::RegexMatcher,
    files: &[PathBuf],
    search_root: &Path,
    output_mode: GrepOutput,
    max_results: Option<usize>,
    top_n: Option<usize>,
) -> ToolResult {
    let mut searcher = searcher(0, 0);

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
        return ToolResult::text(NO_MATCHES);
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

fn search_files(
    matcher: &grep_regex::RegexMatcher,
    files: &[PathBuf],
    search_root: &Path,
    max_results: Option<usize>,
) -> ToolResult {
    let mut searcher = searcher(0, 0);

    let mut matching_files: Vec<String> = Vec::new();

    for file in files {
        let mut found = false;
        let _ = searcher.search_path(
            matcher,
            file,
            grep_searcher::sinks::UTF8(|_, _| {
                found = true;
                Ok(false)
            }),
        );
        if found {
            matching_files.push(display_path(file, search_root));
        }
    }

    if matching_files.is_empty() {
        return ToolResult::text(NO_MATCHES);
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
    async fn a_stringified_bool_reaches_grep() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "HELLO world\n").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .call(serde_json::json!({
                "pattern": "hello",
                "path": dir.path().to_str().unwrap(),
                "case_sensitive": "false"
            }))
            .await;
        let text = extract_text(&result);
        assert!(result.outcome.is_success(), "{text}");
        assert!(text.contains("HELLO world"), "{text}");
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

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_skips_unreadable_dir_instead_of_aborting() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir();
        fs::write(dir.path().join("good.txt"), "needle here").unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("inner.txt"), "needle hidden").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        if fs::read_dir(&locked).is_ok() {
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "needle",
                "path": dir.path().to_str().unwrap()
            }))
            .await;

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let text = extract_text(&result);
        assert!(
            !text.contains("failed to walk directory"),
            "walk should skip the unreadable dir, not abort: {text}"
        );
        assert!(
            text.contains("good.txt") && text.contains("needle here"),
            "should still find the readable file: {text}"
        );
    }

    #[tokio::test]
    async fn grep_missing_path_reports_not_found() {
        let dir = temp_dir();
        let missing = dir.path().join("does-not-exist");
        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "needle",
                "path": missing.to_str().unwrap()
            }))
            .await;
        assert!(result.outcome.is_error(), "missing path should be an error");
        let text = extract_text(&result);
        assert!(text.contains("path not found"), "{text}");
    }

    #[tokio::test]
    async fn grep_skips_hidden_files_unless_asked() {
        let dir = temp_dir();
        fs::write(dir.path().join(".hidden.txt"), "needle in dotfile").unwrap();
        let hidden_dir = dir.path().join(".config");
        fs::create_dir(&hidden_dir).unwrap();
        fs::write(hidden_dir.join("conf.toml"), "needle in dotdir").unwrap();
        fs::write(dir.path().join("visible.txt"), "needle in plain sight").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let default = extract_text(
            &tool
                .execute(serde_json::json!({
                    "pattern": "needle",
                    "path": dir.path().to_str().unwrap()
                }))
                .await,
        );
        assert!(default.contains("visible.txt"), "got: {default}");
        assert!(!default.contains(".hidden.txt"), "got: {default}");
        assert!(!default.contains("conf.toml"), "got: {default}");

        let with_hidden = extract_text(
            &tool
                .execute(serde_json::json!({
                    "pattern": "needle",
                    "path": dir.path().to_str().unwrap(),
                    "include_hidden": true
                }))
                .await,
        );
        assert!(with_hidden.contains("visible.txt"), "got: {with_hidden}");
        assert!(with_hidden.contains(".hidden.txt"), "got: {with_hidden}");
        assert!(with_hidden.contains("conf.toml"), "got: {with_hidden}");
    }

    #[tokio::test]
    async fn grep_include_hidden_still_respects_gitignore() {
        let dir = temp_dir();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("generated.rs"), "needle generated").unwrap();
        fs::write(dir.path().join(".envrc"), "needle envrc").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let text = extract_text(
            &tool
                .execute(serde_json::json!({
                    "pattern": "needle",
                    "path": dir.path().to_str().unwrap(),
                    "include_hidden": true
                }))
                .await,
        );
        assert!(text.contains(".envrc"), "got: {text}");
        assert!(!text.contains("generated.rs"), "got: {text}");
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
    async fn grep_include_recurses_into_subdirectories() {
        let dir = temp_dir();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("nested.rs"), "ThinkingLevel::High").unwrap();
        fs::write(sub.join("nested.txt"), "ThinkingLevel noise").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "ThinkingLevel",
                "path": dir.path().to_str().unwrap(),
                "include": "*.rs"
            }))
            .await;
        let text = extract_text(&result);
        assert!(
            text.contains("nested.rs"),
            "expected match in nested.rs, got: {text}"
        );
        assert!(
            !text.contains("nested.txt"),
            "nested.txt should be excluded by include glob, got: {text}"
        );
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
        assert!(files[0]["count"].as_u64().unwrap() >= files[1]["count"].as_u64().unwrap());
    }

    #[tokio::test]
    async fn grep_strips_trailing_unmatched_quote_and_notes_it() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "hello world\n").unwrap();

        let tool = GrepTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello\"",
                "path": dir.path().to_str().unwrap()
            }))
            .await;
        let text = extract_text(&result);
        assert!(
            text.contains("hello world"),
            "must run cleaned pattern and find the match (got: {text})"
        );
        assert!(
            text.contains("trailing unmatched"),
            "must surface a note about the strip (got: {text})"
        );
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
