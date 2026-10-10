#![feature(sync_nonpoison, nonpoison_mutex)]

pub mod language;
pub mod parser;
pub mod references;
pub mod repo_map;
pub mod symbols;
pub mod watcher;

mod queries;

pub use language::Language;
pub use parser::{ParseError, ParserPool};
pub use repo_map::{IncrementalRepoMap, RankedFile, RenderOptions, RepoMap, is_inside_repo};
pub use symbols::{SymbolInfo, SymbolKind};
pub use watcher::{RepoMapWatcher, SharedRepoMap};
