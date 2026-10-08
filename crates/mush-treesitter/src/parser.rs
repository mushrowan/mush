use std::collections::HashMap;
use std::path::Path;
use std::sync::nonpoison::Mutex;

use crate::language::Language;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("unsupported language: {0}")]
    UnsupportedLanguage(String),
    #[error("grammar not compiled for {0} (feature disabled)")]
    GrammarNotAvailable(Language),
    #[error("parse failed (tree-sitter returned None)")]
    ParseFailed,
    #[error("could not read file: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Default)]
pub struct ParserPool {
    parsers: Mutex<HashMap<Language, tree_sitter::Parser>>,
}

impl ParserPool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn parse(
        &self,
        language: Language,
        source: &[u8],
    ) -> Result<tree_sitter::Tree, ParseError> {
        let ts_lang = language
            .tree_sitter_language()
            .ok_or(ParseError::GrammarNotAvailable(language))?;

        let mut parsers = self.parsers.lock();
        let parser = parsers
            .entry(language)
            .or_insert_with(|| new_parser(&ts_lang));

        parser.parse(source, None).ok_or(ParseError::ParseFailed)
    }
}

impl std::fmt::Debug for ParserPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParserPool")
            .field("cached_parsers", &self.parsers.lock().len())
            .finish()
    }
}

pub(crate) fn detect_language(path: &Path) -> Result<Language, ParseError> {
    Language::detect(path)
        .ok_or_else(|| ParseError::UnsupportedLanguage(path.display().to_string()))
}

pub(crate) fn new_parser(language: &tree_sitter::Language) -> tree_sitter::Parser {
    let mut parser = tree_sitter::Parser::new();
    #[expect(
        clippy::expect_used,
        reason = "grammar was just validated by tree_sitter_language()"
    )]
    parser
        .set_language(language)
        .expect("grammar version mismatch");
    parser
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rust_source() {
        let pool = ParserPool::new();
        let source = b"fn main() { println!(\"hello\"); }";
        let tree = pool.parse(Language::Rust, source).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
    }

    #[test]
    fn parse_python_source() {
        let pool = ParserPool::new();
        let source = b"def hello():\n    print('hello')";
        let tree = pool.parse(Language::Python, source).unwrap();
        assert_eq!(tree.root_node().kind(), "module");
    }

    #[test]
    fn parse_javascript_source() {
        let pool = ParserPool::new();
        let source = b"function greet(name) { return `hello ${name}`; }";
        let tree = pool.parse(Language::JavaScript, source).unwrap();
        assert_eq!(tree.root_node().kind(), "program");
    }

    #[test]
    fn parse_go_source() {
        let pool = ParserPool::new();
        let source = b"package main\nfunc main() {}";
        let tree = pool.parse(Language::Go, source).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
    }

    #[test]
    fn parse_unknown_extension() {
        assert!(matches!(
            detect_language(Path::new("photo.png")),
            Err(ParseError::UnsupportedLanguage(_))
        ));
    }

    #[test]
    fn parser_reuse() {
        let pool = ParserPool::new();
        let source = b"fn a() {} fn b() {}";
        pool.parse(Language::Rust, source).unwrap();
        pool.parse(Language::Rust, source).unwrap();

        let count = pool.parsers.lock().len();
        assert_eq!(count, 1);
    }
}
