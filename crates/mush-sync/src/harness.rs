use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::engine::{Direction, Engine, Report, SyncError};
use crate::local::{SessionState, write_file};
use crate::protocol::{Commit, FileRef, Manifest, blob_aad, manifest_aad, now_ms};
use crate::remote::Committed;

pub const HARNESS: &str = "_harness";
const HARNESS_FILE: &str = "harness";
const KINDS: [&str; 4] = ["prompt", "memory", "skill", "subagent"];

fn table<'a>(state: &'a Value, section: &str, kind: &str) -> Option<&'a Map<String, Value>> {
    state.get(section)?.get(kind)?.as_object()
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or_default()
}

fn version(entry: &Value) -> u64 {
    entry.get("version").and_then(Value::as_u64).unwrap_or(0)
}

fn recency(entry: &Value) -> (&str, u64, String) {
    (text(entry, "updated_at"), version(entry), entry.to_string())
}

fn conflict_copy(id: &str, loser: &Value) -> (String, Value) {
    let tag = &blake3::hash(loser.to_string().as_bytes()).to_hex()[..8];
    let copy_id = format!("{id}-conflict-{tag}");
    let mut copy = loser.clone();
    if let Some(fields) = copy.as_object_mut() {
        let title = fields
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_owned();
        fields.insert("id".into(), Value::String(copy_id.clone()));
        fields.insert(
            "title".into(),
            Value::String(format!("{title} (conflicting edit)")),
        );
    }
    (copy_id, copy)
}

fn kinds(states: &[&Value]) -> Vec<String> {
    let mut found: BTreeSet<String> = KINDS.iter().map(|kind| (*kind).to_owned()).collect();
    for state in states {
        for section in ["entries", "deleted"] {
            if let Some(kinds) = state.get(section).and_then(Value::as_object) {
                found.extend(kinds.keys().cloned());
            }
        }
    }
    let mut kinds: Vec<String> = found.into_iter().collect();
    kinds.sort_by_key(|kind| {
        (
            KINDS
                .iter()
                .position(|known| known == kind)
                .unwrap_or(KINDS.len()),
            kind.clone(),
        )
    });
    kinds
}

#[must_use]
pub fn merge(ours: &Value, theirs: &Value, base: Option<&Value>) -> Option<Value> {
    if !ours.is_object() || !theirs.is_object() {
        return None;
    }
    let mut entries = Map::new();
    let mut deleted = Map::new();
    for kind in kinds(&[ours, theirs]) {
        let mut ids = BTreeSet::new();
        for state in [ours, theirs] {
            for section in ["entries", "deleted"] {
                if let Some(found) = table(state, section, &kind) {
                    ids.extend(found.keys().cloned());
                }
            }
        }
        let mut live = BTreeMap::new();
        let mut dead = BTreeMap::new();
        for id in ids {
            let mine = table(ours, "entries", &kind).and_then(|found| found.get(&id));
            let other = table(theirs, "entries", &kind).and_then(|found| found.get(&id));
            let before = base
                .and_then(|base| table(base, "entries", &kind))
                .and_then(|found| found.get(&id));
            let winner = match (mine, other) {
                (Some(mine), Some(other)) if mine == other => Some(mine.clone()),
                (Some(mine), Some(other)) => {
                    let (winner, loser) = if recency(mine) >= recency(other) {
                        (mine, other)
                    } else {
                        (other, mine)
                    };
                    let concurrent = match base {
                        Some(_) => before != Some(loser),
                        None => version(loser) >= version(winner),
                    };
                    if concurrent {
                        let (copy_id, copy) = conflict_copy(&id, loser);
                        live.entry(copy_id).or_insert(copy);
                    }
                    Some(winner.clone())
                }
                (one, other) => one.or(other).cloned(),
            };
            let tombstone = [ours, theirs]
                .into_iter()
                .filter_map(|state| table(state, "deleted", &kind)?.get(&id))
                .max_by(|left, right| text(left, "deleted_at").cmp(text(right, "deleted_at")))
                .cloned();
            match (winner, tombstone) {
                (Some(entry), Some(tombstone))
                    if text(&tombstone, "deleted_at") >= text(&entry, "updated_at") =>
                {
                    dead.insert(id, tombstone);
                }
                (Some(entry), _) => {
                    live.insert(id, entry);
                }
                (None, Some(tombstone)) => {
                    dead.insert(id, tombstone);
                }
                (None, None) => {}
            }
        }
        entries.insert(kind.clone(), Value::Object(live.into_iter().collect()));
        if !dead.is_empty() {
            deleted.insert(kind, Value::Object(dead.into_iter().collect()));
        }
    }
    let mut refinements: BTreeMap<(String, String), Value> = BTreeMap::new();
    for state in [ours, theirs] {
        for event in state
            .get("refinements")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let key = (
                text(event, "created_at").to_owned(),
                text(event, "id").to_owned(),
            );
            refinements.entry(key).or_insert_with(|| event.clone());
        }
    }
    let schema = [ours, theirs]
        .into_iter()
        .filter_map(|state| state.get("schema").and_then(Value::as_u64))
        .max()
        .unwrap_or(1);
    let mut merged = Map::new();
    merged.insert("schema".into(), Value::from(schema));
    merged.insert("entries".into(), Value::Object(entries));
    merged.insert(
        "refinements".into(),
        Value::Array(refinements.into_values().collect()),
    );
    if !deleted.is_empty() {
        merged.insert("deleted".into(), Value::Object(deleted));
    }
    if let Some(extra) = ours.as_object() {
        for (key, value) in extra {
            if !["schema", "entries", "refinements", "deleted"].contains(&key.as_str()) {
                merged.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
    }
    Some(Value::Object(merged))
}

#[must_use]
pub fn merge_bytes(ours: &[u8], theirs: &[u8], base: Option<&[u8]>) -> Option<Vec<u8>> {
    let parse = |bytes: &[u8]| serde_json::from_slice::<Value>(bytes).ok();
    let base = base.and_then(parse);
    let merged = merge(&parse(ours)?, &parse(theirs)?, base.as_ref())?;
    let mut bytes = serde_json::to_vec_pretty(&merged).ok()?;
    bytes.push(b'\n');
    Some(bytes)
}

#[must_use]
pub fn merge_lines(ours: &[u8], theirs: &[u8]) -> Vec<u8> {
    let known: BTreeSet<&[u8]> = theirs.split(|byte| *byte == b'\n').collect();
    let mut merged = theirs.to_vec();
    if !merged.is_empty() && !merged.ends_with(b"\n") {
        merged.push(b'\n');
    }
    for line in ours.split(|byte| *byte == b'\n') {
        if !line.is_empty() && !known.contains(line) {
            merged.extend_from_slice(line);
            merged.push(b'\n');
        }
    }
    merged
}

fn read_optional(path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

impl Engine {
    pub(crate) async fn reconcile_harness(
        &self,
        fed: Option<crate::engine::RemoteEntry>,
        direction: Direction,
        report: &mut Report,
    ) -> Result<(), SyncError> {
        let path = self.layout().harness.join("harness_state.json");
        let base_path = self.layout().state.join("harness.base.json");
        let sid = self.keys().sid(HARNESS);
        let mut state = SessionState::load(self.layout(), HARNESS)?.unwrap_or_default();
        let mut theirs = fed;
        if theirs.is_none() && state.remote_version > state.version {
            theirs = self.fetch_entry(&sid).await?;
        }
        for _ in 0..4 {
            if theirs
                .as_ref()
                .is_some_and(|entry| entry.head.version <= state.version)
            {
                theirs = None;
            }
            let ours = read_optional(&path)?;
            let base = read_optional(&base_path)?;
            let changed_here = ours.is_some() && ours != base;
            let Some(entry) = theirs.take() else {
                let Some(bytes) = ours.filter(|_| changed_here) else {
                    return Ok(());
                };
                match self.push_harness(&sid, &mut state, &bytes).await? {
                    None => {
                        write_file(&base_path, &bytes)?;
                        report.pushed.push(HARNESS.to_owned());
                        return Ok(());
                    }
                    Some(head) => {
                        theirs = self.open_head(head)?;
                        continue;
                    }
                }
            };
            if direction == Direction::PushOnly {
                state.remote_version = entry.head.version;
                state.save(self.layout(), HARNESS)?;
                return Ok(());
            }
            let Some(file) = entry.manifest.files.get(HARNESS_FILE) else {
                return Ok(());
            };
            let remote = self.fetch_blob(HARNESS, &sid, file).await?;
            let next = match &ours {
                Some(mine) if changed_here => merge_bytes(mine, &remote, base.as_deref())
                    .ok_or_else(|| {
                        SyncError::Io(std::io::Error::other(
                            "the harness state here is not readable json, left as it is",
                        ))
                    })?,
                _ => remote.clone(),
            };
            if read_optional(&path)? != ours {
                theirs = Some(entry);
                continue;
            }
            if ours.as_deref() != Some(next.as_slice()) {
                write_file(&path, &next)?;
                report.pulled.push(HARNESS.to_owned());
            }
            write_file(&base_path, &remote)?;
            state.version = entry.head.version;
            state.remote_version = entry.head.version;
            state.manifest = Some(entry.manifest);
            state.save(self.layout(), HARNESS)?;
        }
        Ok(())
    }

    async fn push_harness(
        &self,
        sid: &str,
        state: &mut SessionState,
        bytes: &[u8],
    ) -> Result<Option<crate::protocol::Head>, SyncError> {
        let blob = self.keys().blob_id(bytes);
        let sealed = self.keys().seal(&blob_aad(sid, &blob), bytes)?;
        self.remote().store_blob(sid, &blob, sealed).await?;
        let manifest = Manifest {
            session: HARNESS.to_owned(),
            writer: self.machine().to_owned(),
            written_at: now_ms(),
            files: [(
                HARNESS_FILE.to_owned(),
                FileRef {
                    blob: blob.clone(),
                    len: bytes.len() as u64,
                },
            )]
            .into(),
            ..Manifest::default()
        };
        let sealed = self.keys().seal(
            &manifest_aad(sid, state.version + 1),
            &serde_json::to_vec(&manifest)?,
        )?;
        let commit = Commit {
            expect: state.version,
            holder: self.machine().to_owned(),
            upload: "harness".to_owned(),
            generation: 0,
            seq: 0,
            blobs: vec![blob],
            manifest: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, sealed),
        };
        match self.remote().commit(sid, &commit).await? {
            Committed::Accepted(head) => {
                state.version = head.version;
                state.remote_version = head.version;
                state.manifest = Some(manifest);
                state.save(self.layout(), HARNESS)?;
                Ok(None)
            }
            Committed::Conflict(head) => Ok(Some(head)),
            Committed::Locked { .. } => Err(SyncError::Io(std::io::Error::other(
                "the shared harness state is locked on the server",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str, content: &str, version: u64, at: &str) -> Value {
        json!({
            "id": id, "kind": "memory", "title": id, "content": content, "path": "general",
            "scope": "global", "reference": {}, "arguments": {}, "metadata": {},
            "source": "agent", "created_at": "2026-10-01T00:00:00.000Z",
            "updated_at": at, "version": version
        })
    }

    fn state(memories: &[Value], deleted: Option<Value>) -> Value {
        let mut memory = Map::new();
        for entry in memories {
            memory.insert(text(entry, "id").to_owned(), entry.clone());
        }
        let mut state = json!({
            "schema": 1,
            "entries": {"prompt": {}, "memory": memory, "skill": {}, "subagent": {}},
            "refinements": []
        });
        if let Some(deleted) = deleted {
            state["deleted"] = deleted;
        }
        state
    }

    fn memories(state: &Value) -> Vec<(String, String)> {
        state["entries"]["memory"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(id, entry)| (id.clone(), text(entry, "content").to_owned()))
            .collect()
    }

    #[test]
    fn entries_added_on_each_machine_are_all_kept() {
        let base = state(&[], None);
        let dot = state(
            &[entry("a", "from dot", 1, "2026-10-02T00:00:00.000Z")],
            None,
        );
        let esme = state(
            &[entry("b", "from esme", 1, "2026-10-03T00:00:00.000Z")],
            None,
        );
        let merged = merge(&dot, &esme, Some(&base)).unwrap();
        assert_eq!(
            memories(&merged),
            [
                ("a".to_owned(), "from dot".to_owned()),
                ("b".to_owned(), "from esme".to_owned())
            ]
        );
        assert_eq!(merged, merge(&esme, &dot, Some(&base)).unwrap());
    }

    #[test]
    fn an_edit_on_one_side_wins_without_leaving_a_copy() {
        let old = entry("a", "old", 1, "2026-10-02T00:00:00.000Z");
        let new = entry("a", "new", 2, "2026-10-04T00:00:00.000Z");
        let base = state(std::slice::from_ref(&old), None);
        let merged = merge(&state(&[old], None), &state(&[new], None), Some(&base)).unwrap();
        assert_eq!(memories(&merged), [("a".to_owned(), "new".to_owned())]);
    }

    #[test]
    fn concurrent_edits_keep_the_newer_and_preserve_the_other() {
        let base = state(&[entry("a", "old", 1, "2026-10-02T00:00:00.000Z")], None);
        let dot = state(
            &[entry("a", "dot edit", 2, "2026-10-04T00:00:00.000Z")],
            None,
        );
        let esme = state(
            &[entry("a", "esme edit", 2, "2026-10-05T00:00:00.000Z")],
            None,
        );
        let merged = merge(&dot, &esme, Some(&base)).unwrap();
        let kept = memories(&merged);
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert_eq!(kept[0], ("a".to_owned(), "esme edit".to_owned()));
        assert!(kept[1].0.starts_with("a-conflict-"));
        assert_eq!(kept[1].1, "dot edit");
        let copy = &merged["entries"]["memory"][&kept[1].0];
        assert_eq!(text(copy, "title"), "a (conflicting edit)");
        assert_eq!(merge(&merged, &esme, Some(&base)).unwrap(), merged);
    }

    #[test]
    fn a_deletion_wins_over_an_older_copy_but_not_over_a_newer_edit() {
        let alive = entry("a", "alive", 1, "2026-10-02T00:00:00.000Z");
        let buried =
            json!({"memory": {"a": {"deleted_at": "2026-10-03T00:00:00.000Z", "version": 1}}});
        let base = state(std::slice::from_ref(&alive), None);
        let deleting = state(&[], Some(buried.clone()));
        let merged = merge(
            &deleting,
            &state(std::slice::from_ref(&alive), None),
            Some(&base),
        )
        .unwrap();
        assert!(memories(&merged).is_empty());
        assert_eq!(merged["deleted"], buried);

        let edited = entry("a", "edited later", 2, "2026-10-04T00:00:00.000Z");
        let merged = merge(&deleting, &state(&[edited], None), Some(&base)).unwrap();
        assert_eq!(
            memories(&merged),
            [("a".to_owned(), "edited later".to_owned())]
        );
        assert!(merged.get("deleted").is_none());
    }

    #[test]
    fn an_old_backup_adds_what_is_missing_and_never_rolls_entries_back() {
        let current = state(
            &[entry("a", "current", 3, "2026-10-12T00:00:00.000Z")],
            None,
        );
        let backup = state(
            &[
                entry("a", "older", 2, "2026-10-07T00:00:00.000Z"),
                entry("lost", "only in the backup", 1, "2026-10-06T00:00:00.000Z"),
            ],
            None,
        );
        let merged = merge(&current, &backup, None).unwrap();
        assert_eq!(
            memories(&merged),
            [
                ("a".to_owned(), "current".to_owned()),
                ("lost".to_owned(), "only in the backup".to_owned())
            ]
        );
    }

    #[test]
    fn refinements_from_both_sides_are_unioned_in_time_order() {
        let mut dot = state(&[], None);
        dot["refinements"] = json!([{"id": "r2", "created_at": "2026-10-02T00:00:00.000Z"}]);
        let mut esme = state(&[], None);
        esme["refinements"] = json!([
            {"id": "r1", "created_at": "2026-10-01T00:00:00.000Z"},
            {"id": "r2", "created_at": "2026-10-02T00:00:00.000Z"}
        ]);
        let merged = merge(&dot, &esme, None).unwrap();
        let ids: Vec<&str> = merged["refinements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| text(event, "id"))
            .collect();
        assert_eq!(ids, ["r1", "r2"]);
    }

    #[test]
    fn unreadable_json_is_never_merged() {
        assert!(merge_bytes(b"not json", b"{}", None).is_none());
    }

    #[test]
    fn appended_lines_from_both_sides_survive() {
        assert_eq!(
            merge_lines(b"a\nb\nmine\n", b"a\nb\ntheirs\n"),
            b"a\nb\ntheirs\nmine\n"
        );
    }
}
