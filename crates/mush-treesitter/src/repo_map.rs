use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

use crate::Language;
use crate::parser::{detect_language, new_parser};
use crate::references::extract_references;
use crate::symbols::{self, SymbolInfo, SymbolKind};

struct FileData {
    definitions: Vec<SymbolInfo>,
    references: HashSet<String>,
}

impl FileData {
    fn extract(language: Language, source: &[u8], tree: &tree_sitter::Tree, path: &Path) -> Self {
        Self {
            definitions: symbols::extract_symbols(language, source, tree, Some(path)),
            references: extract_references(source, tree),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RankedFile {
    pub path: PathBuf,
    pub rank: f64,
    pub symbols: Vec<SymbolInfo>,
}

#[derive(Debug, Clone, Default)]
pub struct RepoMap {
    pub files: Vec<RankedFile>,
}

#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    pub max_chars: usize,
    pub max_files: usize,
    pub summary_only: bool,
}

impl RenderOptions {
    #[must_use]
    pub fn for_chars(max_chars: usize) -> Self {
        Self {
            max_chars,
            max_files: usize::MAX,
            summary_only: false,
        }
    }

    #[must_use]
    pub fn for_tokens(max_tokens: usize) -> Self {
        Self::for_chars(max_tokens * 4)
    }
}

impl RepoMap {
    #[must_use]
    pub fn filter_path(&self, prefix: &str) -> Self {
        let prefix = prefix.trim_end_matches('/');
        let files = self
            .files
            .iter()
            .filter(|f| {
                let path = f.path.to_string_lossy();
                path.starts_with(prefix)
                    && path.as_bytes().get(prefix.len()).is_none_or(|&b| b == b'/')
            })
            .cloned()
            .collect();
        Self { files }
    }

    #[must_use]
    pub fn render(&self, opts: RenderOptions) -> String {
        let mut output = String::new();
        let mut budget = opts.max_chars;
        let mut emitted = 0usize;

        for entry in &self.files {
            if emitted >= opts.max_files {
                break;
            }
            if budget == 0 {
                break;
            }

            let block = if opts.summary_only {
                format!(
                    "{}: ({} symbols, rank={:.4})\n\n",
                    entry.path.display(),
                    entry.symbols.len(),
                    entry.rank,
                )
            } else {
                let mut s = format!("{}:\n", entry.path.display());
                for sym in &entry.symbols {
                    let _ = writeln!(
                        s,
                        "  {:>4}│{} {}",
                        sym.start_line + 1,
                        sym.kind.label(),
                        sym.signature,
                    );
                }
                s.push('\n');
                s
            };

            if block.len() > budget {
                if !opts.summary_only {
                    let header = format!("{}:\n  ⋮...\n\n", entry.path.display());
                    if header.len() <= budget {
                        output.push_str(&header);
                        emitted += 1;
                    }
                }
                break;
            }

            budget -= block.len();
            output.push_str(&block);
            emitted += 1;
        }

        let dropped = self.files.len().saturating_sub(emitted);
        if dropped > 0 {
            let _ = writeln!(
                output,
                "[…{dropped} more files omitted from this view ({} total). \
                 narrow with `path` to focus on a subtree, raise `limit`, \
                 or pass `summary_only=true` to fit more entries.]",
                self.files.len()
            );
        }

        output
    }

    #[must_use]
    pub fn format(&self, max_chars: usize) -> String {
        self.render(RenderOptions::for_chars(max_chars))
    }
}

pub fn build_repo_map(root: &Path) -> RepoMap {
    let incr = IncrementalRepoMap::new(root);
    incr.current
}

fn rank_symbols(
    definitions: &[SymbolInfo],
    def_index: &FxHashMap<&str, Vec<usize>>,
) -> Vec<SymbolInfo> {
    let mut scored: Vec<(usize, &SymbolInfo)> = definitions
        .iter()
        .map(|sym| {
            let kind_bonus = match sym.kind {
                SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Trait
                | SymbolKind::Interface
                | SymbolKind::Enum => 2,
                SymbolKind::Function | SymbolKind::Method => 1,
                SymbolKind::Module => 3,
                SymbolKind::Constant
                | SymbolKind::Variable
                | SymbolKind::Type
                | SymbolKind::Impl
                | SymbolKind::Import => 0,
            };
            let ref_count = def_index.get(sym.name.as_str()).map_or(0, Vec::len);
            (ref_count + kind_bonus, sym)
        })
        .collect();

    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.start_line.cmp(&b.1.start_line))
    });
    scored.into_iter().map(|(_, sym)| sym.clone()).collect()
}

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

pub fn is_inside_repo(path: &Path) -> bool {
    let mut current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    loop {
        if current.join(".git").exists() || current.join(".jj").exists() {
            return true;
        }
        if !current.pop() {
            return false;
        }
    }
}

#[derive(Debug, Clone)]
struct DiscoverOptions {
    max_files: usize,
}

impl Default for DiscoverOptions {
    fn default() -> Self {
        Self {
            max_files: default_max_files(),
        }
    }
}

fn default_max_files() -> usize {
    const HARD_MAX: usize = 50_000;

    #[cfg(target_os = "linux")]
    {
        let limit = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(HARD_MAX);
        (limit / 2).min(HARD_MAX)
    }

    #[cfg(not(target_os = "linux"))]
    {
        HARD_MAX
    }
}

fn discover_files(root: &Path) -> Vec<PathBuf> {
    discover_files_with_options(root, &DiscoverOptions::default())
}

fn discover_files_with_options(root: &Path, opts: &DiscoverOptions) -> Vec<PathBuf> {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let max = if opts.max_files == 0 {
        usize::MAX
    } else {
        opts.max_files
    };

    let mut files = Vec::new();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.into_path();
        if Language::detect(&path).is_some_and(|l| l.is_code()) {
            files.push(path);
            if files.len() >= max {
                tracing::warn!(
                    max_files = max,
                    root = %root.display(),
                    "file discovery hit cap, results truncated"
                );
                break;
            }
        }
    }
    files.sort();
    files
}

fn parse_file_data(path: &Path) -> Result<FileData, crate::ParseError> {
    thread_local! {
        static PARSERS: RefCell<HashMap<Language, tree_sitter::Parser>> =
            RefCell::new(HashMap::new());
    }

    let language = detect_language(path)?;
    let ts_lang = language
        .tree_sitter_language()
        .ok_or(crate::ParseError::GrammarNotAvailable(language))?;
    let source = std::fs::read(path)?;

    let tree = PARSERS.with_borrow_mut(|parsers| {
        parsers
            .entry(language)
            .or_insert_with(|| new_parser(&ts_lang))
            .parse(&source, None)
            .ok_or(crate::ParseError::ParseFailed)
    })?;

    Ok(FileData::extract(language, &source, &tree, path))
}

fn parse_files_parallel(files: &[PathBuf]) -> FxHashMap<PathBuf, FileData> {
    use rayon::prelude::*;

    files
        .par_iter()
        .filter_map(|path| match parse_file_data(path) {
            Ok(data) => Some((path.clone(), data)),
            Err(e) => {
                tracing::debug!("skipping {}: {e}", path.display());
                None
            }
        })
        .collect()
}

pub struct IncrementalRepoMap {
    root: PathBuf,
    file_data: FxHashMap<PathBuf, FileData>,
    current: RepoMap,
    last_edges_fp: u64,
    last_ranks: FxHashMap<PathBuf, f64>,
    pagerank_runs: u64,
}

impl IncrementalRepoMap {
    #[tracing::instrument(name = "repo_map_build", skip_all)]
    pub fn new(root: &Path) -> Self {
        let files = discover_files(root);
        let file_data = parse_files_parallel(&files);

        let BuildOutcome {
            map,
            edges_fp,
            ranks_by_path,
        } = build_from_data(root, &file_data, None);
        Self {
            root: root.to_path_buf(),
            file_data,
            current: map,
            last_edges_fp: edges_fp,
            last_ranks: ranks_by_path,
            pagerank_runs: 1,
        }
    }

    pub fn map(&self) -> &RepoMap {
        &self.current
    }

    pub fn pagerank_runs(&self) -> u64 {
        self.pagerank_runs
    }

    pub fn update(&mut self, changed: &[PathBuf], removed: &[PathBuf]) -> bool {
        if changed.is_empty() && removed.is_empty() {
            return false;
        }

        let mut any_change = false;

        for path in removed {
            if self.file_data.remove(path).is_some() {
                any_change = true;
            }
        }

        for path in changed {
            if !path.exists() {
                if self.file_data.remove(path).is_some() {
                    any_change = true;
                }
                continue;
            }

            match parse_file_data(path) {
                Ok(data) => {
                    self.file_data.insert(path.clone(), data);
                    any_change = true;
                }
                Err(e) => {
                    tracing::debug!("failed to re-parse {}: {e}", path.display());
                    if self.file_data.remove(path).is_some() {
                        any_change = true;
                    }
                }
            }
        }

        if any_change {
            let BuildOutcome {
                map,
                edges_fp,
                ranks_by_path,
            } = build_from_data(
                &self.root,
                &self.file_data,
                Some((self.last_edges_fp, &self.last_ranks)),
            );
            if edges_fp != self.last_edges_fp {
                self.pagerank_runs += 1;
            }
            self.current = map;
            self.last_edges_fp = edges_fp;
            self.last_ranks = ranks_by_path;
        }

        any_change
    }

    pub fn add_new_files(&mut self) -> bool {
        let new_files: Vec<PathBuf> = discover_files(&self.root)
            .into_iter()
            .filter(|path| !self.file_data.contains_key(path))
            .collect();
        if new_files.is_empty() {
            return false;
        }
        self.update(&new_files, &[])
    }
}

struct BuildOutcome {
    map: RepoMap,
    edges_fp: u64,
    ranks_by_path: FxHashMap<PathBuf, f64>,
}

fn build_from_data(
    root: &Path,
    file_data: &FxHashMap<PathBuf, FileData>,
    cached: Option<(u64, &FxHashMap<PathBuf, f64>)>,
) -> BuildOutcome {
    if file_data.is_empty() {
        return BuildOutcome {
            map: RepoMap { files: vec![] },
            edges_fp: 0,
            ranks_by_path: FxHashMap::default(),
        };
    }

    let mut ordered: Vec<(&PathBuf, &FileData)> = file_data.iter().collect();
    ordered.sort_by_key(|(path, _)| *path);
    let ordered_paths: Vec<&PathBuf> = ordered.iter().map(|(path, _)| *path).collect();
    let n = ordered.len();

    let mut def_index: FxHashMap<&str, Vec<usize>> = FxHashMap::default();
    for (index, (_, data)) in ordered.iter().enumerate() {
        for sym in &data.definitions {
            def_index.entry(sym.name.as_str()).or_default().push(index);
        }
    }

    let mut edges: Vec<(usize, usize)> = Vec::new();
    let mut linked_from = vec![usize::MAX; n];
    let mut targets = Vec::new();
    for (from, (_, data)) in ordered.iter().enumerate() {
        for ref_name in &data.references {
            let Some(def_files) = def_index.get(ref_name.as_str()) else {
                continue;
            };
            for &to in def_files {
                if to != from && linked_from[to] != from {
                    linked_from[to] = from;
                    targets.push(to);
                }
            }
        }
        targets.sort_unstable();
        edges.extend(targets.drain(..).map(|to| (from, to)));
    }

    let edges_fp = edges_fingerprint(&ordered_paths, &edges);

    let reused_ranks = cached
        .filter(|(prev_fp, prev_ranks)| *prev_fp == edges_fp && prev_ranks.len() == n)
        .and_then(|(_, prev_ranks)| {
            ordered_paths
                .iter()
                .map(|p| prev_ranks.get(*p).copied())
                .collect::<Option<Vec<f64>>>()
        });
    let ranks = reused_ranks.unwrap_or_else(|| pagerank(&edges, n, 0.85, 20));

    let ranks_by_path: FxHashMap<PathBuf, f64> = ordered_paths
        .iter()
        .zip(&ranks)
        .map(|(path, rank)| ((*path).clone(), *rank))
        .collect();

    let mut ranked: Vec<RankedFile> = ordered
        .iter()
        .zip(&ranks)
        .map(|((path, data), rank)| RankedFile {
            path: path.strip_prefix(root).unwrap_or(path).to_path_buf(),
            rank: *rank,
            symbols: rank_symbols(&data.definitions, &def_index),
        })
        .collect();

    ranked.sort_by(|a, b| b.rank.total_cmp(&a.rank).then_with(|| a.path.cmp(&b.path)));

    ranked.retain(|f| !f.symbols.is_empty());

    BuildOutcome {
        map: RepoMap { files: ranked },
        edges_fp,
        ranks_by_path,
    }
}

fn edges_fingerprint(ordered_paths: &[&PathBuf], edges: &[(usize, usize)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    ordered_paths.len().hash(&mut hasher);
    for p in ordered_paths {
        p.hash(&mut hasher);
    }
    edges.len().hash(&mut hasher);
    for e in edges {
        e.hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_test_repo(dir: &Path) {
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

        let paths: Vec<_> = map
            .files
            .iter()
            .map(|f| f.path.display().to_string())
            .collect();
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

        let config_entry = map
            .files
            .iter()
            .find(|f| f.path.display().to_string().contains("config.rs"))
            .expect("config.rs should be in the map");

        assert!(
            config_entry.rank > 0.0,
            "config.rs should have positive rank"
        );

        let sym_names: Vec<_> = config_entry
            .symbols
            .iter()
            .map(|s| s.name.as_str())
            .collect();
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
        assert!(
            text.contains("config.rs"),
            "output should contain config.rs"
        );
        assert!(
            text.contains("Config"),
            "output should contain Config symbol"
        );
    }

    #[test]
    fn format_respects_budget() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let map = build_repo_map(dir.path());
        let text = map.format(100);

        assert!(
            text.len() <= 200,
            "output should be near budget: {}",
            text.len()
        );
    }

    #[test]
    fn empty_dir_produces_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let map = build_repo_map(dir.path());
        assert!(map.files.is_empty());
    }

    #[test]
    fn pagerank_basic() {
        let edges = vec![(0, 1), (1, 2)];
        let ranks = pagerank(&edges, 3, 0.85, 20);

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
        assert!((ranks[0] - ranks[1]).abs() < 1e-10);
        assert!((ranks[1] - ranks[2]).abs() < 1e-10);
    }

    #[test]
    fn incremental_matches_full_build() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let full = build_repo_map(dir.path());
        let incr = IncrementalRepoMap::new(dir.path());

        assert_eq!(
            full.files.len(),
            incr.map().files.len(),
            "incremental should match full build"
        );

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
            incr.map()
                .files
                .iter()
                .any(|f| f.path.display().to_string().contains("server.rs")),
            "server.rs should be in initial map"
        );

        let server_path = dir.path().join("src/server.rs");
        fs::remove_file(&server_path).unwrap();

        let changed = incr.update(&[], &[server_path]);
        assert!(changed, "removal should report change");

        assert!(
            !incr
                .map()
                .files
                .iter()
                .any(|f| f.path.display().to_string().contains("server.rs")),
            "server.rs should be gone after removal"
        );
    }

    #[test]
    fn incremental_add_new_file() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let mut incr = IncrementalRepoMap::new(dir.path());
        let initial_count = incr.map().files.len();

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
            incr.map()
                .files
                .iter()
                .any(|f| f.path.display().to_string().contains("utils.rs")),
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

    #[test]
    fn incremental_update_skips_pagerank_when_edges_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let mut incr = IncrementalRepoMap::new(dir.path());
        assert_eq!(incr.pagerank_runs(), 1);

        let config_path = dir.path().join("src/config.rs");
        let original = fs::read_to_string(&config_path).unwrap();
        fs::write(
            &config_path,
            format!("// cosmetic change, no structural effect\n{original}"),
        )
        .unwrap();

        let changed = incr.update(std::slice::from_ref(&config_path), &[]);
        assert!(changed, "update should still report parse change");
        assert_eq!(
            incr.pagerank_runs(),
            1,
            "pagerank should be skipped when edge graph is unchanged"
        );

        fs::write(
            &config_path,
            r#"
use crate::server::Server;

pub struct Config {
    pub port: u16,
    pub host: String,
    pub server: Option<Server>,
}
"#,
        )
        .unwrap();

        let changed = incr.update(&[config_path], &[]);
        assert!(changed);
        assert_eq!(
            incr.pagerank_runs(),
            2,
            "pagerank must re-run when edge graph changes"
        );
    }

    #[test]
    fn is_repo_root_detects_git() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(is_inside_repo(dir.path()), ".git should be detected");
    }

    #[test]
    fn is_repo_root_detects_jj() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".jj")).unwrap();
        assert!(is_inside_repo(dir.path()), ".jj should be detected");
    }

    #[test]
    fn is_repo_root_negative_for_bare_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !is_inside_repo(dir.path()),
            "bare tempdir should not be a repo"
        );
    }

    #[test]
    fn is_repo_root_finds_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        let nested = dir.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        assert!(
            is_inside_repo(&nested),
            "nested dir under .git root should count"
        );
    }

    #[test]
    fn discover_files_respects_max_cap() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        for i in 0..20 {
            fs::write(src.join(format!("f{i}.rs")), format!("fn f{i}() {{}}")).unwrap();
        }
        let opts = DiscoverOptions { max_files: 5 };
        let files = discover_files_with_options(dir.path(), &opts);
        assert_eq!(files.len(), 5, "should cap at max_files");
    }

    #[test]
    fn discover_files_default_returns_all_small_repos() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        for i in 0..3 {
            fs::write(src.join(format!("f{i}.rs")), format!("fn f{i}() {{}}")).unwrap();
        }
        let files = discover_files(dir.path());
        assert_eq!(files.len(), 3, "small repos should not be capped");
    }

    #[test]
    fn parallel_parse_matches_serial() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("alpha.rs"), "fn alpha() {}\nstruct Beta;").unwrap();
        fs::write(
            src.join("gamma.py"),
            "def gamma():\n    pass\ndef delta(): pass",
        )
        .unwrap();
        fs::write(
            src.join("epsilon.go"),
            "package main\nfunc Epsilon() {}\nfunc Zeta() {}",
        )
        .unwrap();

        let files = discover_files(dir.path());
        assert_eq!(files.len(), 3, "should find all code files");

        let mut serial: FxHashMap<PathBuf, FileData> = FxHashMap::default();
        for path in &files {
            if let Ok(data) = parse_file_data(path) {
                serial.insert(path.clone(), data);
            }
        }

        let parallel = parse_files_parallel(&files);

        assert_eq!(
            serial.len(),
            parallel.len(),
            "parallel should produce same number of results"
        );
        for (path, s_data) in &serial {
            let p_data = parallel
                .get(path)
                .unwrap_or_else(|| panic!("missing {}", path.display()));
            assert_eq!(
                s_data.definitions.len(),
                p_data.definitions.len(),
                "definition count mismatch for {}",
                path.display()
            );
            assert_eq!(
                s_data.references,
                p_data.references,
                "reference mismatch for {}",
                path.display()
            );
        }
    }

    #[test]
    fn discover_files_skips_data_languages() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("config.json"), "{}").unwrap();
        fs::write(dir.path().join("data.toml"), "[x]\ny = 1").unwrap();
        fs::write(dir.path().join("readme.md"), "# hi").unwrap();
        fs::write(dir.path().join("style.css"), "body {}").unwrap();

        let files = discover_files(dir.path());
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();

        assert!(names.contains(&"main.rs"), "should include code files");
        assert!(
            !names.contains(&"config.json"),
            "should skip json: {names:?}"
        );
        assert!(!names.contains(&"data.toml"), "should skip toml: {names:?}");
        assert!(
            !names.contains(&"readme.md"),
            "should skip markdown: {names:?}"
        );
        assert!(!names.contains(&"style.css"), "should skip css: {names:?}");
    }
}
