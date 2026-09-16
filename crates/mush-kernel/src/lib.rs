pub mod kernel;
pub mod protocol;
pub mod python;

pub use kernel::{
    CellError, CellOutput, HostError, HostRequestHandler, Kernel, KernelConfig, NoHost,
    RestoreReport, SnapshotReport,
};
pub use protocol::{Event, PROTOCOL_VERSION, Request};

#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("no python interpreter found on PATH (set {})", python::PYTHON_ENV)]
    NoPython,
    #[error("python interpreter not found at {0}")]
    PythonMissing(std::path::PathBuf),
    #[error("kernel process has no stdio pipes")]
    Pipes,
    #[error("kernel did not announce itself within {0:?}")]
    ReadyTimeout(std::time::Duration),
    #[error("kernel protocol error: {0}")]
    Protocol(String),
    #[error("kernel request failed: {0}")]
    Request(String),
    #[error("kernel exited{}", if .0.is_empty() { String::new() } else { format!(":\n{}", .0) })]
    Exited(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
