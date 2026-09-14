use std::cell::Cell;
use std::time::{Duration, Instant};

use chrono::Utc;
use crossterm::event::KeyEvent;
use mush_supervisor::status::{Loader, ReadError, RegistrationStore, Snapshot};

use crate::keybinds::{Action, KeyMap, KeyScope};

pub mod render;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub struct SubagentPanel {
    pub open: bool,
    pub selected: usize,
    pub visible_rows: Cell<usize>,
    pub details: bool,
    pub detail_scroll: u16,
    pub snapshot: Snapshot,
    store: Option<RegistrationStore>,
    loader: Option<Loader>,
    pending: Option<tokio::task::JoinHandle<(RegistrationStore, u64, Loader)>>,
    retired: Option<tokio::task::JoinHandle<(RegistrationStore, u64, Loader)>>,
    generation: u64,
    next_refresh: Instant,
}

impl Default for SubagentPanel {
    fn default() -> Self {
        Self {
            open: false,
            selected: 0,
            visible_rows: Cell::new(1),
            details: false,
            detail_scroll: 0,
            snapshot: Snapshot::default(),
            store: None,
            loader: Some(Loader::default()),
            pending: None,
            retired: None,
            generation: 0,
            next_refresh: Instant::now(),
        }
    }
}

impl SubagentPanel {
    pub fn handle_key(&mut self, keymap: &KeyMap, key: KeyEvent) -> bool {
        if keymap.matches(Action::ToggleSubagents, key) {
            self.open = !self.open;
            self.next_refresh = Instant::now();
            return true;
        }
        if !self.open {
            return false;
        }
        let matches = |action| keymap.matches_in(KeyScope::Subagents, action, key);
        let count = self
            .snapshot
            .roots
            .iter()
            .map(|r| r.tasks.len())
            .sum::<usize>();
        let page = self.visible_rows.get().max(1);
        if matches(Action::Cancel) {
            self.open = false;
        } else if matches(Action::Confirm) {
            self.details = !self.details;
            self.detail_scroll = 0;
        } else if self.details {
            if matches(Action::NextItem) || matches(Action::PageDown) {
                self.detail_scroll = self
                    .detail_scroll
                    .saturating_add(if matches(Action::PageDown) {
                        page as u16
                    } else {
                        1
                    })
                    .min(4096);
            } else if matches(Action::PreviousItem) || matches(Action::PageUp) {
                self.detail_scroll =
                    self.detail_scroll
                        .saturating_sub(if matches(Action::PageUp) {
                            page as u16
                        } else {
                            1
                        });
            } else if matches(Action::NavigateTop) {
                self.detail_scroll = 0;
            }
        } else if matches(Action::NextItem) {
            self.selected = self.selected.saturating_add(1);
        } else if matches(Action::PreviousItem) {
            self.selected = self.selected.saturating_sub(1);
        } else if matches(Action::PageDown) {
            self.selected = self.selected.saturating_add(page);
        } else if matches(Action::PageUp) {
            self.selected = self.selected.saturating_sub(page);
        } else if matches(Action::NavigateTop) {
            self.selected = 0;
        } else if matches(Action::NavigateBottom) {
            self.selected = count.saturating_sub(1);
        }
        self.selected = self.selected.min(count.saturating_sub(1));
        true
    }

    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.store = None;
        self.snapshot = Snapshot::default();
        self.loader = Some(Loader::default());
        self.selected = 0;
        self.details = false;
        self.detail_scroll = 0;
        self.visible_rows.set(1);
        self.next_refresh = Instant::now();
    }

    pub async fn poll(
        &mut self,
        store: Result<RegistrationStore, ReadError>,
        now: Instant,
    ) -> bool {
        let mut changed = false;
        if self.retired.as_ref().is_some_and(|task| task.is_finished()) {
            self.retired = None;
        }
        let store = match store {
            Ok(store) => store,
            Err(error) => {
                self.invalidate();
                if let Some(task) = self.pending.take() {
                    task.abort();
                    self.retired = Some(task);
                }
                self.snapshot.registration_error = Some(error);
                return self.open;
            }
        };
        if self.store.as_ref() != Some(&store) {
            self.generation = self.generation.wrapping_add(1);
            self.store = Some(store.clone());
            self.snapshot = Snapshot::default();
            self.loader = Some(Loader::default());
            self.selected = 0;
            self.details = false;
            self.detail_scroll = 0;
            self.next_refresh = now;
            changed = self.open;
        }
        if self.pending.as_ref().is_some_and(|task| task.is_finished())
            && let Some(task) = self.pending.take()
        {
            match task.await {
                Ok((identity, generation, loader))
                    if identity == store && generation == self.generation =>
                {
                    let selected = self
                        .snapshot
                        .roots
                        .iter()
                        .flat_map(|r| {
                            r.tasks
                                .iter()
                                .map(move |t| (&r.source.directory, &t.worker))
                        })
                        .nth(self.selected)
                        .map(|(root, worker)| (root.clone(), worker.clone()));
                    self.snapshot = loader.snapshot().clone();
                    let tasks = self.snapshot.roots.iter().flat_map(|r| {
                        r.tasks
                            .iter()
                            .map(move |t| (&r.source.directory, &t.worker))
                    });
                    if let Some(selected) = selected {
                        self.selected = tasks
                            .clone()
                            .position(|(root, worker)| *root == selected.0 && *worker == selected.1)
                            .unwrap_or_else(|| self.selected.min(tasks.count().saturating_sub(1)));
                    }
                    self.loader = Some(loader);
                    changed |= self.open;
                }
                Ok(_) => {}
                Err(_) => {
                    self.snapshot.registration_error = Some(ReadError::Unavailable);
                    self.loader = Some(Loader::default());
                    changed |= self.open;
                }
            }
        }
        if self.open && self.pending.is_none() && self.retired.is_none() && now >= self.next_refresh
        {
            self.next_refresh = now + REFRESH_INTERVAL;
            let mut loader = self.loader.take().unwrap_or_default();
            let generation = self.generation;
            self.pending = Some(tokio::task::spawn_blocking(move || {
                match store.load() {
                    Ok(registration) => loader.refresh(&registration, Utc::now()),
                    Err(ReadError::Missing) if loader.snapshot().roots.is_empty() => {
                        loader.refresh(&store.empty(), Utc::now());
                    }
                    Err(error) => loader.registration_failed(error),
                }
                (store, generation, loader)
            }));
        }
        changed
    }
}

impl Drop for SubagentPanel {
    fn drop(&mut self) {
        if let Some(task) = self.pending.take() {
            task.abort();
        }
        if let Some(task) = self.retired.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests;
