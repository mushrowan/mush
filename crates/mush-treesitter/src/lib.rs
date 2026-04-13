pub mod chunking;
pub mod language;
pub mod parser;
pub mod references;
pub mod repo_map;
pub mod symbols;
pub mod watcher;

mod queries;

pub use chunking::{Chunk, ChunkOptions, chunk_file};
pub use language::Language;
pub use parser::{ParseError, ParserPool};
pub use repo_map::{DiscoverOptions, IncrementalRepoMap, RepoMap, build_repo_map, is_inside_repo};
pub use symbols::{SymbolInfo, SymbolKind};
pub use watcher::{RepoMapWatcher, SharedMapText};
