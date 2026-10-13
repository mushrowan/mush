use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::local::Layout;
use crate::protocol::now_ms;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalStatus {
    pub last_attempt_ms: u64,
    #[serde(default)]
    pub last_ok_ms: Option<u64>,
    #[serde(default)]
    pub offline_since_ms: Option<u64>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub rev: u64,
    #[serde(default)]
    pub waiting: Vec<(String, String)>,
    #[serde(default)]
    pub failed: Vec<(String, String)>,
}

impl LocalStatus {
    #[must_use]
    pub fn load(layout: &Layout) -> Self {
        std::fs::read(layout.status_file())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, layout: &Layout) -> std::io::Result<()> {
        mush_ai::private_io::write_private(&layout.status_file(), serde_json::to_vec(self)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pending {
    Fork {
        session: String,
        fork: String,
        at_ms: u64,
        why: String,
    },
    Sidecar {
        session: String,
        path: PathBuf,
        at_ms: u64,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Review {
    pub forks: Vec<(String, String, String)>,
    pub sidecars: Vec<(String, PathBuf)>,
}

impl Review {
    #[must_use]
    pub fn len(&self) -> usize {
        self.forks.len() + self.sidecars.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn load_pending(layout: &Layout) -> Vec<Pending> {
    std::fs::read(layout.review_file())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_pending(layout: &Layout, pending: &[Pending]) -> std::io::Result<()> {
    mush_ai::private_io::write_private(&layout.review_file(), serde_json::to_vec(pending)?)
}

pub fn remember(layout: &Layout, items: Vec<Pending>) -> std::io::Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut pending = load_pending(layout);
    for item in items {
        if !pending.contains(&item) {
            pending.push(item);
        }
    }
    save_pending(layout, &pending)
}

#[must_use]
pub fn fork(session: &str, fork: &str, why: &str) -> Pending {
    Pending::Fork {
        session: session.to_owned(),
        fork: fork.to_owned(),
        at_ms: now_ms(),
        why: why.to_owned(),
    }
}

#[must_use]
pub fn sidecar(session: &str, path: PathBuf) -> Pending {
    Pending::Sidecar {
        session: session.to_owned(),
        path,
        at_ms: now_ms(),
    }
}

pub fn review(layout: &Layout) -> std::io::Result<Review> {
    let pending = load_pending(layout);
    let mut review = Review::default();
    let mut still = Vec::new();
    for item in &pending {
        match item {
            Pending::Fork {
                session, fork, why, ..
            } if layout.meta(fork).is_file() => {
                review
                    .forks
                    .push((session.clone(), fork.clone(), why.clone()));
                still.push(item.clone());
            }
            Pending::Sidecar { session, path, .. } if path.is_file() => {
                review.sidecars.push((session.clone(), path.clone()));
                still.push(item.clone());
            }
            _ => {}
        }
    }
    if still.len() != pending.len() {
        save_pending(layout, &still)?;
    }
    match std::fs::read_dir(&layout.sessions) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some((session, _)) = name.split_once(".json.sync-conflict-") {
                    review.sidecars.push((session.to_owned(), entry.path()));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    review.sidecars.sort();
    review.sidecars.dedup();
    Ok(review)
}
