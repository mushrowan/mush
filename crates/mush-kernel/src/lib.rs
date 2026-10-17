#![feature(sync_nonpoison, nonpoison_mutex)]

pub mod kernel;
pub mod process;
pub mod protocol;
pub mod python;
pub mod venv;

pub use kernel::{
    Attachment, CellError, CellOutput, DEFAULT_MAX_OUTPUT_CHARS, HostError, HostRequestHandler,
    Kernel, KernelConfig, NoHost, RESTART_RESETS, RESTORED_TAG, RestoreReport, SnapshotReport,
    Waker, restore_notice_text,
};
pub use protocol::{Event, Request};
pub use python::PythonPlan;
pub use venv::{DEFAULT_PACKAGES, ManagedVenv, Package, SkillPackage};

#[async_trait::async_trait]
pub trait KernelState: Send + Sync {
    async fn snapshot(&self, _path: &std::path::Path) -> Result<Option<SnapshotReport>, String> {
        Ok(None)
    }
    async fn restore_from(&self, _path: &std::path::Path) -> Option<Result<RestoreReport, String>> {
        None
    }
    async fn live_names(&self) -> Option<Result<Vec<String>, String>> {
        None
    }
    async fn reset(&self) {}
    fn attach_session(&self, _session: KernelSession) {}
    fn background_work(&self) -> bool {
        false
    }
    fn set_model(&self, _id: &str, _accepts_images: bool) {}
    fn pid(&self) -> Option<u32> {
        None
    }
    async fn release(&self) -> Release {
        Release::NotRunning
    }
    fn waiting(&self) -> Option<Waiting> {
        None
    }
    fn wake_waits(&self) {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Release {
    Released(SnapshotReport),
    NotRunning,
    Busy,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Waiting {
    pub on: String,
    pub since_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelSession {
    pub session_dir: std::path::PathBuf,
    pub state_path: std::path::PathBuf,
    pub depth: u32,
    pub max_depth: u32,
}

impl KernelSession {
    #[must_use]
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "RLM_SESSION_DIR".into(),
                self.session_dir.to_string_lossy().into_owned(),
            ),
            ("RLM_DEPTH".into(), self.depth.to_string()),
            ("RLM_MAX_DEPTH".into(), self.max_depth.to_string()),
        ]
    }
}

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
    #[error("bad protocol frame: {0}")]
    Corrupted(String),
    #[error("kernel request failed: {0}")]
    Request(String),
    #[error("kernel request timed out after {0:?}")]
    RequestTimeout(std::time::Duration),
    #[error("kernel exited{}", if .0.is_empty() { String::new() } else { format!(":\n{}", .0) })]
    Exited(String),
    #[error("an interrupted cell is still running")]
    Busy,
    #[error(
        "uv is needed to set up the kernel's python. install uv, or point {} at a python",
        python::PYTHON_ENV
    )]
    NoUv,
    #[error("kernel python setup failed: {0}")]
    Bootstrap(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
