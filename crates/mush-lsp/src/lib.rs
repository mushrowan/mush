//! LSP client integration for mush
//!
//! provides language server management for auto-diagnostics after file edits.
//! spawns and manages LSP server processes, one per language, lazily started.

pub mod client;
pub mod discovery;
pub mod error;
pub mod registry;
pub mod tools;
pub mod transport;

pub use client::{LspClient, format_diagnostics};
pub use discovery::{ServerConfig, discover, discover_for_language};
pub use error::LspError;
pub use registry::LspRegistry;
pub use tools::lsp_tools;
