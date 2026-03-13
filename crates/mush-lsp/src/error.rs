//! LSP error types

/// errors from LSP operations
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("LSP server exited unexpectedly")]
    ServerExited,
    #[error("LSP request failed: {code} {message}")]
    Request { code: i64, message: String },
    #[error("LSP server not found for language: {0}")]
    NoServer(String),
    #[error("timeout waiting for LSP response")]
    Timeout,
    #[error("server failed to start: {0}")]
    SpawnFailed(String),
}
