pub mod client;
pub mod discovery;
pub mod error;
pub mod registry;
pub mod tools;
pub mod transport;

pub use client::{LspClient, format_diagnostics};
pub use discovery::{ServerConfig, discover_for_language};
pub use error::LspError;
pub use registry::LspRegistry;
pub use tools::lsp_tools;
