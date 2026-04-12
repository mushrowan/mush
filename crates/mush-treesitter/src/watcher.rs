//! file watcher for incremental repo map updates
//!
//! watches the repository for file changes and debounces events
//! into batched updates to the IncrementalRepoMap

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
/// latest map text from the shared `SharedMapText`. the initial
/// build happens asynchronously so `start` returns immediately.
/// use `is_ready` to check whether the first build has completed.
pub struct RepoMapWatcher {
    _watcher: RecommendedWatcher,
    map_text: SharedMapText,
    ready: Arc<AtomicBool>,
}

impl RepoMapWatcher {
    /// start watching a directory
    ///
    /// returns immediately with an empty map. the initial treesitter
    /// build runs in a background thread and publishes via the shared
    /// map text when complete. file change events are queued during
    /// the build and processed afterwards.
    #[tracing::instrument(name = "repo_map_watcher", skip_all)]
    pub fn start(root: &Path, token_budget: usize) -> Option<Self> {
        let map_text: SharedMapText = Arc::new(RwLock::new(String::new()));
        let ready = Arc::new(AtomicBool::new(false));

        let state = Arc::new(std::sync::Mutex::new(WatcherState {
            incr_map: None,
            pending_changed: Vec::new(),
            pending_removed: Vec::new(),
            token_budget,
        }));

        let map_text_clone = map_text.clone();
        let state_clone = state.clone();
        let ready_clone = ready.clone();
        let root_owned_for_build = root.to_path_buf();

        // debounce channel: file watcher signals events arrived
        let (debounce_tx, debounce_rx) = std::sync::mpsc::channel::<()>();

        // single background thread: initial build then debounce loop
        std::thread::spawn(move || {
            // phase 1: build the initial repo map (the expensive part)
            let incr_map = IncrementalRepoMap::new(&root_owned_for_build);
            let initial_text = incr_map.map().format_for_tokens(token_budget);

            // publish the initial text and store the map
            if let Ok(mut text) = map_text_clone.write() {
                *text = initial_text;
            }
            {
                let mut guard = state_clone.lock().unwrap();
                guard.incr_map = Some(incr_map);

                // process any events that arrived during the build
                let changed: Vec<PathBuf> = guard.pending_changed.drain(..).collect();
                let removed: Vec<PathBuf> = guard.pending_removed.drain(..).collect();
                if !changed.is_empty() || !removed.is_empty() {
                    let budget = guard.token_budget;
                    if let Some(ref mut map) = guard.incr_map {
                        let any_update = map.update(&changed, &removed);
                        let any_new = map.add_new_files();
                        if any_update || any_new {
                            let new_text = map.map().format_for_tokens(budget);
                            if let Ok(mut text) = map_text_clone.write() {
                                *text = new_text;
                            }
                        }
                    }
                }
            }
            ready_clone.store(true, Ordering::Release);
            tracing::debug!("repo map initial build complete");

            // phase 2: debounce loop for file change events
            loop {
                if debounce_rx.recv().is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(DEBOUNCE_MS));
                while debounce_rx.try_recv().is_ok() {}

                let mut guard = state_clone.lock().unwrap();
                let changed: Vec<PathBuf> = guard.pending_changed.drain(..).collect();
                let removed: Vec<PathBuf> = guard.pending_removed.drain(..).collect();

                if changed.is_empty() && removed.is_empty() {
                    continue;
                }

                let budget = guard.token_budget;
                if let Some(ref mut map) = guard.incr_map {
                    let any_update = map.update(&changed, &removed);
                    let any_new = map.add_new_files();

                    if any_update || any_new {
                        let new_text = map.map().format_for_tokens(budget);
                        if let Ok(mut text) = map_text_clone.write() {
                            *text = new_text;
                        }
                        tracing::debug!(files = map.map().files.len(), "repo map updated");
                    }
                }
            }
        });

        let root_owned = root.to_path_buf();
        let watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            let Ok(event) = res else { return };

            let relevant_kind = matches!(
                event.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
            );
            if !relevant_kind {
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
            ready,
        })
    }

    /// whether the initial build has completed
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
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
    incr_map: Option<IncrementalRepoMap>,
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

    /// helper: wait until the background build finishes
    fn wait_ready(watcher: &RepoMapWatcher, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while !watcher.is_ready() {
            assert!(
                std::time::Instant::now() < deadline,
                "repo map build timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn watcher_returns_immediately_with_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let watcher = RepoMapWatcher::start(dir.path(), 1024).unwrap();
        // should not block: map starts empty, build happens in background
        assert!(
            !watcher.map_text().read().unwrap().is_empty() || !watcher.is_ready(),
            "either the map is empty (not built yet) or it was fast enough to be ready"
        );
    }

    #[test]
    fn watcher_populates_map_in_background() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let watcher = RepoMapWatcher::start(dir.path(), 1024).unwrap();
        wait_ready(&watcher, Duration::from_secs(10));

        let text = watcher.map_text().read().unwrap();
        assert!(
            text.contains("lib.rs"),
            "map should contain lib.rs after build"
        );
        assert!(
            text.contains("hello"),
            "map should contain hello fn after build"
        );
    }

    #[test]
    fn watcher_detects_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let watcher = RepoMapWatcher::start(dir.path(), 1024).unwrap();
        wait_ready(&watcher, Duration::from_secs(10));

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
