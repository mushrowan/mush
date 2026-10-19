use std::path::Path;

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
        let mut parser = new_parser(&Language::Rust.tree_sitter_language().unwrap());
        let source = b"fn main() { println!(\"hello\"); }";
        let tree = parser.parse(source, None).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
    }

    #[test]
    fn parse_python_source() {
        let mut parser = new_parser(&Language::Python.tree_sitter_language().unwrap());
        let source = b"def hello():\n    print('hello')";
        let tree = parser.parse(source, None).unwrap();
        assert_eq!(tree.root_node().kind(), "module");
    }

    #[test]
    fn parse_javascript_source() {
        let mut parser = new_parser(&Language::JavaScript.tree_sitter_language().unwrap());
        let source = b"function greet(name) { return `hello ${name}`; }";
        let tree = parser.parse(source, None).unwrap();
        assert_eq!(tree.root_node().kind(), "program");
    }

    #[test]
    fn parse_go_source() {
        let mut parser = new_parser(&Language::Go.tree_sitter_language().unwrap());
        let source = b"package main\nfunc main() {}";
        let tree = parser.parse(source, None).unwrap();
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
        let mut parser = new_parser(&Language::Rust.tree_sitter_language().unwrap());
        let source = b"fn a() {} fn b() {}";
        let first = parser.parse(source, None).unwrap();
        let second = parser.parse(source, None).unwrap();
        assert_eq!(first.root_node().to_sexp(), second.root_node().to_sexp());
    }

    #[test]
    fn reused_and_fresh_parsers_agree_on_generated_sources() {
        let mut cached = std::collections::HashMap::new();
        let mut state = 0x7a5c_324d_1e8b_960fu64;
        for &language in crate::language::ALL {
            let Some(grammar) = language.tree_sitter_language() else {
                continue;
            };
            for _ in 0..32 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let source: Vec<u8> = (0..state as usize % 256)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        state as u8
                    })
                    .collect();
                let reused = cached
                    .entry(language)
                    .or_insert_with(|| new_parser(&grammar))
                    .parse(&source, None)
                    .unwrap();
                let fresh = new_parser(&grammar).parse(&source, None).unwrap();
                assert_eq!(reused.root_node().to_sexp(), fresh.root_node().to_sexp());
                assert_eq!(
                    reused.root_node().has_error(),
                    fresh.root_node().has_error()
                );
            }
        }
    }
}
