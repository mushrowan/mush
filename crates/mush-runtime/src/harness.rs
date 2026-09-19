use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use mush_ai::types::SessionId;
use mush_session::harness::{self, Edit, Entry, Kind, Scope, State};

pub struct HarnessStore {
    local: PathBuf,
    global: PathBuf,
    lock: Mutex<()>,
}

impl HarnessStore {
    #[must_use]
    pub fn new(session_id: &SessionId) -> Self {
        Self {
            local: harness::local_dir(session_id),
            global: harness::global_dir(),
            lock: Mutex::new(()),
        }
    }

    #[must_use]
    pub fn with_dirs(local: PathBuf, global: PathBuf) -> Self {
        Self {
            local,
            global,
            lock: Mutex::new(()),
        }
    }

    fn dir(&self, scope: Scope) -> &Path {
        match scope {
            Scope::Local => &self.local,
            Scope::Global => &self.global,
        }
    }

    #[must_use]
    pub fn merged(&self) -> State {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        State::merged(
            &State::load(&self.global, Scope::Global),
            &State::load(&self.local, Scope::Local),
        )
    }

    #[must_use]
    pub fn list(&self, kind: Option<Kind>) -> Vec<Entry> {
        self.merged().list(kind).into_iter().cloned().collect()
    }

    #[must_use]
    pub fn search(&self, query: &str, kind: Option<Kind>, limit: usize) -> Vec<Entry> {
        self.merged()
            .search(query, kind, limit)
            .into_iter()
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn find(&self, kind: Kind, id: &str, global: bool) -> Option<(Scope, Entry)> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.locate(kind, id, global)
    }

    fn locate(&self, kind: Kind, id: &str, global: bool) -> Option<(Scope, Entry)> {
        let (id, scope) = route(id, global);
        for scope in scope.map_or_else(|| vec![Scope::Local, Scope::Global], |scope| vec![scope]) {
            if let Some(entry) = State::load(self.dir(scope), scope).entry(kind, &id) {
                return Some((scope, entry.clone()));
            }
        }
        None
    }

    pub fn create(&self, global: bool, edit: &Edit) -> Result<Entry, String> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let scope = if global { Scope::Global } else { Scope::Local };
        self.write(scope, |state| state.upsert(edit, scope, "agent"))
    }

    pub fn update(&self, id: &str, global: bool, edit: &Edit) -> Result<Entry, String> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let (scope, existing) = self
            .locate(edit.kind, id, global)
            .ok_or_else(|| format!("{id} not found"))?;
        let mut edit = edit.clone();
        edit.id = Some(existing.id);
        self.write(scope, |state| state.upsert(&edit, scope, "agent"))
    }

    pub fn delete(&self, kind: Kind, id: &str, global: bool) -> Result<Entry, String> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let (scope, existing) = self
            .locate(kind, id, global)
            .ok_or_else(|| format!("{id} not found"))?;
        self.write(scope, |state| state.remove(kind, &existing.id))
    }

    fn write<T>(
        &self,
        scope: Scope,
        edit: impl FnOnce(&mut State) -> Result<T, String>,
    ) -> Result<T, String> {
        let dir = self.dir(scope);
        let mut state = State::load(dir, scope);
        let outcome = edit(&mut state)?;
        state
            .save(dir)
            .map_err(|error| format!("could not save the harness state: {error}"))?;
        Ok(outcome)
    }
}

fn route(id: &str, global: bool) -> (String, Option<Scope>) {
    match id.split_once(':') {
        Some(("local", rest)) if !rest.is_empty() => (rest.to_string(), Some(Scope::Local)),
        Some(("global", rest)) if !rest.is_empty() => (rest.to_string(), Some(Scope::Global)),
        _ => (id.to_string(), global.then_some(Scope::Global)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_session::harness::Action;

    fn store() -> (tempfile::TempDir, HarnessStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStore::with_dirs(dir.path().join("local"), dir.path().join("global"));
        (dir, store)
    }

    fn edit(action: Action, title: &str, content: &str) -> Edit {
        Edit {
            action,
            kind: Kind::Memory,
            id: None,
            title: Some(title.into()),
            content: Some(content.into()),
            path: None,
            reference: None,
            arguments: None,
            metadata: None,
            reason: None,
        }
    }

    #[test]
    fn a_prefixed_id_picks_the_scope_and_a_bare_one_prefers_local() {
        let (_dir, store) = store();
        store
            .create(false, &edit(Action::Create, "tests", "local text"))
            .unwrap();
        store
            .create(true, &edit(Action::Create, "tests", "global text"))
            .unwrap();
        let bare = store.find(Kind::Memory, "tests", false).unwrap();
        assert_eq!(bare.0, Scope::Local);
        assert_eq!(
            store
                .find(Kind::Memory, "global:tests", false)
                .unwrap()
                .1
                .content,
            "global text"
        );
        assert_eq!(
            store
                .find(Kind::Memory, "local:tests", true)
                .unwrap()
                .1
                .content,
            "local text"
        );
        assert_eq!(
            store.find(Kind::Memory, "tests", true).unwrap().1.content,
            "global text"
        );
        assert_eq!(store.list(Some(Kind::Memory)).len(), 2);
    }

    #[test]
    fn an_update_writes_back_to_the_scope_holding_the_entry() {
        let (_dir, store) = store();
        store
            .create(true, &edit(Action::Create, "tests", "global text"))
            .unwrap();
        let mut change = edit(Action::Update, "tests", "changed");
        change.title = None;
        let after = store.update("tests", false, &change).unwrap();
        assert_eq!(after.scope, Scope::Global);
        assert_eq!(after.version, 2);
        assert!(store.merged().entry(Kind::Memory, "tests").is_some());
        assert_eq!(
            store.update("missing", false, &change),
            Err("missing not found".into())
        );
        store.delete(Kind::Memory, "tests", false).unwrap();
        assert!(store.find(Kind::Memory, "tests", false).is_none());
    }

    #[test]
    fn search_reaches_both_scopes() {
        let (_dir, store) = store();
        store
            .create(
                false,
                &edit(Action::Create, "worktree gate", "cargo nextest"),
            )
            .unwrap();
        store
            .create(true, &edit(Action::Create, "formatting", "cargo fmt"))
            .unwrap();
        let hits = store.search("worktree", None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].scope, Scope::Local);
        assert_eq!(store.search("cargo", Some(Kind::Memory), 10).len(), 2);
    }
}
