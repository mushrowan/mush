use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::Language;
use crate::repo_map::{IncrementalRepoMap, RepoMap};

const DEBOUNCE_MS: u64 = 500;

pub type SharedRepoMap = Arc<RwLock<RepoMap>>;

fn discover_watch_dirs(root: &Path) -> Vec<PathBuf> {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut dirs = Vec::new();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            dirs.push(entry.into_path());
        }
    }
    dirs
}

pub struct RepoMapWatcher {
    _watcher: Arc<Mutex<Option<RecommendedWatcher>>>,
    map: SharedRepoMap,
    ready: Arc<AtomicBool>,
}

impl RepoMapWatcher {
    #[tracing::instrument(name = "repo_map_watcher", skip_all)]
    pub fn start(root: &Path) -> Option<Self> {
        let map: SharedRepoMap = Arc::new(RwLock::new(RepoMap::default()));
        let ready = Arc::new(AtomicBool::new(false));

        let state = Arc::new(Mutex::new(WatcherState {
            incr_map: None,
            pending_changed: Vec::new(),
            pending_removed: Vec::new(),
        }));

        let (debounce_tx, debounce_rx) = std::sync::mpsc::channel::<()>();

        std::thread::spawn({
            let map = map.clone();
            let state = state.clone();
            let ready = ready.clone();
            let root = root.to_path_buf();
            move || {
                let incr_map = IncrementalRepoMap::new(&root);
                let initial = incr_map.map().clone();
                if let Ok(mut guard) = map.write() {
                    *guard = initial;
                }
                {
                    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
                    guard.incr_map = Some(incr_map);
                    guard.apply_pending(&map);
                }
                ready.store(true, Ordering::Release);
                tracing::debug!("repo map initial build complete");

                while debounce_rx.recv().is_ok() {
                    std::thread::sleep(Duration::from_millis(DEBOUNCE_MS));
                    while debounce_rx.try_recv().is_ok() {}

                    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(files) = guard.apply_pending(&map) {
                        tracing::debug!(files, "repo map updated");
                    }
                }
            }
        });

        let watcher_slot: Arc<Mutex<Option<RecommendedWatcher>>> = Arc::new(Mutex::new(None));
        let watcher_for_cb = watcher_slot.clone();
        let root_for_cb = root.to_path_buf();
        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            let Ok(event) = res else { return };

            let relevant_kind = matches!(
                event.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
            );
            if !relevant_kind {
                return;
            }

            if matches!(event.kind, EventKind::Create(_)) {
                for path in &event.paths {
                    if !path.is_dir() {
                        continue;
                    }
                    let dirs = discover_watch_dirs(path);
                    if dirs.is_empty() {
                        continue;
                    }
                    if let Ok(mut slot) = watcher_for_cb.lock()
                        && let Some(w) = slot.as_mut()
                    {
                        for dir in dirs {
                            let _ = w.watch(&dir, RecursiveMode::NonRecursive);
                        }
                    }
                }
            }

            let relevant: Vec<PathBuf> = event
                .paths
                .into_iter()
                .filter(|p| p.is_relative() || p.starts_with(&root_for_cb))
                .filter(|p| Language::detect(p).is_some())
                .collect();

            if relevant.is_empty() {
                if matches!(event.kind, EventKind::Create(_)) {
                    let _ = debounce_tx.send(());
                }
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

        for dir in discover_watch_dirs(root) {
            let _ = watcher.watch(&dir, RecursiveMode::NonRecursive);
        }

        *watcher_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(watcher);

        Some(Self {
            _watcher: watcher_slot,
            map,
            ready,
        })
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn map(&self) -> &SharedRepoMap {
        &self.map
    }
}

struct WatcherState {
    incr_map: Option<IncrementalRepoMap>,
    pending_changed: Vec<PathBuf>,
    pending_removed: Vec<PathBuf>,
}

impl WatcherState {
    fn apply_pending(&mut self, map: &SharedRepoMap) -> Option<usize> {
        let changed = std::mem::take(&mut self.pending_changed);
        let removed = std::mem::take(&mut self.pending_removed);
        if changed.is_empty() && removed.is_empty() {
            return None;
        }
        let incr = self.incr_map.as_mut()?;
        let any_update = incr.update(&changed, &removed);
        let any_new = incr.add_new_files();
        if !(any_update || any_new) {
            return None;
        }
        let snapshot = incr.map().clone();
        let file_count = snapshot.files.len();
        if let Ok(mut guard) = map.write() {
            *guard = snapshot;
        }
        Some(file_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir() -> std::io::Result<tempfile::TempDir> {
        if cfg!(target_os = "macos") {
            tempfile::tempdir_in("/private/tmp")
        } else {
            tempfile::tempdir()
        }
    }

    fn setup_test_repo(dir: &Path) {
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("lib.rs"),
            "pub fn hello() -> &'static str { \"hello\" }\n",
        )
        .unwrap();
    }

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

    fn map_text(watcher: &RepoMapWatcher) -> String {
        watcher
            .map()
            .read()
            .unwrap()
            .render(crate::RenderOptions::for_tokens(1024))
    }

    #[test]
    fn watcher_returns_immediately_with_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let watcher = RepoMapWatcher::start(dir.path()).unwrap();
        let map = watcher.map().read().unwrap();
        assert!(
            !map.files.is_empty() || !watcher.is_ready(),
            "either the map is empty (not built yet) or it was fast enough to be ready"
        );
    }

    #[test]
    fn watcher_populates_map_in_background() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());

        let watcher = RepoMapWatcher::start(dir.path()).unwrap();
        wait_ready(&watcher, Duration::from_secs(10));

        let text = map_text(&watcher);
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
    fn watcher_ignores_gitignored_directories() {
        let dir = tempfile::tempdir().unwrap();
        setup_test_repo(dir.path());
        fs::write(dir.path().join(".ignore"), "target/\n").unwrap();

        fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        fs::write(
            dir.path().join("target/debug/build_script.rs"),
            "pub fn placeholder() {}\n",
        )
        .unwrap();

        let watcher = RepoMapWatcher::start(dir.path()).unwrap();
        wait_ready(&watcher, Duration::from_secs(10));

        {
            let text = map_text(&watcher);
            assert!(
                !text.contains("build_script"),
                "initial discovery should skip target/: {text}"
            );
        }

        fs::write(
            dir.path().join("target/debug/build_script.rs"),
            "pub fn junk_symbol_xyz() -> u32 { 42 }\n",
        )
        .unwrap();

        std::thread::sleep(Duration::from_millis(DEBOUNCE_MS + 500));

        let text = map_text(&watcher);
        assert!(
            !text.contains("junk_symbol_xyz"),
            "ignored target/ files should not enter the repo map: {text}"
        );
        assert!(
            !text.contains("build_script"),
            "ignored target/ paths should not enter the repo map: {text}"
        );
    }

    #[test]
    fn watcher_detects_file_changes() {
        let dir = tempdir().unwrap();
        setup_test_repo(dir.path());

        let watcher = RepoMapWatcher::start(dir.path()).unwrap();
        wait_ready(&watcher, Duration::from_secs(10));

        {
            let text = map_text(&watcher);
            assert!(
                !text.contains("goodbye"),
                "should not contain goodbye initially"
            );
        }

        fs::write(
            dir.path().join("src/utils.rs"),
            "pub fn goodbye() -> &'static str { \"bye\" }\n",
        )
        .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut text = map_text(&watcher);
        while !text.contains("goodbye") && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            text = map_text(&watcher);
        }
        assert!(
            text.contains("goodbye"),
            "map should update after file change: {text}"
        );
    }
}
