//! file watcher for incremental repo map updates
//!
//! watches the repository for file changes and debounces events
//! into batched updates to the IncrementalRepoMap

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::Language;
use crate::repo_map::IncrementalRepoMap;

/// debounce interval for file change events
const DEBOUNCE_MS: u64 = 500;

/// shared repo map text that the watcher keeps up to date
pub type SharedMapText = Arc<RwLock<String>>;

/// watches a repo and keeps a formatted repo map string current
///
/// the watcher runs in a background thread. consumers read the
/// latest map text from the shared `SharedMapText`. the watcher
/// is dropped when `RepoMapWatcher` is dropped.
pub struct RepoMapWatcher {
    _watcher: RecommendedWatcher,
    map_text: SharedMapText,
}

impl RepoMapWatcher {
    /// start watching a directory
    ///
    /// builds the initial repo map and starts a file watcher.
    /// returns None if the watcher cannot be created.
    pub fn start(root: &Path, token_budget: usize) -> Option<Self> {
        let incr_map = IncrementalRepoMap::new(root);
        let initial_text = incr_map.map().format_for_tokens(token_budget);
        let map_text: SharedMapText = Arc::new(RwLock::new(initial_text));

        let state = Arc::new(std::sync::Mutex::new(WatcherState {
            incr_map,
            pending_changed: Vec::new(),
            pending_removed: Vec::new(),
            token_budget,
        }));

        let map_text_clone = map_text.clone();
        let state_clone = state.clone();

        // debounce timer: process pending events after DEBOUNCE_MS of quiet
        let (debounce_tx, debounce_rx) = std::sync::mpsc::channel::<()>();

        // spawn debounce processing thread
        std::thread::spawn(move || {
            loop {
                // wait for a signal that events arrived
                if debounce_rx.recv().is_err() {
                    break;
                }
                // drain any additional signals that came in during debounce
                std::thread::sleep(Duration::from_millis(DEBOUNCE_MS));
                while debounce_rx.try_recv().is_ok() {}

                // process accumulated events
                let mut guard = state_clone.lock().unwrap();
                let changed: Vec<PathBuf> = guard.pending_changed.drain(..).collect();
                let removed: Vec<PathBuf> = guard.pending_removed.drain(..).collect();

                if changed.is_empty() && removed.is_empty() {
                    continue;
                }

                let any_update = guard.incr_map.update(&changed, &removed);

                // also check for newly created files
                let any_new = guard.incr_map.add_new_files();

                if any_update || any_new {
                    let new_text = guard.incr_map.map().format_for_tokens(guard.token_budget);
                    if let Ok(mut text) = map_text_clone.write() {
                        *text = new_text;
                    }
                    tracing::debug!(files = guard.incr_map.map().files.len(), "repo map updated");
                }
            }
        });

        let root_owned = root.to_path_buf();
        let watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            let Ok(event) = res else { return };

            let dominated = |kind: &EventKind| {
                matches!(
                    kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                )
            };

            if !dominated(&event.kind) {
                return;
            }

            let relevant: Vec<PathBuf> = event
                .paths
                .into_iter()
                .filter(|p| p.is_relative() || p.starts_with(&root_owned))
                .filter(|p| Language::detect(p).is_some())
                .collect();

            if relevant.is_empty() {
                return;
            }

            if let Ok(mut guard) = state.lock() {
                if matches!(event.kind, EventKind::Remove(_)) {
                    guard.pending_removed.extend(relevant);
                } else {
                    guard.pending_changed.extend(relevant);
                }
            }

            let _ = debounce_tx.send(());
        })
        .ok()?;

        let mut watcher = watcher;
        watcher.watch(root, RecursiveMode::Recursive).ok()?;

        Some(Self {
            _watcher: watcher,
            map_text,
        })
    }

    /// get the shared map text handle
    ///
    /// the text is updated by the background watcher thread.
    /// readers should acquire a read lock briefly.
    pub fn map_text(&self) -> &SharedMapText {
        &self.map_text
    }
}

struct WatcherState {
    incr_map: IncrementalRepoMap,
    pending_changed: Vec<PathBuf>,
    pending_removed: Vec<PathBuf>,
    token_budget: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_test_repo(dir: &Path) {
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("lib.rs"),
            "pub fn hello() -> &'static str { \"hello\" }\n",
        )
        .unwrap();
    }

    #[test]
    fn watcher_starts_with_initial_map() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let watcher = RepoMapWatcher::start(dir.path(), 1024).unwrap();
        let text = watcher.map_text().read().unwrap();

        assert!(text.contains("lib.rs"), "initial map should contain lib.rs");
        assert!(
            text.contains("hello"),
            "initial map should contain hello fn"
        );
    }

    #[test]
    fn watcher_detects_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let watcher = RepoMapWatcher::start(dir.path(), 1024).unwrap();

        // verify initial state
        {
            let text = watcher.map_text().read().unwrap();
            assert!(
                !text.contains("goodbye"),
                "should not contain goodbye initially"
            );
        }

        // add a new file
        fs::write(
            dir.path().join("src/utils.rs"),
            "pub fn goodbye() -> &'static str { \"bye\" }\n",
        )
        .unwrap();

        // wait for debounce + processing
        std::thread::sleep(Duration::from_millis(DEBOUNCE_MS + 500));

        let text = watcher.map_text().read().unwrap();
        assert!(
            text.contains("goodbye"),
            "map should update after file change: {text}"
        );
    }
}
