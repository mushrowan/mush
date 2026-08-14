use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::language::Language;
use crate::symbols::{self, SymbolInfo};

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

struct CachedTree {
    language: Language,
    source: Vec<u8>,
    tree: tree_sitter::Tree,
    mtime: SystemTime,
}

struct CachedTreeRef {
    language: Language,
    source: Vec<u8>,
    tree: tree_sitter::Tree,
}

pub struct ParserPool {
    parsers: Mutex<HashMap<Language, tree_sitter::Parser>>,
    tree_cache: Mutex<HashMap<PathBuf, CachedTree>>,
}

impl ParserPool {
    pub fn new() -> Self {
        Self {
            parsers: Mutex::new(HashMap::new()),
            tree_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn parse(
        &self,
        language: Language,
        source: &[u8],
    ) -> Result<tree_sitter::Tree, ParseError> {
        let ts_lang = language
            .tree_sitter_language()
            .ok_or(ParseError::GrammarNotAvailable(language))?;

        let mut parsers = self.parsers.lock().unwrap_or_else(|e| e.into_inner());
        let parser = parsers.entry(language).or_insert_with(|| {
            let mut p = tree_sitter::Parser::new();
            #[expect(
                clippy::expect_used,
                reason = "grammar was just validated by tree_sitter_language()"
            )]
            p.set_language(&ts_lang).expect("grammar version mismatch");
            p
        });

        parser.parse(source, None).ok_or(ParseError::ParseFailed)
    }

    pub fn parse_file(&self, path: &Path) -> Result<(Language, tree_sitter::Tree), ParseError> {
        let cached = self.parse_file_cached(path)?;
        Ok((cached.language, cached.tree.clone()))
    }

    pub fn symbols_from_file(
        &self,
        path: &Path,
    ) -> Result<(Language, Vec<SymbolInfo>), ParseError> {
        let cached = self.parse_file_cached(path)?;
        let syms =
            symbols::extract_symbols(cached.language, &cached.source, &cached.tree, Some(path));
        Ok((cached.language, syms))
    }

    fn parse_file_cached(&self, path: &Path) -> Result<CachedTreeRef, ParseError> {
        let language = Language::detect(path)
            .ok_or_else(|| ParseError::UnsupportedLanguage(path.display().to_string()))?;
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mtime = std::fs::metadata(path)?.modified().ok();

        if let Some(mtime) = mtime
            && let Some(entry) = self
                .tree_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&canon)
            && entry.mtime == mtime
            && entry.language == language
        {
            return Ok(CachedTreeRef {
                language,
                source: entry.source.clone(),
                tree: entry.tree.clone(),
            });
        }

        let source = std::fs::read(path)?;
        let tree = self.parse(language, &source)?;

        if let Some(mtime) = mtime {
            let mut cache = self.tree_cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.insert(
                canon,
                CachedTree {
                    language,
                    source: source.clone(),
                    tree: tree.clone(),
                    mtime,
                },
            );
        }

        Ok(CachedTreeRef {
            language,
            source,
            tree,
        })
    }

    pub fn invalidate(&self, path: &Path) {
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut cache = self.tree_cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.remove(&canon);
    }

    pub fn invalidate_all(&self) {
        let mut cache = self.tree_cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.clear();
    }

    pub fn cached_file_count(&self) -> usize {
        self.tree_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

impl Default for ParserPool {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ParserPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parser_count = self.parsers.lock().map(|p| p.len()).unwrap_or(0);
        let cache_count = self.tree_cache.lock().map(|c| c.len()).unwrap_or(0);
        f.debug_struct("ParserPool")
            .field("cached_parsers", &parser_count)
            .field("cached_files", &cache_count)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

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
        let pool = ParserPool::new();
        let result = pool.parse_file(Path::new("photo.png"));
        assert!(result.is_err());
    }

    #[test]
    fn parser_reuse() {
        let pool = ParserPool::new();
        let source = b"fn a() {} fn b() {}";
        pool.parse(Language::Rust, source).unwrap();
        pool.parse(Language::Rust, source).unwrap();

        let count = pool.parsers.lock().unwrap().len();
        assert_eq!(count, 1);
    }

    #[test]
    fn parse_file_populates_cache() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn cached() {}").unwrap();

        let pool = ParserPool::new();
        assert_eq!(pool.cached_file_count(), 0);

        pool.parse_file(&file).unwrap();
        assert_eq!(pool.cached_file_count(), 1);

        pool.parse_file(&file).unwrap();
        assert_eq!(pool.cached_file_count(), 1);
    }

    #[test]
    fn cache_invalidated_on_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("evolving.rs");
        std::fs::write(&file, "fn v1() {}").unwrap();

        let pool = ParserPool::new();
        let (_, syms1) = pool.symbols_from_file(&file).unwrap();
        assert_eq!(syms1[0].name, "v1");

        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&file)
            .unwrap();
        f.write_all(b"fn v2() {}").unwrap();
        f.flush().unwrap();
        drop(f);

        let future = SystemTime::now() + std::time::Duration::from_secs(10);
        filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(future)).unwrap();

        let (_, syms2) = pool.symbols_from_file(&file).unwrap();
        assert_eq!(syms2[0].name, "v2");
    }

    #[test]
    fn invalidate_removes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("removeme.rs");
        std::fs::write(&file, "fn bye() {}").unwrap();

        let pool = ParserPool::new();
        pool.parse_file(&file).unwrap();
        assert_eq!(pool.cached_file_count(), 1);

        pool.invalidate(&file);
        assert_eq!(pool.cached_file_count(), 0);
    }

    #[test]
    fn invalidate_all_clears_cache() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.py");
        std::fs::write(&a, "fn a() {}").unwrap();
        std::fs::write(&b, "def b(): pass").unwrap();

        let pool = ParserPool::new();
        pool.parse_file(&a).unwrap();
        pool.parse_file(&b).unwrap();
        assert_eq!(pool.cached_file_count(), 2);

        pool.invalidate_all();
        assert_eq!(pool.cached_file_count(), 0);
    }

    #[test]
    fn symbols_from_file_uses_cache() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "fn hello() {}\nfn world() {}").unwrap();

        let pool = ParserPool::new();

        let (lang, syms) = pool.symbols_from_file(&file).unwrap();
        assert_eq!(lang, Language::Rust);
        assert_eq!(syms.len(), 2);
        assert_eq!(pool.cached_file_count(), 1);

        let (_, syms2) = pool.symbols_from_file(&file).unwrap();
        assert_eq!(syms2.len(), 2);
        assert_eq!(pool.cached_file_count(), 1);
    }
}
