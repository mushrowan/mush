//! aider-style repository map using tree-sitter
//!
//! walks a repo, extracts definitions and references per file, builds
//! a file-level graph, ranks by PageRank, and generates a concise
//! text summary that fits within a token budget
//!
//! the map shows file paths with their most important symbol signatures,
//! giving the LLM a high-level understanding of the codebase

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::references::extract_references;
use crate::symbols::{self, SymbolInfo, SymbolKind};
use crate::{Language, ParserPool};

/// definitions and references extracted from a single file
struct FileData {
    definitions: Vec<SymbolInfo>,
    references: HashSet<String>,
}

/// a ranked file entry in the repo map
#[derive(Debug, Clone)]
pub struct RankedFile {
    pub path: PathBuf,
    pub rank: f64,
    pub symbols: Vec<SymbolInfo>,
}

/// the generated repo map
#[derive(Debug)]
pub struct RepoMap {
    /// files ranked by importance (highest first)
    pub files: Vec<RankedFile>,
}

impl RepoMap {
    /// format the repo map as a text string
    ///
    /// each file shows its path and ranked symbol signatures,
    /// trimmed to fit within `max_chars` (rough token estimate: 1 token ~ 4 chars)
    pub fn format(&self, max_chars: usize) -> String {
        let mut output = String::new();
        let mut budget = max_chars;

        for entry in &self.files {
            if budget == 0 {
                break;
            }

            let mut file_block = format!("{}:\n", entry.path.display());

            for sym in &entry.symbols {
                let line = format!(
                    "  {:>4}│{} {}\n",
                    sym.start_line + 1,
                    sym.kind.label(),
                    sym.signature,
                );
                file_block.push_str(&line);
            }
            file_block.push('\n');

            if file_block.len() > budget {
                // try to fit at least the file header
                let header = format!("{}:\n  ⋮...\n\n", entry.path.display());
                if header.len() <= budget {
                    output.push_str(&header);
                }
                break;
            }

            budget -= file_block.len();
            output.push_str(&file_block);
        }

        output
    }

    /// format the repo map targeting a token budget
    ///
    /// uses a rough estimate of 4 chars per token
    pub fn format_for_tokens(&self, max_tokens: usize) -> String {
        self.format(max_tokens * 4)
    }
}

/// build a repo map for a directory
///
/// walks all parseable files, extracts definitions and references,
/// builds a file-level reference graph, ranks with PageRank, and
/// returns the ranked map
pub fn build_repo_map(root: &Path) -> RepoMap {
    let incr = IncrementalRepoMap::new(root);
    incr.current
}

/// rank symbols within a file by reference count and kind
fn rank_symbols(
    definitions: &[SymbolInfo],
    def_index: &HashMap<&str, Vec<&PathBuf>>,
) -> Vec<SymbolInfo> {
    let mut scored: Vec<(usize, &SymbolInfo)> = definitions
        .iter()
        .map(|sym| {
            // score by how many files define this name (proxy for importance)
            // higher-level constructs (structs, traits, classes) get a bonus
            let kind_bonus = match sym.kind {
                SymbolKind::Class | SymbolKind::Struct | SymbolKind::Trait
                | SymbolKind::Interface | SymbolKind::Enum => 2,
                SymbolKind::Function | SymbolKind::Method => 1,
                SymbolKind::Module => 3,
                _ => 0,
            };
            let ref_count = def_index
                .get(sym.name.as_str())
                .map(|v| v.len())
                .unwrap_or(0);
            (ref_count + kind_bonus, sym)
        })
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.start_line.cmp(&b.1.start_line)));
    scored.into_iter().map(|(_, sym)| sym.clone()).collect()
}

/// simple PageRank implementation
///
/// iterative computation without external graph library deps
fn pagerank(edges: &[(usize, usize)], n: usize, damping: f64, iterations: usize) -> Vec<f64> {
    if n == 0 {
        return vec![];
    }

    let mut ranks = vec![1.0 / n as f64; n];
    let mut out_degree = vec![0usize; n];
    for &(from, _) in edges {
        out_degree[from] += 1;
    }

    for _ in 0..iterations {
        let mut new_ranks = vec![(1.0 - damping) / n as f64; n];
        for &(from, to) in edges {
            if out_degree[from] > 0 {
                new_ranks[to] += damping * ranks[from] / out_degree[from] as f64;
            }
        }
        ranks = new_ranks;
    }

    ranks
}

/// walk a directory tree, respecting .gitignore, returning parseable files
fn discover_files(root: &Path) -> Vec<PathBuf> {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true) // skip hidden files
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut files = Vec::new();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();
        // only include files we can parse
        if Language::detect(&path).is_some() {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// parse a single file and extract its definitions + references
fn parse_file_data(pool: &ParserPool, path: &Path) -> Result<FileData, crate::ParseError> {
    let language = Language::detect(path)
        .ok_or_else(|| crate::ParseError::UnsupportedLanguage(path.display().to_string()))?;
    let source = std::fs::read(path)?;
    let tree = pool.parse(language, &source)?;

    Ok(FileData {
        definitions: symbols::extract_symbols(language, &source, &tree, Some(path)),
        references: extract_references(&source, &tree),
    })
}

/// persistent repo map that can be updated incrementally
///
/// holds parsed file data and a parser pool so only changed files
/// need re-parsing. PageRank is re-run on the full graph after each
/// update (it's fast enough for typical repo sizes).
pub struct IncrementalRepoMap {
    root: PathBuf,
    pool: ParserPool,
    file_data: HashMap<PathBuf, FileData>,
    current: RepoMap,
}

impl IncrementalRepoMap {
    /// build initial map by walking and parsing all files
    pub fn new(root: &Path) -> Self {
        let pool = ParserPool::new();
        let files = discover_files(root);

        let mut file_data: HashMap<PathBuf, FileData> = HashMap::new();
        for path in &files {
            match parse_file_data(&pool, path) {
                Ok(data) => {
                    file_data.insert(path.clone(), data);
                }
                Err(e) => {
                    tracing::debug!("skipping {}: {e}", path.display());
                }
            }
        }

        let current = build_from_data(root, &file_data);
        Self {
            root: root.to_path_buf(),
            pool,
            file_data,
            current,
        }
    }

    /// get the current repo map
    pub fn map(&self) -> &RepoMap {
        &self.current
    }

    /// update after files changed or were removed
    ///
    /// re-parses changed files, removes deleted ones, and rebuilds
    /// the graph. returns true if the map actually changed.
    pub fn update(&mut self, changed: &[PathBuf], removed: &[PathBuf]) -> bool {
        if changed.is_empty() && removed.is_empty() {
            return false;
        }

        let mut any_change = false;

        for path in removed {
            if self.file_data.remove(path).is_some() {
                self.pool.invalidate(path);
                any_change = true;
            }
        }

        for path in changed {
            // invalidate cached parse tree so we get a fresh parse
            self.pool.invalidate(path);

            if !path.exists() {
                // file was deleted between event and processing
                if self.file_data.remove(path).is_some() {
                    any_change = true;
                }
                continue;
            }

            match parse_file_data(&self.pool, path) {
                Ok(data) => {
                    self.file_data.insert(path.clone(), data);
                    any_change = true;
                }
                Err(e) => {
                    tracing::debug!("failed to re-parse {}: {e}", path.display());
                    // remove stale data if we can't parse anymore
                    if self.file_data.remove(path).is_some() {
                        any_change = true;
                    }
                }
            }
        }

        if any_change {
            self.current = build_from_data(&self.root, &self.file_data);
        }

        any_change
    }

    /// add any newly discovered files (e.g. after a create event)
    pub fn add_new_files(&mut self) -> bool {
        let all_files = discover_files(&self.root);
        let mut new_files = Vec::new();
        for path in all_files {
            if !self.file_data.contains_key(&path) {
                new_files.push(path);
            }
        }
        if new_files.is_empty() {
            return false;
        }
        self.update(&new_files, &[])
    }
}

/// shared core: build a RepoMap from pre-parsed file data
fn build_from_data(root: &Path, file_data: &HashMap<PathBuf, FileData>) -> RepoMap {
    if file_data.is_empty() {
        return RepoMap { files: vec![] };
    }

    // build global definition index
    let mut def_index: HashMap<&str, Vec<&PathBuf>> = HashMap::new();
    for (path, data) in file_data {
        for sym in &data.definitions {
            def_index.entry(sym.name.as_str()).or_default().push(path);
        }
    }

    // build file-level edges
    let file_indices: HashMap<&PathBuf, usize> = file_data
        .keys()
        .enumerate()
        .map(|(i, p)| (p, i))
        .collect();
    let n = file_indices.len();

    let mut edges: Vec<(usize, usize)> = Vec::new();
    for (path, data) in file_data {
        let from = file_indices[path];
        for ref_name in &data.references {
            if let Some(def_files) = def_index.get(ref_name.as_str()) {
                for def_path in def_files {
                    if *def_path != path {
                        let to = file_indices[def_path];
                        edges.push((from, to));
                    }
                }
            }
        }
    }

    edges.sort_unstable();
    edges.dedup();

    let ranks = pagerank(&edges, n, 0.85, 20);

    let mut ranked: Vec<RankedFile> = file_data
        .iter()
        .map(|(path, data)| {
            let idx = file_indices[path];
            let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
            RankedFile {
                path: rel,
                rank: ranks[idx],
                symbols: rank_symbols(&data.definitions, &def_index),
            }
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });

    ranked.retain(|f| !f.symbols.is_empty());

    RepoMap { files: ranked }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_test_repo(dir: &Path) {
        // create a small multi-file rust project
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();

        fs::write(
            src.join("lib.rs"),
            r#"
mod config;
mod server;

pub use config::Config;
pub use server::Server;

pub fn run(config: Config) {
    let server = Server::new(config);
    server.start();
}
"#,
        )
        .unwrap();

        fs::write(
            src.join("config.rs"),
            r#"
pub struct Config {
    pub port: u16,
    pub host: String,
}

impl Config {
    pub fn default_config() -> Self {
        Config {
            port: 8080,
            host: "localhost".into(),
        }
    }
}
"#,
        )
        .unwrap();

        fs::write(
            src.join("server.rs"),
            r#"
use crate::config::Config;

pub struct Server {
    config: Config,
}

impl Server {
    pub fn new(config: Config) -> Self {
        Server { config }
    }

    pub fn start(&self) {
        println!("starting on {}:{}", self.config.host, self.config.port);
    }
}
"#,
        )
        .unwrap();
    }

    #[test]
    fn builds_map_from_test_repo() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let map = build_repo_map(dir.path());

        assert!(!map.files.is_empty(), "map should have files");

        // all three files should be present
        let paths: Vec<_> = map.files.iter().map(|f| f.path.display().to_string()).collect();
        assert!(
            paths.iter().any(|p| p.contains("lib.rs")),
            "missing lib.rs: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("config.rs")),
            "missing config.rs: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("server.rs")),
            "missing server.rs: {paths:?}"
        );
    }

    #[test]
    fn config_ranks_high_because_referenced() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let map = build_repo_map(dir.path());

        // Config is used by both lib.rs and server.rs, so config.rs should rank well
        let config_entry = map
            .files
            .iter()
            .find(|f| f.path.display().to_string().contains("config.rs"))
            .expect("config.rs should be in the map");

        // it should have a non-trivial rank
        assert!(config_entry.rank > 0.0, "config.rs should have positive rank");

        // Config struct should be among its symbols
        let sym_names: Vec<_> = config_entry.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            sym_names.contains(&"Config"),
            "Config should be a symbol: {sym_names:?}"
        );
    }

    #[test]
    fn format_produces_readable_output() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let map = build_repo_map(dir.path());
        let text = map.format(10_000);

        assert!(!text.is_empty());
        assert!(text.contains("config.rs"), "output should contain config.rs");
        assert!(text.contains("Config"), "output should contain Config symbol");
    }

    #[test]
    fn format_respects_budget() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let map = build_repo_map(dir.path());
        let text = map.format(100); // very small budget

        assert!(text.len() <= 200, "output should be near budget: {}", text.len());
    }

    #[test]
    fn empty_dir_produces_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let map = build_repo_map(dir.path());
        assert!(map.files.is_empty());
    }

    #[test]
    fn pagerank_basic() {
        // simple 3-node chain: 0→1→2
        let edges = vec![(0, 1), (1, 2)];
        let ranks = pagerank(&edges, 3, 0.85, 20);

        // node 2 (sink) should have highest rank
        assert!(ranks[2] > ranks[1], "sink should rank highest");
        assert!(ranks[1] > ranks[0], "middle should rank higher than source");
    }

    #[test]
    fn pagerank_empty() {
        let ranks = pagerank(&[], 0, 0.85, 20);
        assert!(ranks.is_empty());
    }

    #[test]
    fn pagerank_no_edges() {
        let ranks = pagerank(&[], 3, 0.85, 20);
        // all nodes should have equal rank
        assert!((ranks[0] - ranks[1]).abs() < 1e-10);
        assert!((ranks[1] - ranks[2]).abs() < 1e-10);
    }

    // -- incremental repo map tests --

    #[test]
    fn incremental_matches_full_build() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let full = build_repo_map(dir.path());
        let incr = IncrementalRepoMap::new(dir.path());

        // same number of files
        assert_eq!(
            full.files.len(),
            incr.map().files.len(),
            "incremental should match full build"
        );

        // same file paths (order may differ, so compare sets)
        let full_paths: HashSet<_> = full.files.iter().map(|f| f.path.clone()).collect();
        let incr_paths: HashSet<_> = incr.map().files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(full_paths, incr_paths);
    }

    #[test]
    fn incremental_update_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let mut incr = IncrementalRepoMap::new(dir.path());
        let before = incr.map().format(10_000);

        // add a new function to config.rs
        let config_path = dir.path().join("src/config.rs");
        fs::write(
            &config_path,
            r#"
pub struct Config {
    pub port: u16,
    pub host: String,
}

impl Config {
    pub fn default_config() -> Self {
        Config { port: 8080, host: "localhost".into() }
    }

    pub fn from_env() -> Self {
        Config { port: 3000, host: "0.0.0.0".into() }
    }
}
"#,
        )
        .unwrap();

        let changed = incr.update(&[config_path], &[]);
        assert!(changed, "update should report change");

        let after = incr.map().format(10_000);
        assert!(
            after.contains("from_env"),
            "updated map should contain new function: {after}"
        );
        assert_ne!(before, after, "map should have changed");
    }

    #[test]
    fn incremental_update_removed_file() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let mut incr = IncrementalRepoMap::new(dir.path());
        assert!(
            incr.map().files.iter().any(|f| f.path.display().to_string().contains("server.rs")),
            "server.rs should be in initial map"
        );

        let server_path = dir.path().join("src/server.rs");
        fs::remove_file(&server_path).unwrap();

        let changed = incr.update(&[], &[server_path]);
        assert!(changed, "removal should report change");

        assert!(
            !incr.map().files.iter().any(|f| f.path.display().to_string().contains("server.rs")),
            "server.rs should be gone after removal"
        );
    }

    #[test]
    fn incremental_add_new_file() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let mut incr = IncrementalRepoMap::new(dir.path());
        let initial_count = incr.map().files.len();

        // add a new file
        fs::write(
            dir.path().join("src/utils.rs"),
            r#"
pub fn helper() -> String {
    "hello".to_string()
}
"#,
        )
        .unwrap();

        let changed = incr.add_new_files();
        assert!(changed, "should detect new file");
        assert!(
            incr.map().files.len() > initial_count,
            "should have more files after adding"
        );
        assert!(
            incr.map().files.iter().any(|f| f.path.display().to_string().contains("utils.rs")),
            "utils.rs should be in map"
        );
    }

    #[test]
    fn incremental_no_change_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let mut incr = IncrementalRepoMap::new(dir.path());

        assert!(!incr.update(&[], &[]));
        assert!(!incr.add_new_files());
    }
}
