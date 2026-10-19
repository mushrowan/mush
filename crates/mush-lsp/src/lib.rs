pub mod client;
pub mod discovery;
pub mod error;
pub mod registry;
pub mod tools;
pub mod transport;

pub use client::format_diagnostics;
pub use discovery::ServerConfig;
pub use registry::LspRegistry;
pub use tools::lsp_tools;
