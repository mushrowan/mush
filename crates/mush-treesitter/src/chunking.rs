//! AST-aware chunking for embedding
//!
//! splits source files into semantic chunks using tree-sitter syntax
//! trees. each chunk represents a meaningful code entity (function,
//! struct, impl block) with parent context (imports, enclosing types)
//! for embedding quality.
//!
//! falls back to recursive text splitting for non-parseable files.

use std::path::{Path, PathBuf};

use crate::symbols::{SymbolInfo, SymbolKind};
use crate::{Language, ParserPool};

/// a chunk of source code with metadata
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// the chunk text (includes parent context preamble)
    pub text: String,
    /// source file path
    pub file_path: PathBuf,
    /// symbol name (if from a named entity)
    pub symbol_name: Option<String>,
    /// symbol kind (if from a named entity)
    pub symbol_kind: Option<SymbolKind>,
    /// start line in the original file (0-indexed)
    pub start_line: usize,
    /// end line in the original file (0-indexed)
    pub end_line: usize,
}

/// chunking configuration
#[derive(Debug, Clone)]
pub struct ChunkOptions {
    /// target chunk size in characters (soft limit)
    pub target_size: usize,
    /// maximum chunk size (hard limit, chunks beyond this are split)
    pub max_size: usize,
    /// whether to include parent context (imports, enclosing types)
    pub include_context: bool,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        Self {
            target_size: 750,
            max_size: 1500,
            include_context: true,
        }
    }
}

/// chunk a file using AST-aware splitting
///
/// parses the file with tree-sitter, extracts top-level entities,
/// and produces chunks that respect syntactic boundaries. falls back
/// to text-based splitting if the file cannot be parsed.
pub fn chunk_file(path: &Path, opts: &ChunkOptions) -> Vec<Chunk> {
    let source = match std::fs::read(path) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let source_str = match std::str::from_utf8(&source) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    match Language::detect(path) {
        Some(lang) => {
            let pool = ParserPool::new();
            chunk_parsed(path, source_str, &source, lang, &pool, opts)
        }
        None => chunk_text(path, source_str, opts),
    }
}

/// chunk source that has already been parsed into a known language
fn chunk_parsed(
    path: &Path,
    source_str: &str,
    source_bytes: &[u8],
    language: Language,
    pool: &ParserPool,
    opts: &ChunkOptions,
) -> Vec<Chunk> {
    let tree = match pool.parse(language, source_bytes) {
        Ok(t) => t,
        Err(_) => return chunk_text(path, source_str, opts),
    };

    let symbols = crate::symbols::extract_symbols(language, source_bytes, &tree, Some(path));
    if symbols.is_empty() {
        return chunk_text(path, source_str, opts);
    }

    let lines: Vec<&str> = source_str.lines().collect();
    let preamble = if opts.include_context {
        extract_preamble(&lines, &symbols)
    } else {
        String::new()
    };

    let mut chunks = Vec::new();

    // one chunk per symbol
    for sym in &symbols {
        let body = join_lines(&lines, sym.start_line, sym.end_line);
        let text = prepend_preamble(&preamble, &body);

        if text.len() <= opts.max_size {
            chunks.push(make_chunk(path, text, sym));
        } else {
            chunks.extend(split_large_symbol(path, &body, sym, &preamble, opts));
        }
    }

    // regions between symbols (e.g. standalone comments, constants)
    for (start, end, text) in uncovered_regions(&lines, &symbols) {
        if is_only_preamble(&text) {
            continue;
        }
        let with_ctx = prepend_preamble(&preamble, &text);
        if with_ctx.len() <= opts.max_size {
            chunks.push(Chunk {
                text: with_ctx,
                file_path: path.to_path_buf(),
                symbol_name: None,
                symbol_kind: None,
                start_line: start,
                end_line: end,
            });
        } else {
            chunks.extend(accumulate_lines(path, &text, start, None, None, opts));
        }
    }

    if chunks.is_empty() {
        return chunk_text(path, source_str, opts);
    }

    chunks
}

// -- helpers --

/// everything before the first non-import symbol (imports, module decls, etc)
fn extract_preamble(lines: &[&str], symbols: &[SymbolInfo]) -> String {
    let first_def_line = symbols
        .iter()
        .filter(|s| !matches!(s.kind, SymbolKind::Import | SymbolKind::Module))
        .map(|s| s.start_line)
        .min()
        .unwrap_or(0);

    if first_def_line == 0 {
        return String::new();
    }

    let text: String = lines[..first_def_line].join("\n");
    let trimmed = text.trim();
    if trimmed.is_empty() { String::new() } else { trimmed.to_string() }
}

fn join_lines(lines: &[&str], start: usize, end: usize) -> String {
    let end = (end + 1).min(lines.len());
    lines[start..end].join("\n")
}

fn prepend_preamble(preamble: &str, body: &str) -> String {
    if preamble.is_empty() {
        body.to_string()
    } else {
        format!("{preamble}\n// ...\n\n{body}")
    }
}

fn make_chunk(path: &Path, text: String, sym: &SymbolInfo) -> Chunk {
    Chunk {
        text,
        file_path: path.to_path_buf(),
        symbol_name: Some(sym.name.clone()),
        symbol_kind: Some(sym.kind),
        start_line: sym.start_line,
        end_line: sym.end_line,
    }
}

/// split a symbol that exceeds max_size into sub-chunks
fn split_large_symbol(
    path: &Path,
    body: &str,
    sym: &SymbolInfo,
    preamble: &str,
    opts: &ChunkOptions,
) -> Vec<Chunk> {
    let sig_line = body.lines().next().unwrap_or("");
    let prefix = if preamble.is_empty() {
        format!("{sig_line}\n// ...\n\n")
    } else {
        format!("{preamble}\n// ...\n\n{sig_line}\n// ...\n\n")
    };

    accumulate_lines(
        path,
        body,
        sym.start_line,
        Some(&sym.name),
        Some(sym.kind),
        &ChunkOptions {
            // account for prefix in each sub-chunk
            target_size: opts.target_size,
            max_size: opts.max_size,
            include_context: false,
        },
    )
    .into_iter()
    .map(|mut c| {
        c.text = format!("{prefix}{}", c.text);
        c
    })
    .collect()
}

/// regions of source not covered by any extracted symbol
fn uncovered_regions(lines: &[&str], symbols: &[SymbolInfo]) -> Vec<(usize, usize, String)> {
    let mut sorted: Vec<&SymbolInfo> = symbols.iter().collect();
    sorted.sort_by_key(|s| s.start_line);

    let mut regions = Vec::new();
    let mut covered_until = 0;

    for sym in &sorted {
        if sym.start_line > covered_until {
            let text: String = lines[covered_until..sym.start_line].join("\n");
            regions.push((covered_until, sym.start_line.saturating_sub(1), text));
        }
        covered_until = covered_until.max(sym.end_line + 1);
    }

    if covered_until < lines.len() {
        let text: String = lines[covered_until..].join("\n");
        regions.push((covered_until, lines.len() - 1, text));
    }

    regions
}

/// heuristic: text that's only imports/comments/whitespace
fn is_only_preamble(text: &str) -> bool {
    text.trim().is_empty()
        || text.trim().lines().all(|line| {
            let l = line.trim();
            l.is_empty()
                || l.starts_with("use ")
                || l.starts_with("import ")
                || l.starts_with("from ")
                || l.starts_with("mod ")
                || l.starts_with("require")
                || l.starts_with('#')
                || l.starts_with("//")
                || l.starts_with("/*")
        })
}

/// shared line accumulator used by both text chunking and large-symbol splitting
fn accumulate_lines(
    path: &Path,
    text: &str,
    base_line: usize,
    symbol_name: Option<&str>,
    symbol_kind: Option<SymbolKind>,
    opts: &ChunkOptions,
) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return vec![];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut chunk_start = 0;

    for (i, line) in lines.iter().enumerate() {
        let addition = if current.is_empty() { line.len() } else { line.len() + 1 };

        // split at paragraph boundaries when over target
        let at_boundary = line.trim().is_empty() && !current.is_empty();
        if at_boundary && current.len() + addition > opts.target_size {
            chunks.push(Chunk {
                text: std::mem::take(&mut current),
                file_path: path.to_path_buf(),
                symbol_name: symbol_name.map(String::from),
                symbol_kind,
                start_line: base_line + chunk_start,
                end_line: base_line + i - 1,
            });
            chunk_start = i + 1;
            continue;
        }

        // hard split at max_size
        if !current.is_empty() && current.len() + addition > opts.max_size {
            chunks.push(Chunk {
                text: std::mem::take(&mut current),
                file_path: path.to_path_buf(),
                symbol_name: symbol_name.map(String::from),
                symbol_kind,
                start_line: base_line + chunk_start,
                end_line: base_line + i - 1,
            });
            chunk_start = i;
        }

        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }

    if !current.trim().is_empty() {
        chunks.push(Chunk {
            text: current,
            file_path: path.to_path_buf(),
            symbol_name: symbol_name.map(String::from),
            symbol_kind,
            start_line: base_line + chunk_start,
            end_line: base_line + lines.len() - 1,
        });
    }

    chunks
}

/// fallback: split plain text into chunks by paragraph boundaries
pub fn chunk_text(path: &Path, text: &str, opts: &ChunkOptions) -> Vec<Chunk> {
    accumulate_lines(path, text, 0, None, None, opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn default_opts() -> ChunkOptions {
        ChunkOptions::default()
    }

    // -- text fallback tests --

    #[test]
    fn chunk_text_small_file() {
        let text = "hello world\nthis is a test";
        let chunks = chunk_text(Path::new("test.txt"), text, &default_opts());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, text);
        assert_eq!(chunks[0].start_line, 0);
    }

    #[test]
    fn chunk_text_splits_at_paragraph_boundary() {
        let para1 = "line\n".repeat(100);
        let para2 = "more\n".repeat(100);
        let text = format!("{para1}\n{para2}");
        let opts = ChunkOptions {
            target_size: 400,
            max_size: 1500,
            include_context: true,
        };
        let chunks = chunk_text(Path::new("test.txt"), &text, &opts);
        assert!(chunks.len() >= 2, "should split: got {} chunks", chunks.len());
    }

    #[test]
    fn chunk_text_empty() {
        let chunks = chunk_text(Path::new("test.txt"), "", &default_opts());
        assert!(chunks.is_empty());
    }

    // -- AST chunking tests --

    #[test]
    fn chunk_rust_file_by_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        fs::write(
            &path,
            r#"use std::io;

pub struct Config {
    pub name: String,
}

impl Config {
    pub fn new(name: &str) -> Self {
        Config { name: name.into() }
    }
}

pub fn run(config: Config) {
    println!("{}", config.name);
}
"#,
        )
        .unwrap();

        let chunks = chunk_file(&path, &default_opts());
        assert!(!chunks.is_empty(), "should produce chunks");

        let names: Vec<_> = chunks.iter().filter_map(|c| c.symbol_name.as_deref()).collect();
        assert!(names.contains(&"Config"), "missing Config: {names:?}");
        assert!(names.contains(&"run"), "missing run: {names:?}");
    }

    #[test]
    fn chunk_includes_import_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        fs::write(
            &path,
            r#"use std::collections::HashMap;
use std::io::Result;

pub fn process(map: HashMap<String, String>) -> Result<()> {
    Ok(())
}
"#,
        )
        .unwrap();

        let opts = ChunkOptions {
            include_context: true,
            ..default_opts()
        };
        let chunks = chunk_file(&path, &opts);

        let fn_chunk = chunks.iter().find(|c| c.symbol_name.as_deref() == Some("process"));
        assert!(fn_chunk.is_some(), "should have process chunk");
        let text = &fn_chunk.unwrap().text;
        assert!(
            text.contains("use std::collections::HashMap"),
            "chunk should include import context: {text}"
        );
    }

    #[test]
    fn chunk_without_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        fs::write(
            &path,
            r#"use std::io;

pub fn hello() {
    println!("hi");
}
"#,
        )
        .unwrap();

        let opts = ChunkOptions {
            include_context: false,
            ..default_opts()
        };
        let chunks = chunk_file(&path, &opts);

        let fn_chunk = chunks.iter().find(|c| c.symbol_name.as_deref() == Some("hello"));
        assert!(fn_chunk.is_some());
        assert!(
            !fn_chunk.unwrap().text.contains("use std::io"),
            "should not include imports when context disabled"
        );
    }

    #[test]
    fn chunk_metadata_correct() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        fs::write(
            &path,
            r#"pub fn alpha() {}

pub fn beta() {}
"#,
        )
        .unwrap();

        let chunks = chunk_file(&path, &default_opts());
        assert!(chunks.len() >= 2);

        for chunk in &chunks {
            assert_eq!(chunk.file_path, path);
        }

        let alpha = chunks.iter().find(|c| c.symbol_name.as_deref() == Some("alpha")).unwrap();
        assert_eq!(alpha.symbol_kind, Some(SymbolKind::Function));
        assert_eq!(alpha.start_line, 0);
    }

    #[test]
    fn chunk_python_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.py");
        fs::write(
            &path,
            r#"import os

class Server:
    def __init__(self, port):
        self.port = port

    def start(self):
        print(f"starting on {self.port}")

def main():
    s = Server(8080)
    s.start()
"#,
        )
        .unwrap();

        let chunks = chunk_file(&path, &default_opts());
        assert!(!chunks.is_empty(), "should produce chunks for python");

        let names: Vec<_> = chunks.iter().filter_map(|c| c.symbol_name.as_deref()).collect();
        assert!(
            names.contains(&"Server") || names.contains(&"main"),
            "should find python symbols: {names:?}"
        );
    }

    #[test]
    fn chunk_non_parseable_falls_back_to_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        fs::write(&path, "just some plain text\n\nwith paragraphs\n").unwrap();

        let chunks = chunk_file(&path, &default_opts());
        assert!(!chunks.is_empty());
        assert!(chunks[0].symbol_name.is_none());
    }

    #[test]
    fn chunk_large_function_gets_split() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.rs");

        let mut body = String::from("pub fn big_fn() {\n");
        for i in 0..200 {
            body.push_str(&format!("    let x{i} = {i};\n"));
        }
        body.push_str("}\n");

        fs::write(&path, &body).unwrap();

        let opts = ChunkOptions {
            target_size: 500,
            max_size: 800,
            include_context: false,
        };
        let chunks = chunk_file(&path, &opts);
        assert!(
            chunks.len() >= 2,
            "large function should be split: got {} chunks, total {} chars",
            chunks.len(),
            body.len()
        );

        for chunk in &chunks {
            assert_eq!(chunk.symbol_name.as_deref(), Some("big_fn"));
            assert_eq!(chunk.symbol_kind, Some(SymbolKind::Function));
        }
    }

    #[test]
    fn chunk_missing_file_returns_empty() {
        let chunks = chunk_file(Path::new("/nonexistent/file.rs"), &default_opts());
        assert!(chunks.is_empty());
    }

    #[test]
    fn is_only_preamble_detects_imports() {
        assert!(is_only_preamble("use std::io;\nuse std::fs;\n"));
        assert!(is_only_preamble("// comment\n"));
        assert!(is_only_preamble("  \n\n  "));
        assert!(!is_only_preamble("pub fn foo() {}"));
    }
}
