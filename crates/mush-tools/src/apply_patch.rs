//! apply_patch tool - codex-style patch format for GPT models
//!
//! parses and applies the patch format that GPT models are trained on.
//! replaces edit + write when using gpt-* models (not gpt-4, not oss variants).
//!
//! format:
//! ```text
//! *** Begin Patch
//! *** Add File: path/to/new.rs
//! +line one
//! +line two
//! *** Update File: path/to/existing.rs
//! @@ fn some_context()
//!  unchanged line
//! -old line
//! +new line
//! *** Delete File: path/to/remove.rs
//! *** End Patch
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::util::resolve_path;

// -- types --

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyPatchArgs {
    patch_text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateChunk {
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub context: Option<String>,
    pub is_eof: bool,
}

// -- parser --

#[derive(Debug)]
pub enum PatchError {
    MissingMarkers,
    ContextNotFound {
        context: String,
        path: String,
    },
    LinesNotFound {
        path: String,
        lines: String,
    },
    ReadFile {
        path: String,
        source: std::io::Error,
    },
    EmptyPatch,
    Io(std::io::Error),
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingMarkers => write!(f, "invalid patch format: missing Begin/End markers"),
            Self::ContextNotFound { context, path } => {
                write!(f, "failed to find context '{context}' in {path}")
            }
            Self::LinesNotFound { path, lines } => {
                write!(f, "failed to find expected lines in {path}:\n{lines}")
            }
            Self::ReadFile { path, source } => write!(f, "failed to read {path}: {source}"),
            Self::EmptyPatch => write!(f, "no hunks found in patch"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PatchError {}

impl From<std::io::Error> for PatchError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// strip heredoc wrapper if present
fn strip_heredoc(input: &str) -> &str {
    let trimmed = input.trim();
    // simple check: if it starts with cat <<'EOF' or <<EOF pattern, strip
    if let Some(rest) = trimmed.strip_prefix("cat ")
        && let Some(body_start) = rest.find('\n')
    {
        let header = &rest[..body_start];
        if header.contains("<<") {
            // extract delimiter
            let delim = header
                .split("<<")
                .nth(1)
                .unwrap_or("")
                .trim()
                .trim_matches(|c| c == '\'' || c == '"');
            if let Some(end_pos) = rest.rfind(delim) {
                let body = &rest[body_start + 1..end_pos];
                return body.trim();
            }
        }
    }
    trimmed
}

pub fn parse_patch(patch_text: &str) -> Result<Vec<Hunk>, PatchError> {
    let cleaned = strip_heredoc(patch_text);
    let lines: Vec<&str> = cleaned.lines().collect();

    let begin = lines
        .iter()
        .position(|l| l.trim() == "*** Begin Patch")
        .ok_or(PatchError::MissingMarkers)?;
    let end = lines
        .iter()
        .position(|l| l.trim() == "*** End Patch")
        .ok_or(PatchError::MissingMarkers)?;

    if begin >= end {
        return Err(PatchError::MissingMarkers);
    }

    let mut hunks = Vec::new();
    let mut i = begin + 1;

    while i < end {
        let line = lines[i];

        if let Some(path) = line.strip_prefix("*** Add File:") {
            let path = path.trim().to_string();
            i += 1;
            let mut contents = String::new();
            while i < end && !lines[i].starts_with("***") {
                if let Some(rest) = lines[i].strip_prefix('+') {
                    if !contents.is_empty() {
                        contents.push('\n');
                    }
                    contents.push_str(rest);
                }
                i += 1;
            }
            hunks.push(Hunk::Add { path, contents });
        } else if let Some(path) = line.strip_prefix("*** Delete File:") {
            hunks.push(Hunk::Delete {
                path: path.trim().to_string(),
            });
            i += 1;
        } else if let Some(path) = line.strip_prefix("*** Update File:") {
            let path = path.trim().to_string();
            i += 1;

            let move_path = if i < end && lines[i].starts_with("*** Move to:") {
                let mp = lines[i]
                    .strip_prefix("*** Move to:")
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                i += 1;
                Some(mp)
            } else {
                None
            };

            let mut chunks = Vec::new();
            while i < end && !lines[i].starts_with("*** ") {
                if lines[i].starts_with("@@") {
                    let context_line = lines[i][2..].trim();
                    let context = if context_line.is_empty() {
                        None
                    } else {
                        Some(context_line.to_string())
                    };
                    i += 1;

                    let mut old_lines = Vec::new();
                    let mut new_lines = Vec::new();
                    let mut is_eof = false;

                    while i < end
                        && !lines[i].starts_with("@@")
                        && (!lines[i].starts_with("*** ") || lines[i] == "*** End of File")
                    {
                        if lines[i] == "*** End of File" {
                            is_eof = true;
                            i += 1;
                            break;
                        }
                        if let Some(rest) = lines[i].strip_prefix(' ') {
                            old_lines.push(rest.to_string());
                            new_lines.push(rest.to_string());
                        } else if let Some(rest) = lines[i].strip_prefix('-') {
                            old_lines.push(rest.to_string());
                        } else if let Some(rest) = lines[i].strip_prefix('+') {
                            new_lines.push(rest.to_string());
                        }
                        i += 1;
                    }

                    chunks.push(UpdateChunk {
                        old_lines,
                        new_lines,
                        context,
                        is_eof,
                    });
                } else {
                    i += 1;
                }
            }

            hunks.push(Hunk::Update {
                path,
                move_path,
                chunks,
            });
        } else {
            i += 1;
        }
    }

    if hunks.is_empty() {
        return Err(PatchError::EmptyPatch);
    }

    Ok(hunks)
}

// -- application --

/// multi-pass line matching: exact, rstrip, trim, normalised unicode
fn seek_sequence(lines: &[&str], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() || lines.len() < pattern.len() {
        return None;
    }

    let comparators: Vec<fn(&str, &str) -> bool> = vec![
        |a, b| a == b,
        |a, b| a.trim_end() == b.trim_end(),
        |a, b| a.trim() == b.trim(),
        |a, b| normalise_unicode(a.trim()) == normalise_unicode(b.trim()),
    ];

    for cmp in &comparators {
        // if eof anchor, try from end first
        if eof && lines.len() >= pattern.len() {
            let from_end = lines.len() - pattern.len();
            if from_end >= start
                && pattern
                    .iter()
                    .enumerate()
                    .all(|(j, p)| cmp(lines[from_end + j], p))
            {
                return Some(from_end);
            }
        }

        for i in start..=lines.len().saturating_sub(pattern.len()) {
            if pattern
                .iter()
                .enumerate()
                .all(|(j, p)| cmp(lines[i + j], p))
            {
                return Some(i);
            }
        }
    }

    None
}

/// normalise unicode punctuation to ascii equivalents
fn normalise_unicode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' => '-',
            '\u{00A0}' => ' ',
            _ => c,
        })
        .collect::<String>()
        .replace('\u{2026}', "...")
}

/// compute replacements for a file's update chunks
fn compute_replacements(
    original_lines: &[&str],
    file_path: &str,
    chunks: &[UpdateChunk],
) -> Result<Vec<(usize, usize, Vec<String>)>, PatchError> {
    let mut replacements = Vec::new();
    let mut line_idx = 0;

    for chunk in chunks {
        if let Some(ref ctx) = chunk.context {
            let ctx_vec = vec![ctx.clone()];
            match seek_sequence(original_lines, &ctx_vec, line_idx, false) {
                Some(idx) => line_idx = idx + 1,
                None => {
                    return Err(PatchError::ContextNotFound {
                        context: ctx.clone(),
                        path: file_path.to_string(),
                    });
                }
            }
        }

        // pure addition (no old lines to match)
        if chunk.old_lines.is_empty() {
            let insertion_idx = if !original_lines.is_empty()
                && original_lines.last().is_some_and(|l| l.is_empty())
            {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern = chunk.old_lines.clone();
        let mut new_slice = chunk.new_lines.clone();

        let mut found = seek_sequence(original_lines, &pattern, line_idx, chunk.is_eof);

        // retry without trailing empty line
        if found.is_none() && pattern.last().is_some_and(|l| l.is_empty()) {
            pattern.pop();
            if new_slice.last().is_some_and(|l| l.is_empty()) {
                new_slice.pop();
            }
            found = seek_sequence(original_lines, &pattern, line_idx, chunk.is_eof);
        }

        match found {
            Some(idx) => {
                replacements.push((idx, pattern.len(), new_slice));
                line_idx = idx + pattern.len();
            }
            None => {
                return Err(PatchError::LinesNotFound {
                    path: file_path.to_string(),
                    lines: chunk.old_lines.join("\n"),
                });
            }
        }
    }

    replacements.sort_by_key(|r| r.0);
    Ok(replacements)
}

/// apply sorted replacements in reverse order
fn apply_replacements(lines: &[&str], replacements: &[(usize, usize, Vec<String>)]) -> Vec<String> {
    let mut result: Vec<String> = lines.iter().map(|s| s.to_string()).collect();

    for &(start, old_len, ref new_segment) in replacements.iter().rev() {
        result.splice(start..start + old_len, new_segment.iter().cloned());
    }

    result
}

/// derive new file contents by applying update chunks
fn derive_new_contents(file_path: &Path, chunks: &[UpdateChunk]) -> Result<String, PatchError> {
    let original = fs::read_to_string(file_path).map_err(|e| PatchError::ReadFile {
        path: file_path.display().to_string(),
        source: e,
    })?;

    let mut original_lines: Vec<&str> = original.split('\n').collect();
    // drop trailing empty element for consistent line counting
    if original_lines.last().is_some_and(|l| l.is_empty()) {
        original_lines.pop();
    }

    let replacements =
        compute_replacements(&original_lines, &file_path.display().to_string(), chunks)?;
    let mut new_lines = apply_replacements(&original_lines, &replacements);

    // ensure trailing newline
    if new_lines.is_empty() || !new_lines.last().is_some_and(|l| l.is_empty()) {
        new_lines.push(String::new());
    }

    Ok(new_lines.join("\n"))
}

/// apply parsed hunks to the filesystem
pub fn apply_hunks(cwd: &Path, hunks: &[Hunk]) -> Result<Vec<String>, PatchError> {
    let mut summary = Vec::new();

    for hunk in hunks {
        match hunk {
            Hunk::Add { path, contents } => {
                let resolved = resolve_path(cwd, path);
                if let Some(parent) = resolved.parent() {
                    fs::create_dir_all(parent)?;
                }
                let content = if contents.is_empty() || contents.ends_with('\n') {
                    contents.clone()
                } else {
                    format!("{contents}\n")
                };
                fs::write(&resolved, content)?;
                summary.push(format!("A {path}"));
            }
            Hunk::Delete { path } => {
                let resolved = resolve_path(cwd, path);
                fs::remove_file(&resolved)?;
                summary.push(format!("D {path}"));
            }
            Hunk::Update {
                path,
                move_path,
                chunks,
            } => {
                let resolved = resolve_path(cwd, path);
                let new_content = derive_new_contents(&resolved, chunks)?;

                if let Some(mp) = move_path {
                    let dest = resolve_path(cwd, mp);
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&dest, &new_content)?;
                    fs::remove_file(&resolved)?;
                    summary.push(format!("M {mp} (moved from {path})"));
                } else {
                    fs::write(&resolved, &new_content)?;
                    summary.push(format!("M {path}"));
                }
            }
        }
    }

    Ok(summary)
}

// -- tool --

const DESCRIPTION: &str = "\
Use the apply_patch tool to make file changes. The patch format supports creating, updating, \
deleting, and moving files.\n\n\
Format:\n\
```\n\
*** Begin Patch\n\
*** Add File: path/to/new.txt\n\
+new file line one\n\
+new file line two\n\
*** Update File: path/to/existing.txt\n\
@@ def some_function():\n\
 unchanged context line\n\
-old line to remove\n\
+new line to add\n\
*** Delete File: path/to/remove.txt\n\
*** End Patch\n\
```\n\n\
Rules:\n\
- every patch must have *** Begin Patch and *** End Patch markers\n\
- each file section starts with *** Add File:, *** Update File:, or *** Delete File:\n\
- new file lines must be prefixed with +\n\
- update sections use @@ with optional context to locate the change\n\
- in update sections: space prefix = unchanged, - prefix = remove, + prefix = add\n\
- to move a file, add *** Move to: new/path after *** Update File:\n\
- do NOT re-run to verify changes; trust the output summary";

pub struct ApplyPatchTool {
    cwd: PathBuf,
}

impl ApplyPatchTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

impl AgentTool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn label(&self) -> &str {
        "ApplyPatch"
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "patch_text": {
                    "type": "string",
                    "description": "the full patch text describing all changes to make"
                }
            },
            "required": ["patch_text"]
        })
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let args = match parse_tool_args::<ApplyPatchArgs>(args) {
                Ok(args) => args,
                Err(error) => return error,
            };
            if args.patch_text.is_empty() {
                return ToolResult::error("patch_text is required");
            }

            let hunks = match parse_patch(&args.patch_text) {
                Ok(h) => h,
                Err(e) => return ToolResult::error(format!("patch parse error: {e}")),
            };

            match apply_hunks(&self.cwd, &hunks) {
                Ok(summary) => {
                    let output = format!("applied patch successfully:\n{}", summary.join("\n"));
                    ToolResult::text(output)
                }
                Err(e) => ToolResult::error(format!("patch apply error: {e}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parse_add_file() {
        let patch = "\
*** Begin Patch
*** Add File: hello.txt
+Hello World
+Line two
*** End Patch";
        let hunks = parse_patch(patch).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            hunks[0],
            Hunk::Add {
                path: "hello.txt".into(),
                contents: "Hello World\nLine two".into(),
            }
        );
    }

    #[test]
    fn parse_delete_file() {
        let patch = "\
*** Begin Patch
*** Delete File: old.txt
*** End Patch";
        let hunks = parse_patch(patch).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            hunks[0],
            Hunk::Delete {
                path: "old.txt".into()
            }
        );
    }

    #[test]
    fn parse_update_with_context() {
        let patch = "\
*** Begin Patch
*** Update File: src/main.rs
@@ fn greet()
 println!(\"Hi\");
-old line
+new line
*** End Patch";
        let hunks = parse_patch(patch).unwrap();
        assert_eq!(hunks.len(), 1);
        match &hunks[0] {
            Hunk::Update { path, chunks, .. } => {
                assert_eq!(path, "src/main.rs");
                assert_eq!(chunks.len(), 1);
                assert_eq!(chunks[0].context.as_deref(), Some("fn greet()"));
                assert_eq!(chunks[0].old_lines, vec!["println!(\"Hi\");", "old line"]);
                assert_eq!(chunks[0].new_lines, vec!["println!(\"Hi\");", "new line"]);
            }
            _ => panic!("expected update hunk"),
        }
    }

    #[test]
    fn parse_move_file() {
        let patch = "\
*** Begin Patch
*** Update File: old.rs
*** Move to: new.rs
@@ fn main()
-old
+new
*** End Patch";
        let hunks = parse_patch(patch).unwrap();
        match &hunks[0] {
            Hunk::Update {
                path, move_path, ..
            } => {
                assert_eq!(path, "old.rs");
                assert_eq!(move_path.as_deref(), Some("new.rs"));
            }
            _ => panic!("expected update hunk"),
        }
    }

    #[test]
    fn parse_multiple_hunks() {
        let patch = "\
*** Begin Patch
*** Add File: new.txt
+hello
*** Delete File: gone.txt
*** Update File: mod.rs
@@
-old
+new
*** End Patch";
        let hunks = parse_patch(patch).unwrap();
        assert_eq!(hunks.len(), 3);
    }

    #[test]
    fn parse_missing_markers_errors() {
        assert!(parse_patch("no markers here").is_err());
    }

    #[test]
    fn parse_empty_patch_errors() {
        let patch = "*** Begin Patch\n*** End Patch";
        assert!(parse_patch(patch).is_err());
    }

    #[test]
    fn apply_add_file() {
        let dir = TempDir::new().unwrap();
        let hunks = vec![Hunk::Add {
            path: "hello.txt".into(),
            contents: "Hello World".into(),
        }];
        let summary = apply_hunks(dir.path(), &hunks).unwrap();
        assert_eq!(summary, vec!["A hello.txt"]);
        let content = fs::read_to_string(dir.path().join("hello.txt")).unwrap();
        assert_eq!(content, "Hello World\n");
    }

    #[test]
    fn apply_add_nested_dirs() {
        let dir = TempDir::new().unwrap();
        let hunks = vec![Hunk::Add {
            path: "deep/nested/file.txt".into(),
            contents: "nested".into(),
        }];
        apply_hunks(dir.path(), &hunks).unwrap();
        assert!(dir.path().join("deep/nested/file.txt").exists());
    }

    #[test]
    fn apply_delete_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("gone.txt"), "bye").unwrap();
        let hunks = vec![Hunk::Delete {
            path: "gone.txt".into(),
        }];
        let summary = apply_hunks(dir.path(), &hunks).unwrap();
        assert_eq!(summary, vec!["D gone.txt"]);
        assert!(!dir.path().join("gone.txt").exists());
    }

    #[test]
    fn apply_update_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.txt"), "line 1\nline 2\nline 3\n").unwrap();

        let hunks = vec![Hunk::Update {
            path: "test.txt".into(),
            move_path: None,
            chunks: vec![UpdateChunk {
                old_lines: vec!["line 1".into(), "line 2".into()],
                new_lines: vec!["line 1".into(), "LINE 2".into()],
                context: None,
                is_eof: false,
            }],
        }];

        apply_hunks(dir.path(), &hunks).unwrap();
        let content = fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert_eq!(content, "line 1\nLINE 2\nline 3\n");
    }

    #[test]
    fn apply_update_multiple_chunks() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.txt"),
            "line 1\nline 2\nline 3\nline 4\n",
        )
        .unwrap();

        let hunks = vec![Hunk::Update {
            path: "test.txt".into(),
            move_path: None,
            chunks: vec![
                UpdateChunk {
                    old_lines: vec!["line 1".into(), "line 2".into()],
                    new_lines: vec!["line 1".into(), "LINE 2".into()],
                    context: None,
                    is_eof: false,
                },
                UpdateChunk {
                    old_lines: vec!["line 3".into(), "line 4".into()],
                    new_lines: vec!["line 3".into(), "LINE 4".into()],
                    context: None,
                    is_eof: false,
                },
            ],
        }];

        apply_hunks(dir.path(), &hunks).unwrap();
        let content = fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert_eq!(content, "line 1\nLINE 2\nline 3\nLINE 4\n");
    }

    #[test]
    fn apply_move_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("old.txt"), "old content\n").unwrap();

        let hunks = vec![Hunk::Update {
            path: "old.txt".into(),
            move_path: Some("new.txt".into()),
            chunks: vec![UpdateChunk {
                old_lines: vec!["old content".into()],
                new_lines: vec!["new content".into()],
                context: None,
                is_eof: false,
            }],
        }];

        apply_hunks(dir.path(), &hunks).unwrap();
        assert!(!dir.path().join("old.txt").exists());
        let content = fs::read_to_string(dir.path().join("new.txt")).unwrap();
        assert_eq!(content, "new content\n");
    }

    #[test]
    fn apply_update_with_context_seek() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n\nfn other() {\n    println!(\"bye\");\n}\n",
        )
        .unwrap();

        let hunks = vec![Hunk::Update {
            path: "test.rs".into(),
            move_path: None,
            chunks: vec![UpdateChunk {
                old_lines: vec!["    println!(\"bye\");".into()],
                new_lines: vec!["    println!(\"goodbye\");".into()],
                context: Some("fn other() {".into()),
                is_eof: false,
            }],
        }];

        apply_hunks(dir.path(), &hunks).unwrap();
        let content = fs::read_to_string(dir.path().join("test.rs")).unwrap();
        assert!(content.contains("goodbye"));
        assert!(content.contains("hello")); // first fn unchanged
    }

    #[test]
    fn apply_whitespace_tolerant_matching() {
        let dir = TempDir::new().unwrap();
        // file has trailing spaces
        fs::write(dir.path().join("test.txt"), "line one  \nline two\n").unwrap();

        let hunks = vec![Hunk::Update {
            path: "test.txt".into(),
            move_path: None,
            chunks: vec![UpdateChunk {
                // patch doesn't have trailing spaces
                old_lines: vec!["line one".into()],
                new_lines: vec!["LINE ONE".into()],
                context: None,
                is_eof: false,
            }],
        }];

        apply_hunks(dir.path(), &hunks).unwrap();
        let content = fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert!(content.contains("LINE ONE"));
    }

    #[test]
    fn apply_eof_anchor_matches_end() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.txt"),
            "first\nsecond\nthird\nlast line\n",
        )
        .unwrap();

        let hunks = vec![Hunk::Update {
            path: "test.txt".into(),
            move_path: None,
            chunks: vec![UpdateChunk {
                old_lines: vec!["last line".into()],
                new_lines: vec!["final line".into()],
                context: None,
                is_eof: true,
            }],
        }];

        apply_hunks(dir.path(), &hunks).unwrap();
        let content = fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert_eq!(content, "first\nsecond\nthird\nfinal line\n");
    }

    #[test]
    fn apply_eof_anchor_prefers_end_over_earlier_match() {
        let dir = TempDir::new().unwrap();
        // "dupe" appears twice, eof anchor should match the last one
        fs::write(dir.path().join("test.txt"), "dupe\nother\ndupe\n").unwrap();

        let hunks = vec![Hunk::Update {
            path: "test.txt".into(),
            move_path: None,
            chunks: vec![UpdateChunk {
                old_lines: vec!["dupe".into()],
                new_lines: vec!["replaced".into()],
                context: None,
                is_eof: true,
            }],
        }];

        apply_hunks(dir.path(), &hunks).unwrap();
        let content = fs::read_to_string(dir.path().join("test.txt")).unwrap();
        // should replace the last occurrence
        assert_eq!(content, "dupe\nother\nreplaced\n");
    }

    #[test]
    fn apply_move_with_multiple_chunks() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("old.rs"),
            "fn a() {\n    old_a();\n}\n\nfn b() {\n    old_b();\n}\n",
        )
        .unwrap();

        let hunks = vec![Hunk::Update {
            path: "old.rs".into(),
            move_path: Some("new.rs".into()),
            chunks: vec![
                UpdateChunk {
                    old_lines: vec!["    old_a();".into()],
                    new_lines: vec!["    new_a();".into()],
                    context: Some("fn a() {".into()),
                    is_eof: false,
                },
                UpdateChunk {
                    old_lines: vec!["    old_b();".into()],
                    new_lines: vec!["    new_b();".into()],
                    context: Some("fn b() {".into()),
                    is_eof: false,
                },
            ],
        }];

        apply_hunks(dir.path(), &hunks).unwrap();
        assert!(!dir.path().join("old.rs").exists());
        let content = fs::read_to_string(dir.path().join("new.rs")).unwrap();
        assert!(content.contains("new_a()"));
        assert!(content.contains("new_b()"));
        assert!(!content.contains("old_a()"));
        assert!(!content.contains("old_b()"));
    }

    #[test]
    fn apply_context_disambiguates_duplicate_lines() {
        let dir = TempDir::new().unwrap();
        // same body line in two different functions
        fs::write(
            dir.path().join("test.rs"),
            "fn alpha() {\n    process();\n}\n\nfn beta() {\n    process();\n}\n",
        )
        .unwrap();

        let hunks = vec![Hunk::Update {
            path: "test.rs".into(),
            move_path: None,
            chunks: vec![UpdateChunk {
                old_lines: vec!["    process();".into()],
                new_lines: vec!["    handle();".into()],
                context: Some("fn beta() {".into()),
                is_eof: false,
            }],
        }];

        apply_hunks(dir.path(), &hunks).unwrap();
        let content = fs::read_to_string(dir.path().join("test.rs")).unwrap();
        // alpha's process() should be untouched
        assert!(content.contains("fn alpha() {\n    process();\n}"));
        assert!(content.contains("fn beta() {\n    handle();\n}"));
    }

    #[test]
    fn parse_eof_marker() {
        let patch = "\
*** Begin Patch
*** Update File: test.txt
@@
 context
-old last
+new last
*** End of File
*** End Patch";
        let hunks = parse_patch(patch).unwrap();
        match &hunks[0] {
            Hunk::Update { chunks, .. } => {
                assert!(chunks[0].is_eof);
            }
            _ => panic!("expected update hunk"),
        }
    }

    #[test]
    fn roundtrip_eof_anchor() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.txt"), "first\nsecond\nthird\nlast\n").unwrap();

        let patch = "\
*** Begin Patch
*** Update File: test.txt
@@
 third
-last
+final
*** End of File
*** End Patch";

        let hunks = parse_patch(patch).unwrap();
        match &hunks[0] {
            Hunk::Update { chunks, .. } => assert!(chunks[0].is_eof),
            _ => panic!("expected update"),
        }
        apply_hunks(dir.path(), &hunks).unwrap();
        let content = fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert_eq!(content, "first\nsecond\nthird\nfinal\n");
    }

    #[test]
    fn apply_pure_addition_no_old_lines() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.txt"), "existing content\n").unwrap();

        let hunks = vec![Hunk::Update {
            path: "test.txt".into(),
            move_path: None,
            chunks: vec![UpdateChunk {
                old_lines: vec![],
                new_lines: vec!["appended line".into()],
                context: None,
                is_eof: false,
            }],
        }];

        apply_hunks(dir.path(), &hunks).unwrap();
        let content = fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert!(content.contains("appended line"));
        assert!(content.contains("existing content"));
    }

    #[test]
    fn apply_eof_with_context() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.rs"),
            "fn main() {\n    hello();\n}\n\nfn cleanup() {\n    done();\n}\n",
        )
        .unwrap();

        let hunks = vec![Hunk::Update {
            path: "test.rs".into(),
            move_path: None,
            chunks: vec![UpdateChunk {
                old_lines: vec!["    done();".into()],
                new_lines: vec!["    finished();".into()],
                context: Some("fn cleanup() {".into()),
                is_eof: true,
            }],
        }];

        apply_hunks(dir.path(), &hunks).unwrap();
        let content = fs::read_to_string(dir.path().join("test.rs")).unwrap();
        assert!(content.contains("finished()"));
        assert!(content.contains("hello()")); // first fn untouched
    }

    #[test]
    fn apply_move_with_no_content_changes() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("old.txt"), "keep this\n").unwrap();

        let hunks = vec![Hunk::Update {
            path: "old.txt".into(),
            move_path: Some("renamed.txt".into()),
            chunks: vec![],
        }];

        // no chunks = no content changes, just a rename
        // this should fail because derive_new_contents has no changes
        // but the original content is still read and written to the new path
        let result = apply_hunks(dir.path(), &hunks);
        assert!(result.is_ok());
        assert!(!dir.path().join("old.txt").exists());
        let content = fs::read_to_string(dir.path().join("renamed.txt")).unwrap();
        assert_eq!(content, "keep this\n");
    }

    #[test]
    fn apply_context_not_found_errors() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.rs"), "fn main() {\n}\n").unwrap();

        let hunks = vec![Hunk::Update {
            path: "test.rs".into(),
            move_path: None,
            chunks: vec![UpdateChunk {
                old_lines: vec!["old".into()],
                new_lines: vec!["new".into()],
                context: Some("fn nonexistent() {".into()),
                is_eof: false,
            }],
        }];

        let result = apply_hunks(dir.path(), &hunks);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("nonexistent"), "got: {err}");
    }

    #[test]
    fn apply_unicode_normalisation_matching() {
        let dir = TempDir::new().unwrap();
        // file uses unicode smart quotes
        fs::write(
            dir.path().join("test.txt"),
            "let msg = \u{201C}hello\u{201D};\n",
        )
        .unwrap();

        let hunks = vec![Hunk::Update {
            path: "test.txt".into(),
            move_path: None,
            chunks: vec![UpdateChunk {
                // patch uses ascii quotes (normalisation should match)
                old_lines: vec!["let msg = \"hello\";".into()],
                new_lines: vec!["let msg = \"world\";".into()],
                context: None,
                is_eof: false,
            }],
        }];

        apply_hunks(dir.path(), &hunks).unwrap();
        let content = fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert!(content.contains("world"));
    }

    #[test]
    fn apply_empty_file_update_errors() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("empty.txt"), "").unwrap();

        let hunks = vec![Hunk::Update {
            path: "empty.txt".into(),
            move_path: None,
            chunks: vec![UpdateChunk {
                old_lines: vec!["nonexistent".into()],
                new_lines: vec!["replaced".into()],
                context: None,
                is_eof: false,
            }],
        }];

        let result = apply_hunks(dir.path(), &hunks);
        assert!(result.is_err());
    }

    #[test]
    fn parse_heredoc_wrapped_patch() {
        let patch =
            "cat <<'EOF'\n*** Begin Patch\n*** Add File: hello.txt\n+hi\n*** End Patch\nEOF";
        let hunks = parse_patch(patch).unwrap();
        assert_eq!(hunks.len(), 1);
        match &hunks[0] {
            Hunk::Add { path, contents } => {
                assert_eq!(path, "hello.txt");
                assert_eq!(contents, "hi");
            }
            _ => panic!("expected add hunk"),
        }
    }

    #[test]
    fn apply_add_then_update_same_patch() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("existing.rs"), "fn old() {}\n").unwrap();

        let patch = "\
*** Begin Patch
*** Add File: new.txt
+first line
+second line
*** Update File: existing.rs
@@
-fn old() {}
+fn updated() {}
*** Delete File: nonexistent_but_thats_ok.txt
*** End Patch";

        let hunks = parse_patch(patch).unwrap();
        assert_eq!(hunks.len(), 3);
        // only apply the first two (delete will fail on missing file)
        let mut results = Vec::new();
        for hunk in &hunks[..2] {
            match apply_hunks(dir.path(), std::slice::from_ref(hunk)) {
                Ok(s) => results.extend(s),
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(dir.path().join("new.txt").exists());
        let content = fs::read_to_string(dir.path().join("existing.rs")).unwrap();
        assert!(content.contains("updated()"));
    }

    #[test]
    fn roundtrip_parse_and_apply() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("existing.rs"),
            "fn main() {\n    old();\n}\n",
        )
        .unwrap();

        let patch = "\
*** Begin Patch
*** Add File: new.txt
+hello world
*** Update File: existing.rs
@@ fn main() {
-    old();
+    new();
*** End Patch";

        let hunks = parse_patch(patch).unwrap();
        let summary = apply_hunks(dir.path(), &hunks).unwrap();

        assert_eq!(summary.len(), 2);
        assert!(dir.path().join("new.txt").exists());
        let content = fs::read_to_string(dir.path().join("existing.rs")).unwrap();
        assert!(content.contains("new()"));
        assert!(!content.contains("old()"));
    }
}
