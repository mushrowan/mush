use std::path::{Path, PathBuf};

use crate::local::{Layout, complete_lines, copy_durably, prefix_equal, write_file};
use crate::protocol::{now_ms, valid_session};

#[derive(Debug, Default)]
pub struct Imported {
    pub added: Vec<String>,
    pub advanced: Vec<String>,
    pub unchanged: Vec<String>,
    pub forked: Vec<(String, String)>,
    pub kept_beside: Vec<String>,
    pub busy: Vec<String>,
    pub harness: bool,
}

enum Relation {
    Equal,
    Ahead,
    Behind,
    Diverged,
}

fn updated_at(path: &Path) -> u64 {
    mush_session::load_meta(path).map_or(0, |meta| meta.updated_at.as_ms())
}

fn relate(ours: &Path, theirs: &Path) -> std::io::Result<Relation> {
    match (ours.exists(), theirs.exists()) {
        (false, false) => return Ok(Relation::Equal),
        (true, true) => {}
        _ => return Ok(Relation::Diverged),
    }
    let mine = complete_lines(ours, 0, std::fs::metadata(ours)?.len())?;
    let other = complete_lines(theirs, 0, std::fs::metadata(theirs)?.len())?;
    if !prefix_equal(ours, 0, theirs, mine.min(other))? {
        return Ok(Relation::Diverged);
    }
    Ok(match mine.cmp(&other) {
        std::cmp::Ordering::Equal => Relation::Equal,
        std::cmp::Ordering::Less => Relation::Ahead,
        std::cmp::Ordering::Greater => Relation::Behind,
    })
}

fn label_of(source: &Path) -> String {
    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c))
        .collect();
    if cleaned.is_empty() {
        "backup".to_owned()
    } else {
        cleaned
    }
}

fn keep_beside(layout: &Layout, id: &str, who: &str, from: &Path) -> std::io::Result<()> {
    copy_durably(
        from,
        &layout
            .sessions
            .join(format!("{id}.json.sync-conflict-{who}-{}", now_ms())),
    )
}

fn fork_in(
    layout: &Layout,
    id: &str,
    label: &str,
    meta: &Path,
    history: &Path,
) -> std::io::Result<Option<String>> {
    let bytes = std::fs::read(meta)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"import");
    hasher.update(id.as_bytes());
    hasher.update(label.as_bytes());
    hasher.update(&bytes);
    let fork = hasher.finalize().to_hex()[..16].to_owned();
    if layout.meta(&fork).exists() {
        return Ok(None);
    }
    if history.exists() {
        copy_durably(history, &layout.history(&fork))?;
    }
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if let Some(fields) = value
        .get_mut("meta")
        .and_then(serde_json::Value::as_object_mut)
    {
        let title = fields
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("untitled")
            .to_owned();
        fields.insert("id".into(), fork.clone().into());
        fields.insert("title".into(), format!("{title} (from {label})").into());
        fields.remove("parent");
    }
    write_file(&layout.meta(&fork), &serde_json::to_vec_pretty(&value)?)?;
    Ok(Some(fork))
}

fn sessions_in(source: &Path) -> std::io::Result<(PathBuf, Vec<String>)> {
    let dir = if source.join("sessions").is_dir() {
        source.join("sessions")
    } else {
        source.to_owned()
    };
    let mut ids: Vec<String> = std::fs::read_dir(&dir)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".json")?;
            let meta = mush_session::load_meta(&entry.path()).ok()?;
            (valid_session(id) && meta.id.as_str() == id).then(|| id.to_owned())
        })
        .collect();
    ids.sort();
    Ok((dir, ids))
}

pub fn import(layout: &Layout, source: &Path) -> std::io::Result<Imported> {
    let label = label_of(source);
    let mut imported = Imported::default();
    let (dir, ids) = sessions_in(source)?;
    std::fs::create_dir_all(&layout.sessions)?;
    let store = layout.store();
    for id in ids {
        let (meta, history) = (
            dir.join(format!("{id}.json")),
            dir.join(format!("{id}.history.jsonl")),
        );
        let Ok(_lease) = store.acquire(&mush_session::SessionId::from(id.as_str())) else {
            imported.busy.push(id);
            continue;
        };
        if !layout.meta(&id).exists() && !layout.history(&id).exists() {
            if history.exists() {
                copy_durably(&history, &layout.history(&id))?;
            }
            copy_durably(&meta, &layout.meta(&id))?;
            imported.added.push(id);
            continue;
        }
        let same_meta =
            std::fs::read(&meta)? == std::fs::read(layout.meta(&id)).unwrap_or_default();
        let theirs_newer = updated_at(&meta) > updated_at(&layout.meta(&id));
        match relate(&layout.history(&id), &history)? {
            Relation::Diverged => match fork_in(layout, &id, &label, &meta, &history)? {
                Some(fork) => imported.forked.push((id, fork)),
                None => imported.unchanged.push(id),
            },
            Relation::Ahead => {
                if !same_meta && !theirs_newer {
                    keep_beside(layout, &id, "here", &layout.meta(&id))?;
                    imported.kept_beside.push(id.clone());
                }
                copy_durably(&history, &layout.history(&id))?;
                copy_durably(&meta, &layout.meta(&id))?;
                imported.advanced.push(id);
            }
            Relation::Equal if !same_meta && theirs_newer => {
                keep_beside(layout, &id, "here", &layout.meta(&id))?;
                copy_durably(&meta, &layout.meta(&id))?;
                imported.kept_beside.push(id.clone());
                imported.advanced.push(id);
            }
            Relation::Equal | Relation::Behind if !same_meta && theirs_newer => {
                keep_beside(layout, &id, &label, &meta)?;
                imported.kept_beside.push(id.clone());
                imported.unchanged.push(id);
            }
            Relation::Equal | Relation::Behind => imported.unchanged.push(id),
        }
    }
    for candidate in [
        source.join("harness_state.json"),
        source.join("harness").join("harness_state.json"),
    ] {
        let Ok(theirs) = std::fs::read(&candidate) else {
            continue;
        };
        let target = layout.harness.join("harness_state.json");
        let merged = match std::fs::read(&target) {
            Ok(ours) => crate::harness::merge_bytes(&ours, &theirs, None).ok_or_else(|| {
                std::io::Error::other(format!(
                    "{} or the local harness state is not readable json",
                    candidate.display()
                ))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => theirs,
            Err(error) => return Err(error),
        };
        write_file(&target, &merged)?;
        imported.harness = true;
    }
    Ok(imported)
}
