use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::nonpoison::Mutex;

use chrono::{DateTime, Utc};
use mush_ai::types::Message;
use serde::{Deserialize, Serialize};

pub const MAX_NOTE_CHARS: usize = 4_000;
pub const MAX_NOTES_CHARS: usize = 24_000;
pub const MAX_KEY_CHARS: usize = 48;
pub const DEFAULT_ARCHIVE_LIMIT: usize = 20;
const UPDATES_BEFORE_FULL: u32 = 8;
const STORE_FILE: &str = "notes.json";
const ARCHIVE_FILE: &str = "notes-archive.jsonl";
const LEGACY_FILE: &str = "notes.md";
const AUTO_KEY_PREFIX: char = 'n';

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub key: String,
    pub text: String,
    #[serde(default)]
    pub auto: bool,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveReason {
    Dropped,
    Replaced,
    Cleared,
    Overflow,
}

impl ArchiveReason {
    const fn label(self) -> &'static str {
        match self {
            Self::Dropped => "dropped",
            Self::Replaced => "replaced",
            Self::Cleared => "cleared",
            Self::Overflow => "archived to make room",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Archived {
    pub key: String,
    pub text: String,
    pub reason: ArchiveReason,
    pub archived_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Shown {
    fingerprint: String,
    notes: Vec<(String, String)>,
    updates_since_full: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Store {
    notes: Vec<Note>,
    #[serde(default)]
    next_auto: u64,
    #[serde(default)]
    shown: Option<Shown>,
}

#[derive(Default)]
struct State {
    store: Store,
    archive: Vec<Archived>,
    archive_written: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub key: String,
    pub replaced: bool,
    pub made_room: Vec<String>,
    pub count: usize,
    pub chars: usize,
}

impl std::fmt::Display for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let verb = if self.replaced {
            "replaced (old text archived)"
        } else {
            "kept"
        };
        write!(
            f,
            "{verb} [{}]; {} notes, {} of {MAX_NOTES_CHARS} characters",
            self.key, self.count, self.chars
        )?;
        if !self.made_room.is_empty() {
            let keys: Vec<String> = self
                .made_room
                .iter()
                .map(|key| format!("[{key}]"))
                .collect();
            write!(
                f,
                "; archived {} to make room (rlm.notes(archived=True) reads them)",
                keys.join(", ")
            )?;
        }
        Ok(())
    }
}

pub struct Delivery {
    pub message: Message,
    pub text: String,
}

pub struct Notes {
    dir: Mutex<Option<PathBuf>>,
    state: Mutex<State>,
}

impl Notes {
    #[must_use]
    pub fn new(dir: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(load(dir.as_deref())),
            dir: Mutex::new(dir),
        })
    }

    pub fn rebind(&self, dir: Option<PathBuf>) {
        let state = load(dir.as_deref());
        *self.dir.lock() = dir;
        *self.state.lock() = state;
    }

    #[must_use]
    pub fn read(&self) -> String {
        render(&self.state.lock().store.notes)
    }

    pub fn append(&self, text: &str) -> Result<Kept, String> {
        self.keep(text, None)
    }

    pub fn keep(&self, text: &str, key: Option<&str>) -> Result<Kept, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("a note needs some text; rlm.drop_note(key) removes one".into());
        }
        let length = text.chars().count();
        if length > MAX_NOTE_CHARS {
            return Err(format!(
                "a note is at most {MAX_NOTE_CHARS} characters and this one is {length}; split it \
                 under separate keys or keep the detail in a file and note its path"
            ));
        }
        let explicit = key.map(str::trim).map(validate_key).transpose()?;
        let now = Utc::now();
        let mut state = self.state.lock();
        let auto = explicit.is_none();
        let key = explicit.unwrap_or_else(|| state.store.auto_key());
        let note = Note {
            key: key.clone(),
            text: text.to_string(),
            auto,
            updated_at: now,
        };
        let old_size = state
            .store
            .notes
            .iter()
            .find(|existing| existing.key == key)
            .map_or(0, size);
        let needed = total(&state.store.notes) - old_size + size(&note);
        let made_room = state.make_room(needed, &key, now)?;
        let replaced = match state
            .store
            .notes
            .iter()
            .position(|existing| existing.key == key)
        {
            Some(index) => {
                let old = std::mem::replace(&mut state.store.notes[index], note);
                let changed = old.text != text;
                if changed {
                    state.archive.push(Archived {
                        key: old.key,
                        text: old.text,
                        reason: ArchiveReason::Replaced,
                        archived_at: now,
                    });
                }
                changed
            }
            None => {
                state.store.notes.push(note);
                false
            }
        };
        let kept = Kept {
            key,
            replaced,
            made_room,
            count: state.store.notes.len(),
            chars: total(&state.store.notes),
        };
        self.persist(&mut state);
        Ok(kept)
    }

    pub fn drop_note(&self, key: &str) -> Result<String, String> {
        let key = key.trim();
        let mut state = self.state.lock();
        let Some(index) = state.store.notes.iter().position(|note| note.key == key) else {
            return Err(missing(key, &state.store.notes));
        };
        let note = state.store.notes.remove(index);
        state.archive.push(Archived {
            key: note.key,
            text: note.text,
            reason: ArchiveReason::Dropped,
            archived_at: Utc::now(),
        });
        let left = state.store.notes.len();
        self.persist(&mut state);
        Ok(format!("archived [{key}]; {left} notes left"))
    }

    pub fn clear(&self) {
        let now = Utc::now();
        let mut state = self.state.lock();
        let notes = std::mem::take(&mut state.store.notes);
        state.archive.extend(notes.into_iter().map(|note| Archived {
            key: note.key,
            text: note.text,
            reason: ArchiveReason::Cleared,
            archived_at: now,
        }));
        self.persist(&mut state);
    }

    #[must_use]
    pub fn archived(&self, key: Option<&str>, limit: usize) -> String {
        let state = self.state.lock();
        let matching: Vec<&Archived> = state
            .archive
            .iter()
            .filter(|entry| key.is_none_or(|key| entry.key == key.trim()))
            .collect();
        let skip = matching.len().saturating_sub(limit);
        let mut out = String::new();
        if skip > 0 {
            let _ = writeln!(out, "({skip} older archived notes not shown)");
        }
        for entry in &matching[skip..] {
            let _ = writeln!(
                out,
                "- [{}] {} {}: {}",
                entry.key,
                entry.reason.label(),
                entry.archived_at.format("%Y-%m-%d %H:%MZ"),
                entry.text
            );
        }
        out
    }

    pub fn delivery(&self, latest_in_context: Option<&str>) -> Option<Delivery> {
        let mut state = self.state.lock();
        let text = render(&state.store.notes);
        let current = crate::notes::fingerprint(&text);
        if latest_in_context == Some(current.as_str()) {
            return None;
        }
        if latest_in_context.is_none() && state.store.notes.is_empty() {
            return None;
        }
        let pairs = pairs(&state.store.notes);
        let update = state
            .store
            .shown
            .as_ref()
            .filter(|shown| latest_in_context == Some(shown.fingerprint.as_str()))
            .filter(|shown| shown.updates_since_full < UPDATES_BEFORE_FULL)
            .map(|shown| (shown.updates_since_full, changes(&shown.notes, &pairs)))
            .filter(|(_, changes)| changes.chars().count() < text.chars().count());
        let (delivery, updates_since_full) = match update {
            Some((updates, changes)) => (
                Delivery {
                    message: crate::notes::update_message(&current, &changes),
                    text: changes,
                },
                updates + 1,
            ),
            None => (
                Delivery {
                    message: crate::notes::message(&text),
                    text,
                },
                0,
            ),
        };
        state.store.shown = Some(Shown {
            fingerprint: current,
            notes: pairs,
            updates_since_full,
        });
        self.persist_store(&state.store);
        Some(delivery)
    }

    fn persist(&self, state: &mut State) {
        self.persist_store(&state.store);
        let Some(dir) = self.dir.lock().clone() else {
            return;
        };
        let mut lines = String::new();
        for entry in &state.archive[state.archive_written..] {
            if let Ok(line) = serde_json::to_string(entry) {
                lines.push_str(&line);
                lines.push('\n');
            }
        }
        state.archive_written = state.archive.len();
        if !lines.is_empty() {
            crate::persist::append_private(&dir.join(ARCHIVE_FILE), &lines);
        }
    }

    fn persist_store(&self, store: &Store) {
        let Some(dir) = self.dir.lock().clone() else {
            return;
        };
        crate::persist::write_json(&dir.join(STORE_FILE), store);
        let legacy = dir.join(LEGACY_FILE);
        if legacy.exists() {
            let _ = std::fs::remove_file(legacy);
        }
    }
}

impl Store {
    fn auto_key(&mut self) -> String {
        loop {
            self.next_auto += 1;
            let key = format!("{AUTO_KEY_PREFIX}{}", self.next_auto);
            if self.notes.iter().all(|note| note.key != key) {
                return key;
            }
        }
    }
}

impl State {
    fn make_room(
        &mut self,
        mut needed: usize,
        keeping: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, String> {
        if needed <= MAX_NOTES_CHARS {
            return Ok(Vec::new());
        }
        let mut candidates: Vec<(DateTime<Utc>, usize)> = self
            .store
            .notes
            .iter()
            .enumerate()
            .filter(|(_, note)| note.auto && note.key != keeping)
            .map(|(index, note)| (note.updated_at, index))
            .collect();
        candidates.sort();
        let mut evict = Vec::new();
        for (_, index) in candidates {
            if needed <= MAX_NOTES_CHARS {
                break;
            }
            needed -= size(&self.store.notes[index]);
            evict.push(index);
        }
        if needed > MAX_NOTES_CHARS {
            return Err(format!(
                "the notes are full ({MAX_NOTES_CHARS} characters) and only keyed notes are left; \
                 shorten a keyed note with rlm.note(text, key=...) or archive one with \
                 rlm.drop_note(key)"
            ));
        }
        evict.sort_unstable_by(|a, b| b.cmp(a));
        let mut made_room = Vec::with_capacity(evict.len());
        for index in evict {
            let note = self.store.notes.remove(index);
            made_room.push(note.key.clone());
            self.archive.push(Archived {
                key: note.key,
                text: note.text,
                reason: ArchiveReason::Overflow,
                archived_at: now,
            });
        }
        made_room.reverse();
        Ok(made_room)
    }
}

fn validate_key(key: &str) -> Result<String, String> {
    if key.is_empty() {
        return Err("a note key needs at least one character".into());
    }
    if key.chars().count() > MAX_KEY_CHARS {
        return Err(format!("a note key is at most {MAX_KEY_CHARS} characters"));
    }
    if let Some(bad) = key
        .chars()
        .find(|ch| !(ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':')))
    {
        return Err(format!(
            "a note key uses letters, digits and - _ . / : only, not {bad:?}"
        ));
    }
    Ok(key.to_string())
}

fn missing(key: &str, notes: &[Note]) -> String {
    if notes.is_empty() {
        return format!("there is no note [{key}]; there are no notes");
    }
    let keys: Vec<&str> = notes.iter().map(|note| note.key.as_str()).collect();
    format!("there is no note [{key}]; the keys are {}", keys.join(", "))
}

fn line(key: &str, text: &str) -> String {
    format!("- [{key}] {text}\n")
}

fn size(note: &Note) -> usize {
    line(&note.key, &note.text).chars().count()
}

fn total(notes: &[Note]) -> usize {
    notes.iter().map(size).sum()
}

fn render(notes: &[Note]) -> String {
    notes
        .iter()
        .map(|note| line(&note.key, &note.text))
        .collect()
}

fn pairs(notes: &[Note]) -> Vec<(String, String)> {
    notes
        .iter()
        .map(|note| (note.key.clone(), note.text.clone()))
        .collect()
}

fn changes(shown: &[(String, String)], current: &[(String, String)]) -> String {
    let before: HashMap<&str, &str> = shown
        .iter()
        .map(|(key, text)| (key.as_str(), text.as_str()))
        .collect();
    let now: HashMap<&str, &str> = current
        .iter()
        .map(|(key, text)| (key.as_str(), text.as_str()))
        .collect();
    let mut out = String::new();
    for (key, text) in current {
        if before.get(key.as_str()) != Some(&text.as_str()) {
            out.push_str(&line(key, text));
        }
    }
    for (key, _) in shown {
        if !now.contains_key(key.as_str()) {
            out.push_str(&line(
                key,
                "(removed; rlm.notes(archived=True) still has it)",
            ));
        }
    }
    out
}

fn load(dir: Option<&Path>) -> State {
    let Some(dir) = dir else {
        return State::default();
    };
    let store = crate::persist::load_json::<Store>(Some(&dir.join(STORE_FILE)))
        .unwrap_or_else(|| legacy(&dir.join(LEGACY_FILE)));
    let archive: Vec<Archived> = std::fs::read_to_string(dir.join(ARCHIVE_FILE))
        .map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        })
        .unwrap_or_default();
    State {
        store,
        archive_written: archive.len(),
        archive,
    }
}

fn legacy(path: &Path) -> Store {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Store::default();
    };
    let updated_at = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map_or_else(|_| Utc::now(), DateTime::<Utc>::from);
    let mut bodies: Vec<String> = Vec::new();
    for line in text.lines() {
        match (line.strip_prefix("- "), bodies.last_mut()) {
            (Some(start), _) => bodies.push(start.to_string()),
            (None, Some(body)) => {
                body.push('\n');
                body.push_str(line);
            }
            (None, None) => bodies.push(line.to_string()),
        }
    }
    let mut store = Store::default();
    for body in bodies {
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        let key = store.auto_key();
        store.notes.push(Note {
            key,
            text: body.to_string(),
            auto: true,
            updated_at,
        });
    }
    store
}

pub use mush_session::notes::{extract, fingerprint, latest_fingerprint, message, update_message};

#[cfg(test)]
mod tests {
    use super::*;

    fn body(delivery: &Delivery) -> String {
        let Message::User(user) = &delivery.message else {
            panic!("a notes carrier is a user message")
        };
        extract(&user.text()).unwrap().1
    }

    #[test]
    fn unkeyed_notes_get_auto_keys_and_keyed_notes_replace_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let notes = Notes::new(Some(dir.path().to_path_buf()));
        assert!(notes.read().is_empty());

        assert_eq!(notes.append("tests run under nextest").unwrap().key, "n1");
        notes.keep("phase one", Some("plan")).unwrap();
        notes.append("  the daemon lives in mush-daemon  ").unwrap();
        let kept = notes.keep("phase two", Some(" plan ")).unwrap();
        assert!(kept.replaced);
        assert_eq!(kept.count, 3);
        assert!(notes.append("   ").is_err());

        let text = notes.read();
        assert_eq!(
            text,
            "- [n1] tests run under nextest\n- [plan] phase two\n- [n2] the daemon lives in mush-daemon\n"
        );
        assert!(notes.archived(None, 10).contains("- [plan] replaced"));
        assert!(notes.archived(None, 10).ends_with(": phase one\n"));

        let reloaded = Notes::new(Some(dir.path().to_path_buf()));
        assert_eq!(reloaded.read(), text);
        assert_eq!(reloaded.append("next").unwrap().key, "n3");
        assert_eq!(reloaded.archived(Some("plan"), 10).lines().count(), 1);
    }

    #[test]
    fn keys_are_validated() {
        let notes = Notes::new(None);
        assert!(notes.keep("x", Some("")).is_err());
        assert!(notes.keep("x", Some("has space")).is_err());
        assert!(
            notes
                .keep("x", Some(&"k".repeat(MAX_KEY_CHARS + 1)))
                .is_err()
        );
        assert!(notes.keep("x", Some("state/fleet:mail-1.2_b")).is_ok());
        let error = notes
            .keep(&"x".repeat(MAX_NOTE_CHARS + 1), None)
            .unwrap_err();
        assert!(
            error.contains(&format!("{}", MAX_NOTE_CHARS + 1)),
            "{error}"
        );
    }

    #[test]
    fn dropping_and_clearing_archive_instead_of_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let notes = Notes::new(Some(dir.path().to_path_buf()));
        notes.keep("rules", Some("core")).unwrap();
        notes.append("a passing thought").unwrap();
        assert!(notes.drop_note("nope").unwrap_err().contains("core, n1"));
        notes.drop_note("n1").unwrap();
        assert_eq!(notes.read(), "- [core] rules\n");
        notes.clear();
        assert!(notes.read().is_empty());

        let reloaded = Notes::new(Some(dir.path().to_path_buf()));
        let archived = reloaded.archived(None, 10);
        assert!(archived.contains("- [n1] dropped"), "{archived}");
        assert!(archived.contains("- [core] cleared"), "{archived}");
        let newest = reloaded.archived(None, 1);
        assert!(
            newest.starts_with("(1 older archived notes not shown)"),
            "{newest}"
        );
        assert!(newest.contains("[core]"));
    }

    #[test]
    fn full_notes_archive_the_oldest_unkeyed_ones_but_never_a_keyed_one() {
        let notes = Notes::new(None);
        let chunk = MAX_NOTE_CHARS - 20;
        notes.keep(&"k".repeat(chunk), Some("keep")).unwrap();
        for _ in 0..4 {
            notes.append(&"y".repeat(chunk)).unwrap();
        }
        notes.keep(&"m".repeat(chunk), Some("mine")).unwrap();
        let kept = notes.append(&"z".repeat(chunk)).unwrap();
        assert_eq!(kept.made_room, ["n1"]);
        assert!(kept.to_string().contains("archived [n1] to make room"));
        assert!(kept.chars <= MAX_NOTES_CHARS);
        assert!(notes.read().contains("[keep]"));
        assert!(
            notes
                .archived(Some("n1"), 5)
                .contains("archived to make room")
        );

        let big = Notes::new(None);
        for key in ["a", "b", "c", "d", "e", "f"] {
            big.keep(&"q".repeat(chunk), Some(key)).unwrap();
        }
        let error = big.keep(&"q".repeat(chunk), Some("g")).unwrap_err();
        assert!(error.contains("full"), "{error}");
        assert!(big.keep(&"r".repeat(chunk), Some("a")).is_ok());
    }

    #[test]
    fn legacy_markdown_notes_load_as_unkeyed_notes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(LEGACY_FILE),
            "- CORE: rules\n- STATE: two\nlines\n",
        )
        .unwrap();
        let notes = Notes::new(Some(dir.path().to_path_buf()));
        assert_eq!(
            notes.read(),
            "- [n1] CORE: rules\n- [n2] STATE: two\nlines\n"
        );
        notes.keep("rules", Some("core")).unwrap();
        assert!(!dir.path().join(LEGACY_FILE).exists());
        let reloaded = Notes::new(Some(dir.path().to_path_buf()));
        assert!(reloaded.read().ends_with("- [core] rules\n"));
    }

    #[test]
    fn delivery_shows_the_whole_block_once_then_only_what_changed() {
        let notes = Notes::new(None);
        assert!(notes.delivery(None).is_none());
        notes.keep("phase one", Some("plan")).unwrap();
        notes.append("the socket path").unwrap();
        notes
            .keep(&"rules that stay put ".repeat(10), Some("core"))
            .unwrap();

        let full = notes.delivery(None).unwrap();
        assert!(body(&full).contains("- [plan] phase one\n- [n1] the socket path"));
        let mut context = vec![full.message];
        let latest = latest_fingerprint(&context);
        assert!(notes.delivery(latest.as_deref()).is_none());

        notes.keep("phase two", Some("plan")).unwrap();
        notes.drop_note("n1").unwrap();
        let update = notes.delivery(latest.as_deref()).unwrap();
        let changed = body(&update);
        assert!(changed.contains("- [plan] phase two"), "{changed}");
        assert!(changed.contains("- [n1] (removed"), "{changed}");
        assert!(!changed.contains("phase one"));
        assert!(!changed.contains("rules that stay put"));
        context.push(update.message);
        assert_eq!(
            latest_fingerprint(&context).as_deref(),
            Some(fingerprint(&notes.read()).as_str())
        );
        assert!(
            notes
                .delivery(latest_fingerprint(&context).as_deref())
                .is_none()
        );

        let after_compaction = notes.delivery(None).unwrap();
        assert!(body(&after_compaction).contains("- [plan] phase two\n- [core] rules"));
    }

    #[test]
    fn delivery_falls_back_to_the_whole_block_when_the_context_drifted() {
        let notes = Notes::new(None);
        notes.keep("one", Some("a")).unwrap();
        notes.keep("two", Some("b")).unwrap();
        let first = notes.delivery(None).unwrap();
        let mut latest = latest_fingerprint(&[first.message]);
        notes.keep("three", Some("a")).unwrap();
        let elsewhere = notes.delivery(Some("an older branch")).unwrap();
        assert!(body(&elsewhere).contains("- [b] two"));

        for round in 0..=UPDATES_BEFORE_FULL + 1 {
            notes.keep(&format!("round {round}"), Some("a")).unwrap();
            let delivered = notes.delivery(latest.as_deref()).unwrap();
            let whole = body(&delivered).contains("- [b] two");
            assert_eq!(
                whole,
                round == 0 || round == UPDATES_BEFORE_FULL + 1,
                "round {round}"
            );
            latest = latest_fingerprint(&[delivered.message]);
        }

        notes.clear();
        let cleared = notes.delivery(latest.as_deref()).unwrap();
        assert!(body(&cleared).contains("(no notes)") || body(&cleared).contains("(removed"));
    }

    #[test]
    fn the_notes_message_roundtrips_its_fingerprint() {
        let notes = Notes::new(None);
        notes.append("the daemon lives in mush-daemon").unwrap();
        let text = notes.read();
        let carrier = message(&text);
        let Message::User(user) = &carrier else {
            panic!("{carrier:?}")
        };
        let (found, body) = extract(&user.text()).unwrap();
        assert_eq!(found, fingerprint(&text));
        assert!(body.ends_with("- [n1] the daemon lives in mush-daemon"));
        assert_eq!(
            latest_fingerprint(&[carrier]).as_deref(),
            Some(fingerprint(&text).as_str())
        );
        assert_eq!(latest_fingerprint(&[]), None);
        assert_ne!(fingerprint(&text), fingerprint("something else"));
    }
}
