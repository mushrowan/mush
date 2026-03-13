//! semantic symbol extraction from parsed syntax trees
//!
//! walks a tree-sitter parse tree and extracts top-level symbols
//! (functions, classes, structs, etc.) using per-language query patterns

use std::path::{Path, PathBuf};

use tree_sitter::StreamingIterator;

use crate::language::Language;
use crate::queries;

/// kind of symbol extracted from source code
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Interface,
    Trait,
    Impl,
    Module,
    Constant,
    Variable,
    Type,
    Import,
}

impl SymbolKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Function => "fn",
            Self::Method => "method",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Interface => "interface",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Module => "mod",
            Self::Constant => "const",
            Self::Variable => "var",
            Self::Type => "type",
            Self::Import => "import",
        }
    }
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// a symbol extracted from source code
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInfo {
    /// symbol name
    pub name: String,
    /// what kind of symbol
    pub kind: SymbolKind,
    /// source file path (set when extracted via `symbols_from_file`)
    pub file_path: Option<PathBuf>,
    /// start line (0-indexed)
    pub start_line: usize,
    /// end line (0-indexed)
    pub end_line: usize,
    /// start byte offset
    pub start_byte: usize,
    /// end byte offset
    pub end_byte: usize,
    /// the symbol's signature or first line (for display)
    pub signature: String,
}

/// extract symbols from source code for a given language
///
/// when `file_path` is provided, each symbol will carry the path
/// (needed when indexing whole repos)
pub fn extract_symbols(
    language: Language,
    source: &[u8],
    tree: &tree_sitter::Tree,
    file_path: Option<&Path>,
) -> Vec<SymbolInfo> {
    let query_source = queries::query_for(language);
    if query_source.is_empty() {
        return vec![];
    }

    let ts_lang = match language.tree_sitter_language() {
        Some(l) => l,
        None => return vec![],
    };

    let query = match tree_sitter::Query::new(&ts_lang, query_source) {
        Ok(q) => q,
        Err(e) => {
            tracing::warn!("failed to compile query for {language}: {e}");
            return vec![];
        }
    };

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source);

    let mut symbols = Vec::new();

    while let Some(m) = matches.next() {
        let mut name = None;
        let mut kind = None;
        let mut definition_node = None;

        for capture in m.captures {
            let capture_name = &query.capture_names()[capture.index as usize];
            match &**capture_name {
                "name" => {
                    name = capture.node.utf8_text(source).ok().map(|s| s.to_string());
                }
                _ if capture_name.starts_with("definition.") => {
                    kind = symbol_kind_from_capture(capture_name);
                    definition_node = Some(capture.node);
                }
                _ => {}
            }
        }

        if let (Some(name), Some(kind), Some(node)) = (name, kind, definition_node) {
            let signature = extract_signature(source, &node);
            symbols.push(SymbolInfo {
                name,
                kind,
                file_path: file_path.map(Path::to_path_buf),
                start_line: node.start_position().row,
                end_line: node.end_position().row,
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                signature,
            });
        }
    }

    symbols
}



fn symbol_kind_from_capture(capture_name: &str) -> Option<SymbolKind> {
    match capture_name {
        "definition.function" => Some(SymbolKind::Function),
        "definition.method" => Some(SymbolKind::Method),
        "definition.class" => Some(SymbolKind::Class),
        "definition.struct" => Some(SymbolKind::Struct),
        "definition.enum" => Some(SymbolKind::Enum),
        "definition.interface" => Some(SymbolKind::Interface),
        "definition.trait" => Some(SymbolKind::Trait),
        "definition.impl" => Some(SymbolKind::Impl),
        "definition.module" => Some(SymbolKind::Module),
        "definition.constant" => Some(SymbolKind::Constant),
        "definition.variable" => Some(SymbolKind::Variable),
        "definition.type" => Some(SymbolKind::Type),
        "definition.import" => Some(SymbolKind::Import),
        _ => None,
    }
}

/// extract the first line of a node as its signature
fn extract_signature(source: &[u8], node: &tree_sitter::Node) -> String {
    let start = node.start_byte();
    let end = node.end_byte().min(source.len());
    let text = String::from_utf8_lossy(&source[start..end]);

    let first_line = text.lines().next().unwrap_or("").trim();

    // truncate at a char boundary
    if first_line.len() > 200 {
        let boundary = first_line
            .char_indices()
            .take_while(|(i, _)| *i <= 197)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}...", &first_line[..boundary])
    } else {
        first_line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_and_extract(lang: Language, source: &str) -> Vec<SymbolInfo> {
        let pool = crate::ParserPool::new();
        let tree = pool.parse(lang, source.as_bytes()).unwrap();
        extract_symbols(lang, source.as_bytes(), &tree, None)
    }

    #[cfg(feature = "lang-rust")]
    #[test]
    fn rust_functions_and_structs() {
        let source = r#"
fn standalone() {}

pub fn public_one(x: i32) -> bool { true }

struct Point {
    x: f64,
    y: f64,
}

enum Direction {
    North,
    South,
}

trait Drawable {
    fn draw(&self);
}

impl Point {
    fn new() -> Self { Self { x: 0.0, y: 0.0 } }
}

const MAX: usize = 100;

type Result<T> = std::result::Result<T, Error>;

mod inner {}
"#;
        let symbols = parse_and_extract(Language::Rust, source);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"standalone"), "missing standalone: {names:?}");
        assert!(names.contains(&"public_one"), "missing public_one: {names:?}");
        assert!(names.contains(&"Point"), "missing Point: {names:?}");
        assert!(names.contains(&"Direction"), "missing Direction: {names:?}");
        assert!(names.contains(&"Drawable"), "missing Drawable: {names:?}");
        assert!(names.contains(&"new"), "missing new (impl method): {names:?}");
        assert!(names.contains(&"MAX"), "missing MAX: {names:?}");
        assert!(names.contains(&"inner"), "missing inner: {names:?}");

        // check kinds
        let point = symbols.iter().find(|s| s.name == "Point").unwrap();
        assert_eq!(point.kind, SymbolKind::Struct);

        let direction = symbols.iter().find(|s| s.name == "Direction").unwrap();
        assert_eq!(direction.kind, SymbolKind::Enum);

        let drawable = symbols.iter().find(|s| s.name == "Drawable").unwrap();
        assert_eq!(drawable.kind, SymbolKind::Trait);
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn python_functions_and_classes() {
        let source = r#"
def greet(name):
    print(f"hello {name}")

class Animal:
    def speak(self):
        pass

CONSTANT = 42
"#;
        let symbols = parse_and_extract(Language::Python, source);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"greet"), "missing greet: {names:?}");
        assert!(names.contains(&"Animal"), "missing Animal: {names:?}");
        assert!(names.contains(&"speak"), "missing speak: {names:?}");
    }

    #[cfg(feature = "lang-go")]
    #[test]
    fn go_functions_and_types() {
        let source = r#"
package main

func main() {}

func Add(a, b int) int { return a + b }

type Server struct {
    port int
}

func (s *Server) Start() {}

type Handler interface {
    Handle()
}
"#;
        let symbols = parse_and_extract(Language::Go, source);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"main"), "missing main: {names:?}");
        assert!(names.contains(&"Add"), "missing Add: {names:?}");
        assert!(names.contains(&"Server"), "missing Server: {names:?}");
        assert!(names.contains(&"Start"), "missing Start: {names:?}");
        assert!(names.contains(&"Handler"), "missing Handler: {names:?}");
    }

    #[cfg(feature = "lang-javascript")]
    #[test]
    fn javascript_functions_and_classes() {
        let source = r#"
function hello() {}

const greet = (name) => `hello ${name}`;

class Widget {
    render() {}
}
"#;
        let symbols = parse_and_extract(Language::JavaScript, source);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"hello"), "missing hello: {names:?}");
        assert!(names.contains(&"Widget"), "missing Widget: {names:?}");
        assert!(names.contains(&"render"), "missing render: {names:?}");
    }

    #[test]
    fn signature_extraction_caps_length() {
        let long = "x".repeat(300);
        let source = format!("fn {long}() {{}}");
        let symbols = parse_and_extract(Language::Rust, &source);

        if let Some(sym) = symbols.first() {
            assert!(sym.signature.len() <= 200 + 3); // 200 + "..."
        }
    }

    #[cfg(feature = "lang-typescript")]
    #[test]
    fn typescript_interfaces_and_types() {
        let source = r#"
interface Props {
    name: string;
}

type Result<T> = { ok: true; value: T } | { ok: false; error: string };

enum Direction {
    Up,
    Down,
}

function greet(props: Props): string {
    return `hello ${props.name}`;
}

class Service {
    start() {}
}
"#;
        let symbols = parse_and_extract(Language::TypeScript, source);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"Props"), "missing Props: {names:?}");
        assert!(names.contains(&"Result"), "missing Result: {names:?}");
        assert!(names.contains(&"Direction"), "missing Direction: {names:?}");
        assert!(names.contains(&"greet"), "missing greet: {names:?}");
        assert!(names.contains(&"Service"), "missing Service: {names:?}");
        assert!(names.contains(&"start"), "missing start: {names:?}");

        let props = symbols.iter().find(|s| s.name == "Props").unwrap();
        assert_eq!(props.kind, SymbolKind::Interface);
    }

    #[cfg(feature = "lang-c")]
    #[test]
    fn c_functions_and_structs() {
        let source = r#"
struct Point {
    int x;
    int y;
};

enum Color { RED, GREEN, BLUE };

int add(int a, int b) {
    return a + b;
}
"#;
        let symbols = parse_and_extract(Language::C, source);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"Point"), "missing Point: {names:?}");
        assert!(names.contains(&"Color"), "missing Color: {names:?}");
        assert!(names.contains(&"add"), "missing add: {names:?}");
    }

    #[cfg(feature = "lang-java")]
    #[test]
    fn java_classes_and_methods() {
        let source = r#"
public class App {
    public void run() {}

    public static void main(String[] args) {}
}

interface Runnable {
    void execute();
}

enum Status {
    OK,
    ERROR
}
"#;
        let symbols = parse_and_extract(Language::Java, source);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"App"), "missing App: {names:?}");
        assert!(names.contains(&"run"), "missing run: {names:?}");
        assert!(names.contains(&"main"), "missing main: {names:?}");
        assert!(names.contains(&"Runnable"), "missing Runnable: {names:?}");
        assert!(names.contains(&"Status"), "missing Status: {names:?}");
    }

    #[cfg(feature = "lang-bash")]
    #[test]
    fn bash_functions() {
        let source = r#"
greet() {
    echo "hello $1"
}

function deploy {
    rsync -avz . remote:/app
}
"#;
        let symbols = parse_and_extract(Language::Bash, source);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"greet"), "missing greet: {names:?}");
        assert!(names.contains(&"deploy"), "missing deploy: {names:?}");
    }

    #[cfg(feature = "lang-nix")]
    #[test]
    fn nix_bindings() {
        let source = r#"
{
  description = "my flake";
  inputs = { };
  outputs = { self, nixpkgs }: { };
}
"#;
        let symbols = parse_and_extract(Language::Nix, source);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(names.contains(&"description"), "missing description: {names:?}");
        assert!(names.contains(&"inputs"), "missing inputs: {names:?}");
        assert!(names.contains(&"outputs"), "missing outputs: {names:?}");
    }

    #[test]
    fn empty_source_produces_no_symbols() {
        let symbols = parse_and_extract(Language::Rust, "");
        assert!(symbols.is_empty());
    }

    #[test]
    fn symbols_have_line_info() {
        let source = "fn first() {}\n\nfn second() {}";
        let symbols = parse_and_extract(Language::Rust, source);

        let first = symbols.iter().find(|s| s.name == "first").unwrap();
        assert_eq!(first.start_line, 0);

        let second = symbols.iter().find(|s| s.name == "second").unwrap();
        assert_eq!(second.start_line, 2);
    }

    #[test]
    fn extract_without_path_has_none() {
        let symbols = parse_and_extract(Language::Rust, "fn hello() {}");
        assert!(symbols[0].file_path.is_none());
    }

    #[test]
    fn extract_with_path_carries_it() {
        let pool = crate::ParserPool::new();
        let source = b"fn hello() {}";
        let tree = pool.parse(Language::Rust, source).unwrap();
        let path = Path::new("src/lib.rs");
        let symbols = extract_symbols(Language::Rust, source, &tree, Some(path));

        assert_eq!(symbols[0].file_path.as_deref(), Some(path));
    }
}
