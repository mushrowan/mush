use mush_session::{Session, SessionId, SessionStore};
use mush_sync::Layout;

use crate::harness::user;

fn session(store: &SessionStore, texts: &[&str]) -> SessionId {
    let mut session = Session::new("test-model", "/tmp");
    session.meta.title = Some("backed up".into());
    for text in texts {
        session.push_message(user(text));
    }
    store.save(&session).unwrap();
    session.meta.id
}

fn extend(store: &SessionStore, id: &SessionId, text: &str) {
    let mut session = store.load(id).unwrap();
    session.push_message(user(text));
    store.save(&session).unwrap();
}

fn texts(store: &SessionStore, id: &SessionId) -> Vec<String> {
    store
        .load(id)
        .unwrap()
        .context()
        .iter()
        .filter_map(|message| match message {
            mush_ai::types::Message::User(user) => Some(user.text()),
            _ => None,
        })
        .collect()
}

fn back_up(store: &SessionStore, id: &SessionId, backup: &std::path::Path) {
    std::fs::create_dir_all(backup).unwrap();
    for path in [store.session_path(id), store.history_path(id)] {
        std::fs::copy(&path, backup.join(path.file_name().unwrap())).unwrap();
    }
}

#[test]
fn a_backup_fills_gaps_and_advances_old_copies_but_never_overwrites_newer_work() {
    let here = tempfile::tempdir().unwrap();
    let backup = here.path().join("mush-sync-backup-20261011");
    let store = SessionStore::new(here.path().join("data/sessions"));

    let behind_here = session(&store, &["one"]);
    back_up(&store, &behind_here, &backup);
    let mut copy = store.load(&behind_here).unwrap();
    copy.push_message(user("two, only in the backup"));
    let elsewhere = SessionStore::new(here.path().join("elsewhere"));
    elsewhere.save(&copy).unwrap();
    back_up(&elsewhere, &behind_here, &backup);

    let ahead_here = session(&store, &["one"]);
    back_up(&store, &ahead_here, &backup);
    extend(&store, &ahead_here, "two, only here");

    let split = session(&store, &["one"]);
    let mut theirs = store.load(&split).unwrap();
    theirs.push_message(user("the backup went this way"));
    elsewhere.save(&theirs).unwrap();
    back_up(&elsewhere, &split, &backup);
    extend(&store, &split, "this machine went that way");

    let missing = session(&elsewhere, &["only in the backup"]);
    back_up(&elsewhere, &missing, &backup);

    let layout = Layout::new(&here.path().join("data"));
    let report = mush_sync::import::import(&layout, &backup).unwrap();
    assert_eq!(report.added, [missing.to_string()]);
    assert_eq!(report.advanced, [behind_here.to_string()]);
    assert_eq!(report.forked.len(), 1, "{report:?}");

    assert_eq!(
        texts(&store, &behind_here),
        ["one", "two, only in the backup"]
    );
    assert_eq!(texts(&store, &ahead_here), ["one", "two, only here"]);
    assert_eq!(texts(&store, &split), ["one", "this machine went that way"]);
    let fork = SessionId::from(report.forked[0].1.as_str());
    assert_eq!(texts(&store, &fork), ["one", "the backup went this way"]);
    assert_eq!(
        store.resolve(fork.as_str()).unwrap().title.as_deref(),
        Some("backed up (from mush-sync-backup-20261011)")
    );
    assert_eq!(texts(&store, &missing), ["only in the backup"]);

    let again = mush_sync::import::import(&layout, &backup).unwrap();
    assert!(
        again.added.is_empty() && again.advanced.is_empty() && again.forked.is_empty(),
        "{again:?}"
    );
}

#[test]
fn the_same_diverged_session_in_two_backups_comes_back_as_one_fork() {
    let here = tempfile::tempdir().unwrap();
    let store = SessionStore::new(here.path().join("data/sessions"));
    let id = session(&store, &["one"]);
    let mut theirs = store.load(&id).unwrap();
    theirs.push_message(user("the backup went this way"));
    let elsewhere = SessionStore::new(here.path().join("elsewhere"));
    elsewhere.save(&theirs).unwrap();
    let first = here.path().join("mush-sync-backup-20261007");
    let second = here.path().join("mush-sync-backup-20261011");
    back_up(&elsewhere, &id, &first);
    back_up(&elsewhere, &id, &second);
    let header = second.join(format!("{id}.json"));
    let mut touched: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&header).unwrap()).unwrap();
    touched["meta"]["updated_at"] = 7.into();
    std::fs::write(&header, serde_json::to_vec(&touched).unwrap()).unwrap();
    extend(&store, &id, "this machine went that way");

    let layout = Layout::new(&here.path().join("data"));
    let once = mush_sync::import::import(&layout, &first).unwrap();
    assert_eq!(once.forked.len(), 1, "{once:?}");
    let twice = mush_sync::import::import(&layout, &second).unwrap();
    assert!(twice.forked.is_empty(), "{twice:?}");
    assert_eq!(twice.unchanged, [id.to_string()]);
    assert_eq!(store.list().unwrap().len(), 2);
}

#[test]
fn an_older_backed_up_header_leaves_no_conflict_file_beside_newer_work() {
    let here = tempfile::tempdir().unwrap();
    let backup = here.path().join("mush-sync-backup-20261011");
    let store = SessionStore::new(here.path().join("data/sessions"));
    let id = session(&store, &["one"]);
    back_up(&store, &id, &backup);
    let header = backup.join(format!("{id}.json"));
    let mut older: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&header).unwrap()).unwrap();
    older["meta"]["created_at"] = 5.into();
    older["meta"]["updated_at"] = u64::MAX.into();
    std::fs::write(&header, serde_json::to_vec(&older).unwrap()).unwrap();
    extend(&store, &id, "two, only here");

    let layout = Layout::new(&here.path().join("data"));
    let report = mush_sync::import::import(&layout, &backup).unwrap();
    assert_eq!(report.unchanged, [id.to_string()]);
    assert!(report.kept_beside.is_empty(), "{report:?}");
    let leftovers: Vec<_> = std::fs::read_dir(here.path().join("data/sessions"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("sync-conflict")
        })
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert_eq!(texts(&store, &id), ["one", "two, only here"]);
}

#[test]
fn a_backed_up_harness_adds_its_entries_without_rolling_back_newer_ones() {
    use mush_session::harness::{Action, Edit, Kind, Scope, State};
    let here = tempfile::tempdir().unwrap();
    let backup = here.path().join("backup");
    let global = here.path().join("data/harness");
    let edit = |id: &str, content: &str, action: Action| Edit {
        action,
        kind: Kind::Memory,
        id: Some(id.to_owned()),
        title: Some(id.to_owned()),
        content: Some(content.to_owned()),
        path: None,
        reference: None,
        arguments: None,
        metadata: None,
        reason: None,
    };
    let mut state = State::default();
    state
        .upsert(
            &edit("style", "short", Action::Create),
            Scope::Global,
            "agent",
        )
        .unwrap();
    state
        .upsert(
            &edit("lost", "only in the backup", Action::Create),
            Scope::Global,
            "agent",
        )
        .unwrap();
    state.save(&backup.join("harness")).unwrap();
    state.remove(Kind::Memory, "lost").unwrap();
    state.deleted.clear();
    state
        .upsert(
            &edit("style", "short and kind", Action::Update),
            Scope::Global,
            "agent",
        )
        .unwrap();
    state.save(&global).unwrap();

    std::fs::copy(
        backup.join("harness/harness_state.json"),
        backup.join("harness_state.json"),
    )
    .unwrap();
    let report =
        mush_sync::import::import(&Layout::new(&here.path().join("data")), &backup).unwrap();
    assert!(report.harness);
    assert!(report.added.is_empty(), "{report:?}");
    assert!(
        !here
            .path()
            .join("data/sessions/harness_state.json")
            .exists()
    );
    let merged = State::load(&global, Scope::Global);
    let memories: Vec<(String, String)> = merged
        .list(Some(Kind::Memory))
        .into_iter()
        .map(|entry| (entry.id.clone(), entry.content.clone()))
        .collect();
    assert_eq!(
        memories,
        [
            ("lost".to_owned(), "only in the backup".to_owned()),
            ("style".to_owned(), "short and kind".to_owned())
        ]
    );
}
