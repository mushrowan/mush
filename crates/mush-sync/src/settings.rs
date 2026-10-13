use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::engine::Engine;
use crate::keys::{KeyError, Keys};
use crate::local::{Layout, Limits};
use crate::remote::{Remote, RemoteError};

#[derive(Debug, Clone)]
pub struct Settings {
    pub server: String,
    pub key_file: PathBuf,
    pub machine: String,
    pub data_dir: PathBuf,
    pub limits: Limits,
    pub lease_ttl: Duration,
    pub connect_timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error(transparent)]
    Key(#[from] KeyError),
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error("machine name {0:?} needs to be letters, digits, '-', '_' or '.'")]
    Machine(String),
}

impl Settings {
    pub fn engine(&self) -> Result<Arc<Engine>, SetupError> {
        self.build(None)
    }

    pub fn engine_with_progress(
        &self,
        progress: crate::engine::Progress,
    ) -> Result<Arc<Engine>, SetupError> {
        self.build(Some(progress))
    }

    fn build(&self, progress: Option<crate::engine::Progress>) -> Result<Arc<Engine>, SetupError> {
        if !crate::protocol::valid_holder(&self.machine) {
            return Err(SetupError::Machine(self.machine.clone()));
        }
        let keys = Keys::load(&self.key_file)?;
        let remote = Remote::new(&self.server, keys.token(), self.connect_timeout)?;
        let engine = Engine::new(
            Layout::new(&self.data_dir),
            Arc::new(keys),
            remote,
            self.machine.clone(),
            self.limits,
        );
        Ok(Arc::new(match progress {
            Some(progress) => engine.with_progress(progress),
            None => engine,
        }))
    }
}

#[must_use]
pub fn hostname() -> String {
    let name = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "machine".to_owned()
    } else {
        cleaned
    }
}
