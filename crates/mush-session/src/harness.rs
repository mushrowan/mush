use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};

pub const STATE_FILE: &str = "harness_state.json";
pub const HISTORY_FILE: &str = "refinements.jsonl";
const OVERVIEW_ENTRIES_PER_KIND: usize = 6;
const OVERVIEW_REFINEMENTS: usize = 5;
const OVERVIEW_CONTENT_CHARS: usize = 180;
const PLAN_ENTRIES_PER_KIND: usize = 40;
const PLAN_CONTENT_CHARS: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Prompt,
    Memory,
    Skill,
    Subagent,
}

impl Kind {
    pub const ALL: [Self; 4] = [Self::Prompt, Self::Memory, Self::Skill, Self::Subagent];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Memory => "memory",
            Self::Skill => "skill",
            Self::Subagent => "subagent",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().to_lowercase();
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Local,
    Global,
}

impl Scope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Create,
    Update,
    Delete,
}

impl Action {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }

    #[must_use]
    pub const fn past_tense(self) -> &'static str {
        match self {
            Self::Create => "created",
            Self::Update => "updated",
            Self::Delete => "deleted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub kind: Kind,
    pub title: String,
    pub content: String,
    pub path: String,
    #[serde(default = "default_scope")]
    pub scope: Scope,
    #[serde(default)]
    pub reference: Map<String, Value>,
    #[serde(default)]
    pub arguments: Map<String, Value>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
    pub version: u32,
}

fn default_scope() -> Scope {
    Scope::Global
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefinementEvent {
    pub id: String,
    pub trigger: String,
    pub changes: Vec<String>,
    pub evidence: String,
    pub outcome: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub deleted_at: String,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub schema: u32,
    pub entries: BTreeMap<Kind, BTreeMap<String, Entry>>,
    pub refinements: Vec<RefinementEvent>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deleted: BTreeMap<Kind, BTreeMap<String, Tombstone>>,
    #[serde(skip)]
    skipped: Skipped,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Skipped {
    entries: Vec<SkippedEntry>,
    refinements: Vec<SkippedRefinement>,
    unreadable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct SkippedEntry {
    kind: String,
    id: String,
    raw: Value,
    reason: String,
}

#[derive(Debug, Clone, PartialEq)]
struct SkippedRefinement {
    index: usize,
    raw: Value,
    reason: String,
}

impl SkippedEntry {
    fn line(&self) -> String {
        format!(
            "harness: skipped malformed entry {} ({})",
            compact(&self.id, OVERVIEW_CONTENT_CHARS),
            self.reason
        )
    }
}

impl SkippedRefinement {
    fn line(&self) -> String {
        let label = match &self.raw {
            Value::Object(fields) => match fields.get("id") {
                Some(Value::String(id)) => compact(id, OVERVIEW_CONTENT_CHARS),
                Some(other) => format!("a {} id", json_type(other)),
                None => "an undefined id".into(),
            },
            other => format!("a {}", json_type(other)),
        };
        format!(
            "harness: skipped malformed refinement event {label} ({})",
            self.reason
        )
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema: 1,
            entries: Kind::ALL
                .into_iter()
                .map(|kind| (kind, BTreeMap::new()))
                .collect(),
            refinements: Vec::new(),
            deleted: BTreeMap::new(),
            skipped: Skipped::default(),
        }
    }
}

impl State {
    pub fn load(dir: &Path, scope: Scope) -> Self {
        let Ok(bytes) = std::fs::read(dir.join(STATE_FILE)) else {
            return Self::default();
        };
        let mut state = match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(record)) => Self::from_record(record),
            Ok(other) => Self::unreadable(format!("a {} instead of an object", json_type(&other))),
            Err(error) => Self::unreadable(error.to_string()),
        };
        for entry in state.entries.values_mut().flat_map(BTreeMap::values_mut) {
            entry.scope = scope;
        }
        state
    }

    fn unreadable(reason: String) -> Self {
        let mut state = Self::default();
        state.skipped.unreadable.push(reason);
        state
    }

    fn from_record(mut record: Map<String, Value>) -> Self {
        let mut state = Self::default();
        if let Some(schema) = record
            .get("schema")
            .and_then(Value::as_u64)
            .and_then(|schema| u32::try_from(schema).ok())
        {
            state.schema = schema;
        }
        match record.remove("entries") {
            None => {}
            Some(Value::Object(kinds)) => {
                for (kind_key, entries) in kinds {
                    let Value::Object(entries) = entries else {
                        state
                            .skipped
                            .unreadable
                            .push(format!("entries.{kind_key} is not an object"));
                        continue;
                    };
                    let kind = serde_json::from_value::<Kind>(Value::String(kind_key.clone()));
                    for (id, raw) in entries {
                        let parsed = match &kind {
                            Ok(kind) => serde_json::from_value::<Entry>(raw.clone())
                                .map(|entry| (*kind, entry))
                                .map_err(|error| error.to_string()),
                            Err(_) => Err(format!("unknown kind {kind_key}")),
                        };
                        match parsed {
                            Ok((kind, entry)) => {
                                state.entries.entry(kind).or_default().insert(id, entry);
                            }
                            Err(reason) => state.skipped.entries.push(SkippedEntry {
                                kind: kind_key.clone(),
                                id,
                                raw,
                                reason,
                            }),
                        }
                    }
                }
            }
            Some(other) => state
                .skipped
                .unreadable
                .push(format!("entries is a {}", json_type(&other))),
        }
        if let Some(deleted) = record
            .remove("deleted")
            .and_then(|deleted| serde_json::from_value(deleted).ok())
        {
            state.deleted = deleted;
        }
        match record.remove("refinements") {
            None => {}
            Some(Value::Array(events)) => {
                for (index, raw) in events.into_iter().enumerate() {
                    match serde_json::from_value::<RefinementEvent>(raw.clone()) {
                        Ok(event) => state.refinements.push(event),
                        Err(error) => state.skipped.refinements.push(SkippedRefinement {
                            index,
                            raw,
                            reason: error.to_string(),
                        }),
                    }
                }
            }
            Some(other) => state
                .skipped
                .unreadable
                .push(format!("refinements is a {}", json_type(&other))),
        }
        state
    }

    fn skipped_lines(&self, kind: Option<Kind>) -> Vec<String> {
        self.skipped
            .entries
            .iter()
            .filter(|skipped| match kind {
                Some(kind) => skipped.kind == kind.as_str(),
                None => Kind::ALL.iter().all(|known| skipped.kind != known.as_str()),
            })
            .map(SkippedEntry::line)
            .collect()
    }

    fn diagnostics(&self) -> Vec<String> {
        self.skipped
            .unreadable
            .iter()
            .map(|reason| format!("harness: could not read {STATE_FILE} ({reason})"))
            .chain(self.skipped.entries.iter().map(SkippedEntry::line))
            .chain(self.skipped.refinements.iter().map(SkippedRefinement::line))
            .collect()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<PathBuf> {
        if let Some(reason) = self.skipped.unreadable.first() {
            return Err(std::io::Error::other(format!(
                "{STATE_FILE} could not be read ({reason}), so it was left as it is"
            )));
        }
        let mut value = serde_json::to_value(self).map_err(std::io::Error::other)?;
        if let Some(entries) = value.get_mut("entries").and_then(Value::as_object_mut) {
            for skipped in &self.skipped.entries {
                if let Some(kind) = entries
                    .entry(skipped.kind.clone())
                    .or_insert_with(|| Value::Object(Map::new()))
                    .as_object_mut()
                {
                    kind.entry(skipped.id.clone())
                        .or_insert_with(|| skipped.raw.clone());
                }
            }
        }
        if let Some(refinements) = value.get_mut("refinements").and_then(Value::as_array_mut) {
            for skipped in &self.skipped.refinements {
                let at = skipped.index.min(refinements.len());
                refinements.insert(at, skipped.raw.clone());
            }
        }
        std::fs::create_dir_all(dir)?;
        let path = dir.join(STATE_FILE);
        let tmp = dir.join(format!("{STATE_FILE}.tmp"));
        let mut json = serde_json::to_vec_pretty(&value).map_err(std::io::Error::other)?;
        json.push(b'\n');
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    #[must_use]
    pub fn merged(global: &Self, local: &Self) -> Self {
        let mut merged = Self {
            schema: global.schema.max(local.schema),
            ..Self::default()
        };
        for kind in Kind::ALL {
            let target = merged.entries.entry(kind).or_default();
            for (id, entry) in global.entries.get(&kind).into_iter().flatten() {
                let mut entry = entry.clone();
                entry.scope = Scope::Global;
                target.insert(id.clone(), entry);
            }
            for (id, entry) in local.entries.get(&kind).into_iter().flatten() {
                let mut entry = entry.clone();
                entry.scope = Scope::Local;
                let key = if target.contains_key(id) {
                    format!("local:{id}")
                } else {
                    id.clone()
                };
                target.insert(key, entry);
            }
        }
        merged.refinements = global
            .refinements
            .iter()
            .chain(&local.refinements)
            .cloned()
            .collect();
        for source in [global, local] {
            merged
                .skipped
                .entries
                .extend(source.skipped.entries.iter().cloned());
            merged
                .skipped
                .refinements
                .extend(source.skipped.refinements.iter().cloned());
            merged
                .skipped
                .unreadable
                .extend(source.skipped.unreadable.iter().cloned());
        }
        merged
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.values().all(BTreeMap::is_empty) && self.diagnostics().is_empty()
    }

    #[must_use]
    pub fn entry(&self, kind: Kind, id: &str) -> Option<&Entry> {
        self.entries.get(&kind)?.get(id)
    }

    #[must_use]
    pub fn list(&self, kind: Option<Kind>) -> Vec<&Entry> {
        Kind::ALL
            .into_iter()
            .filter(|candidate| kind.is_none_or(|wanted| wanted == *candidate))
            .flat_map(|candidate| self.entries.get(&candidate).into_iter().flatten())
            .map(|(_, entry)| entry)
            .collect()
    }

    pub fn upsert(&mut self, edit: &Edit, scope: Scope, source: &str) -> Result<Entry, String> {
        let id = edit.id.clone().unwrap_or_else(|| {
            slug(
                edit.title.as_deref().unwrap_or(edit.kind.as_str()),
                edit.kind.as_str(),
            )
        });
        if let Some(error) = validate(edit, &id) {
            return Err(error);
        }
        let before = self.entry(edit.kind, &id).cloned();
        match (edit.action, &before) {
            (Action::Create, Some(_)) => return Err(format!("{id} already exists")),
            (Action::Update, None) => return Err(format!("{id} not found")),
            _ => {}
        }
        let after = merge_entry(edit, &id, before.as_ref(), scope, source);
        self.revive(edit.kind, &id);
        self.entries
            .entry(edit.kind)
            .or_default()
            .insert(id, after.clone());
        Ok(after)
    }

    pub fn remove(&mut self, kind: Kind, id: &str) -> Result<Entry, String> {
        let removed = self
            .entries
            .entry(kind)
            .or_default()
            .remove(id)
            .ok_or_else(|| format!("{id} not found"))?;
        self.bury(kind, id, removed.version);
        Ok(removed)
    }

    fn bury(&mut self, kind: Kind, id: &str, version: u32) {
        self.deleted.entry(kind).or_default().insert(
            id.to_owned(),
            Tombstone {
                deleted_at: now(),
                version,
            },
        );
    }

    fn revive(&mut self, kind: Kind, id: &str) {
        if let Some(buried) = self.deleted.get_mut(&kind) {
            buried.remove(id);
            if buried.is_empty() {
                self.deleted.remove(&kind);
            }
        }
    }

    #[must_use]
    pub fn search(&self, query: &str, kind: Option<Kind>, limit: usize) -> Vec<&Entry> {
        let terms = query_terms(query);
        let entries = self.list(kind);
        if terms.is_empty() || entries.is_empty() {
            return Vec::new();
        }
        let haystacks: Vec<Haystack> = entries.iter().map(|entry| Haystack::of(entry)).collect();
        let weighted: Vec<(String, f64)> = terms.into_iter().map(|term| (term, 1.0)).collect();
        let idf = term_idf(&haystacks, &weighted);
        let mut ranked: Vec<(f64, &Entry)> = entries
            .iter()
            .zip(&haystacks)
            .filter_map(|(entry, haystack)| {
                let score = haystack.score(&idf);
                (score > 0.0).then_some((score, *entry))
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| b.1.updated_at.cmp(&a.1.updated_at))
        });
        ranked
            .into_iter()
            .take(limit)
            .map(|(_, entry)| entry)
            .collect()
    }

    #[must_use]
    pub fn digest(&self, focus: &Focus) -> String {
        let mut lines = vec![
            "# continual harness state".to_string(),
            String::new(),
            "local entries belong to this session, global entries persist across sessions. \
             these are compact summaries, use them as routing hints and read the whole entry \
             with `await harness.get('<kind>', '<id>')` when the detail matters. \
             `await harness.search(query)` ranks them. the base system prompt is immutable, \
             prompt entries are supplemental notes."
                .to_string(),
            "default to local refinement for current task progress, temporary blockers and \
             session coordination. use global only for stable cross-session lessons, durable \
             user preferences, reusable skills and subagents, or explicitly project-qualified \
             facts."
                .to_string(),
            String::new(),
            "call `await refine.run()` after a repeated failure, when a reusable tactic emerges, \
             when a repeated delegation role should become a subagent spec, a repeated procedure \
             a skill, a durable fact or preference a memory, a narrow behavioural policy a prompt \
             note, when the user corrects behaviour that should persist, or when an entry proves \
             wrong. keep edits small and evidence-backed. skill entries are python calls with a \
             `reference` and `arguments` contract, subagent specs are spawned with \
             `await rlm.spawn('<task>', name='<worker>')`."
                .to_string(),
            String::new(),
        ];
        for reason in &self.skipped.unreadable {
            lines.push(format!("harness: could not read {STATE_FILE} ({reason})"));
        }
        let mut total = self.skipped.entries.len();
        for kind in Kind::ALL {
            let entries = self.entries.get(&kind).map(BTreeMap::len).unwrap_or(0);
            total += entries;
            lines.push(format!("{}: {entries}", kind.as_str()));
            if !focus.is_empty() && entries > OVERVIEW_ENTRIES_PER_KIND {
                lines.push(
                    "(ranked by relevance to the current task, `await harness.search(query)` \
                     finds the rest)"
                        .to_string(),
                );
            }
            let ranked = focus.rank(self.entries[&kind].values().collect());
            for entry in ranked.iter().take(OVERVIEW_ENTRIES_PER_KIND) {
                lines.push(entry_line(entry, OVERVIEW_CONTENT_CHARS));
            }
            lines.extend(self.skipped_lines(Some(kind)));
            if entries > OVERVIEW_ENTRIES_PER_KIND {
                lines.push(format!(
                    "- +{} more {} entries",
                    entries - OVERVIEW_ENTRIES_PER_KIND,
                    kind.as_str()
                ));
            }
            lines.push(String::new());
        }
        let unknown = self.skipped_lines(None);
        if !unknown.is_empty() {
            lines.extend(unknown);
            lines.push(String::new());
        }
        if total == 0 {
            lines.push("no saved harness entries yet.".into());
            lines.push(String::new());
        }
        lines.push(format!("recent refinements: {}", self.refinements.len()));
        let skip = self.refinements.len().saturating_sub(OVERVIEW_REFINEMENTS);
        for event in self.refinements.iter().skip(skip) {
            let changes = if event.changes.is_empty() {
                "no applied edits".to_string()
            } else {
                event.changes.join(", ")
            };
            let outcome = if event.outcome.is_empty() {
                String::new()
            } else {
                format!(
                    "; outcome: {}",
                    compact(&event.outcome, OVERVIEW_CONTENT_CHARS)
                )
            };
            lines.push(format!(
                "- [{}] {}: {changes}{outcome}",
                event.id,
                compact(&event.trigger, OVERVIEW_CONTENT_CHARS)
            ));
        }
        if skip > 0 {
            lines.push(format!("- +{skip} older refinement events"));
        }
        lines.extend(self.skipped.refinements.iter().map(SkippedRefinement::line));
        lines.join("\n").trim().to_string()
    }

    #[must_use]
    pub fn fingerprint(&self) -> String {
        let mut entries: Vec<_> = Kind::ALL
            .into_iter()
            .flat_map(|kind| self.entries[&kind].iter())
            .map(|(id, entry)| {
                let skill = entry.kind == Kind::Skill;
                let key = (entry.scope.as_str(), entry.kind.as_str(), id.as_str());
                let material = serde_json::json!({
                    "arguments": skill.then_some(&entry.arguments),
                    "content": entry.content,
                    "id": id,
                    "kind": entry.kind.as_str(),
                    "path": entry.path,
                    "reference": skill.then_some(&entry.reference),
                    "scope": entry.scope.as_str(),
                    "title": entry.title,
                    "version": entry.version,
                });
                (key, material)
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let entries: Vec<Value> = entries.into_iter().map(|(_, material)| material).collect();
        let refinements: Vec<Value> = self
            .refinements
            .iter()
            .map(|event| {
                serde_json::json!({
                    "changes": event.changes,
                    "id": event.id,
                    "outcome": event.outcome,
                    "trigger": event.trigger,
                })
            })
            .collect();
        let mut material = serde_json::json!({ "entries": entries, "refinements": refinements });
        let diagnostics = self.diagnostics();
        if !diagnostics.is_empty() {
            material["skipped"] = serde_json::json!(diagnostics);
        }
        sha256_hex(material.to_string().as_bytes())
    }

    #[must_use]
    pub fn overview_for_plan(&self) -> String {
        let mut lines = Vec::new();
        for kind in Kind::ALL {
            let entries = &self.entries[&kind];
            lines.push(format!("{}: {}", kind.as_str(), entries.len()));
            for entry in entries.values().take(PLAN_ENTRIES_PER_KIND) {
                lines.push(entry_line(entry, PLAN_CONTENT_CHARS));
            }
            lines.extend(
                self.skipped_lines(Some(kind))
                    .into_iter()
                    .map(|line| format!("- {line}")),
            );
            if entries.len() > PLAN_ENTRIES_PER_KIND {
                lines.push(format!(
                    "- +{} more {} entries",
                    entries.len() - PLAN_ENTRIES_PER_KIND,
                    kind.as_str()
                ));
            }
        }
        lines.join("\n")
    }

    pub fn apply(
        &mut self,
        proposal: &Proposal,
        id: &str,
        scope: Scope,
        rollback_of: Option<&str>,
    ) -> RefinementResult {
        let mut applied = Vec::new();
        for edit in &proposal.edits {
            let entry_id = edit.id.clone().unwrap_or_else(|| match edit.action {
                Action::Create => slug(
                    edit.title.as_deref().unwrap_or(edit.kind.as_str()),
                    edit.kind.as_str(),
                ),
                _ => String::new(),
            });
            if let Some(error) = validate(edit, &entry_id).or_else(|| validate_update(edit)) {
                applied.push(AppliedEdit::failed(edit, entry_id, None, error));
                continue;
            }
            let before = self.entry(edit.kind, &entry_id).cloned();
            match (edit.action, &before) {
                (Action::Delete, None) | (Action::Update, None) => {
                    applied.push(AppliedEdit::failed(
                        edit,
                        entry_id,
                        None,
                        "entry not found".into(),
                    ));
                }
                (Action::Create, Some(_)) => {
                    applied.push(AppliedEdit::failed(
                        edit,
                        entry_id,
                        before,
                        "entry already exists".into(),
                    ));
                }
                (Action::Delete, Some(gone)) => {
                    let version = gone.version;
                    self.entries.entry(edit.kind).or_default().remove(&entry_id);
                    self.bury(edit.kind, &entry_id, version);
                    applied.push(AppliedEdit {
                        edit: edit.clone(),
                        id: entry_id,
                        before,
                        after: None,
                        applied: true,
                        error: None,
                    });
                }
                (Action::Create | Action::Update, _) => {
                    let after = merge_entry(edit, &entry_id, before.as_ref(), scope, "refine");
                    self.revive(edit.kind, &entry_id);
                    self.entries
                        .entry(edit.kind)
                        .or_default()
                        .insert(entry_id.clone(), after.clone());
                    applied.push(AppliedEdit {
                        edit: edit.clone(),
                        id: entry_id,
                        before,
                        after: Some(after),
                        applied: true,
                        error: None,
                    });
                }
            }
        }
        let changes = applied
            .iter()
            .filter(|edit| edit.applied)
            .map(|edit| {
                format!(
                    "{} {}:{}",
                    edit.edit.action.as_str(),
                    edit.edit.kind.as_str(),
                    edit.id
                )
            })
            .collect();
        self.refinements.push(RefinementEvent {
            id: id.to_string(),
            trigger: proposal.summary.clone(),
            changes,
            evidence: proposal.rationale.clone(),
            outcome: proposal.expected_outcome.clone(),
            created_at: now(),
        });
        RefinementResult {
            id: id.to_string(),
            summary: proposal.summary.clone(),
            rationale: proposal.rationale.clone(),
            expected_outcome: proposal.expected_outcome.clone(),
            applied_edits: applied,
            scope,
            rollback_of: rollback_of.map(str::to_string),
        }
    }
}

fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{3040}'..='\u{30ff}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{f900}'..='\u{faff}'
            | '\u{ac00}'..='\u{d7af}'
            | '\u{20000}'..='\u{2a6df}'
            | '\u{2a700}'..='\u{2ee5f}'
            | '\u{2f800}'..='\u{2fa1f}'
            | '\u{30000}'..='\u{3347f}'
    )
}

fn query_runs(text: &str) -> Vec<(bool, String)> {
    let mut runs: Vec<(bool, String)> = Vec::new();
    let mut run = String::new();
    let mut run_is_cjk = false;
    for c in text.chars() {
        if c.is_alphanumeric() || c.general_category_group() == GeneralCategoryGroup::Mark {
            let cjk = is_cjk(c);
            if !run.is_empty() && cjk != run_is_cjk {
                runs.push((run_is_cjk, std::mem::take(&mut run)));
            }
            run_is_cjk = cjk;
            run.push(c);
        } else if !run.is_empty() {
            runs.push((run_is_cjk, std::mem::take(&mut run)));
        }
    }
    if !run.is_empty() {
        runs.push((run_is_cjk, run));
    }
    runs
}

const FOCUS_TEXTS: usize = 4;
const FOCUS_TERMS: usize = 48;
const FOCUS_TERM_CHARS: usize = 4;
const FOCUS_TEXT_CHARS: usize = 4_000;
const GOAL_WEIGHT: f64 = 3.0;
const NEWEST_TEXT_WEIGHT: f64 = 2.0;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Focus {
    terms: Vec<(String, f64)>,
}

impl Focus {
    #[must_use]
    pub fn new<'a>(goal: Option<&str>, newest_first: impl IntoIterator<Item = &'a str>) -> Self {
        let mut focus = Self::default();
        focus.add(goal.unwrap_or_default(), GOAL_WEIGHT);
        let mut weight = NEWEST_TEXT_WEIGHT;
        for text in newest_first.into_iter().take(FOCUS_TEXTS) {
            focus.add(text, weight);
            weight = (weight - 0.5).max(1.0);
        }
        focus
    }

    #[must_use]
    pub fn recent_texts(messages: &[mush_ai::types::Message]) -> Vec<String> {
        use mush_ai::types::Message;
        messages
            .iter()
            .rev()
            .filter_map(|message| match message {
                Message::User(user) => Some(user.text()),
                Message::Assistant(assistant) => Some(assistant.text()),
                Message::Event(event) => Some(event.content.clone()),
                Message::ToolResult(_) => None,
            })
            .filter(|text| {
                !text.trim().is_empty()
                    && !crate::compact::is_regenerated_text(text)
                    && !is_refinement_notice(text)
            })
            .take(FOCUS_TEXTS)
            .collect()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    fn add(&mut self, text: &str, weight: f64) {
        let text: String = text.chars().take(FOCUS_TEXT_CHARS).collect();
        for term in query_terms(&text) {
            if term.chars().count() < FOCUS_TERM_CHARS && !term.chars().any(is_cjk) {
                continue;
            }
            if self.terms.iter().any(|(known, _)| *known == term) {
                continue;
            }
            if self.terms.len() >= FOCUS_TERMS {
                return;
            }
            self.terms.push((term, weight));
        }
    }

    fn rank<'a>(&self, mut entries: Vec<&'a Entry>) -> Vec<&'a Entry> {
        let by_path =
            |a: &&Entry, b: &&Entry| (&a.path, &a.title, &a.id).cmp(&(&b.path, &b.title, &b.id));
        if self.is_empty() {
            entries.sort_by(by_path);
            return entries;
        }
        let haystacks: Vec<Haystack> = entries.iter().map(|entry| Haystack::of(entry)).collect();
        let idf = term_idf(&haystacks, &self.terms);
        let mut scored: Vec<(f64, &Entry)> = haystacks
            .iter()
            .map(|haystack| haystack.score(&idf))
            .zip(entries)
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| by_path(&a.1, &b.1)));
        scored.into_iter().map(|(_, entry)| entry).collect()
    }
}

struct Haystack {
    title: String,
    content: String,
    identifier: String,
}

impl Haystack {
    fn of(entry: &Entry) -> Self {
        Self {
            title: entry.title.to_lowercase(),
            content: entry.content.to_lowercase(),
            identifier: format!("{} {}", entry.path, entry.id).to_lowercase(),
        }
    }

    fn fields(&self, term: &str) -> u32 {
        u32::from(self.title.contains(term))
            + u32::from(self.content.contains(term))
            + u32::from(self.identifier.contains(term))
    }

    fn score(&self, weighted_idf: &[(&str, f64)]) -> f64 {
        weighted_idf
            .iter()
            .map(|(term, weight)| match self.fields(term) {
                0 => 0.0,
                found => weight * (1.0 + f64::from(found - 1) * 0.5),
            })
            .sum()
    }
}

fn term_idf<'a>(haystacks: &[Haystack], terms: &'a [(String, f64)]) -> Vec<(&'a str, f64)> {
    terms
        .iter()
        .filter_map(|(term, weight)| {
            let documents = haystacks
                .iter()
                .filter(|haystack| haystack.fields(term) > 0)
                .count();
            (documents > 0).then(|| {
                let idf = (1.0 + haystacks.len() as f64 / documents as f64).ln();
                (term.as_str(), weight * idf)
            })
        })
        .collect()
}

fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for (cjk, run) in query_runs(&query.to_lowercase()) {
        let chars: Vec<char> = run.chars().collect();
        let candidates: Vec<String> = if cjk {
            if chars.len() == 1 {
                vec![run]
            } else {
                chars.windows(2).map(|pair| pair.iter().collect()).collect()
            }
        } else {
            let floor = if run.is_ascii() { 3 } else { 2 };
            if chars.len() >= floor {
                vec![run]
            } else {
                Vec::new()
            }
        };
        for term in candidates {
            if !terms.contains(&term) {
                terms.push(term);
            }
        }
    }
    terms
}

fn merge_entry(edit: &Edit, id: &str, before: Option<&Entry>, scope: Scope, source: &str) -> Entry {
    Entry {
        id: id.to_string(),
        kind: edit.kind,
        title: edit
            .title
            .clone()
            .or_else(|| before.map(|b| b.title.clone()))
            .unwrap_or_else(|| id.to_string()),
        content: edit
            .content
            .clone()
            .or_else(|| before.map(|b| b.content.clone()))
            .unwrap_or_default(),
        path: edit
            .path
            .clone()
            .or_else(|| before.map(|b| b.path.clone()))
            .unwrap_or_else(|| "general".into()),
        scope: before.map_or(scope, |b| b.scope),
        reference: edit
            .reference
            .clone()
            .or_else(|| before.map(|b| b.reference.clone()))
            .unwrap_or_default(),
        arguments: edit
            .arguments
            .clone()
            .or_else(|| before.map(|b| b.arguments.clone()))
            .unwrap_or_default(),
        metadata: edit
            .metadata
            .clone()
            .or_else(|| before.map(|b| b.metadata.clone()))
            .unwrap_or_default(),
        source: source.to_string(),
        created_at: before.map_or_else(now, |b| b.created_at.clone()),
        updated_at: now(),
        version: before.map_or(1, |b| b.version + 1),
    }
}

fn entry_line(entry: &Entry, content_chars: usize) -> String {
    let skill_field = |label: &str, fields: &Map<String, Value>| {
        if entry.kind == Kind::Skill && !fields.is_empty() {
            let json = serde_json::to_string(fields).unwrap_or_default();
            format!(" {label}={}", compact(&json, content_chars))
        } else {
            String::new()
        }
    };
    format!(
        "- [{}:{}] {} ({}, v{}){}{}: {}",
        entry.scope.as_str(),
        entry.id,
        entry.title,
        entry.path,
        entry.version,
        skill_field("ref", &entry.reference),
        skill_field("args", &entry.arguments),
        compact(&entry.content, content_chars)
    )
}

fn compact(text: &str, max: usize) -> String {
    let normalised = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalised.chars().count() <= max {
        return normalised;
    }
    let head: String = normalised.chars().take(max.saturating_sub(3)).collect();
    format!("{head}...")
}

fn slug(raw: &str, fallback: &str) -> String {
    let mut out = String::new();
    let mut last_underscore = true;
    for c in raw.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    let out: String = out.trim_matches('_').chars().take(80).collect();
    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[must_use]
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edit {
    pub action: Action,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    #[serde(default = "default_summary")]
    pub summary: String,
    #[serde(default)]
    pub rationale: String,
    #[serde(default, alias = "expectedOutcome")]
    pub expected_outcome: String,
    #[serde(default)]
    pub edits: Vec<Edit>,
}

fn default_summary() -> String {
    "refined the continual harness".into()
}

impl Proposal {
    pub fn parse(text: &str) -> Result<Self, String> {
        let candidate =
            json_object(text).ok_or_else(|| "the refiner returned no json object".to_string())?;
        let value: Value = serde_json::from_str(candidate).map_err(|error| {
            if incomplete_json(candidate) {
                "the model stopped before completing its json object. retry with a smaller request"
                    .to_string()
            } else {
                format!("refiner json did not parse: {error}")
            }
        })?;
        let Value::Object(mut record) = value else {
            return Err("refiner json must be an object".into());
        };
        if let Some(edits) = record.get_mut("edits").and_then(Value::as_array_mut) {
            edits.retain_mut(normalise_edit);
        }
        let mut proposal: Self = serde_json::from_value(Value::Object(record))
            .map_err(|error| format!("refiner json has the wrong shape: {error}"))?;
        for edit in &mut proposal.edits {
            if edit.title.as_deref().is_some_and(str::is_empty) {
                edit.title = None;
            }
            if edit.content.as_deref().is_some_and(str::is_empty) {
                edit.content = None;
            }
        }
        Ok(proposal)
    }

    #[must_use]
    pub fn rollback_of(target: &RefinementResult) -> Self {
        let mut edits = Vec::new();
        for applied in target
            .applied_edits
            .iter()
            .rev()
            .filter(|edit| edit.applied)
        {
            match (&applied.before, &applied.after) {
                (Some(before), after) => edits.push(Edit {
                    action: if after.is_some() {
                        Action::Update
                    } else {
                        Action::Create
                    },
                    kind: applied.edit.kind,
                    id: Some(applied.id.clone()),
                    title: Some(before.title.clone()),
                    content: Some(before.content.clone()),
                    path: Some(before.path.clone()),
                    reference: Some(before.reference.clone()),
                    arguments: Some(before.arguments.clone()),
                    metadata: Some(before.metadata.clone()),
                    reason: Some(format!("rollback {}", target.id)),
                }),
                (None, Some(_)) => edits.push(Edit {
                    action: Action::Delete,
                    kind: applied.edit.kind,
                    id: Some(applied.id.clone()),
                    title: None,
                    content: None,
                    path: None,
                    reference: None,
                    arguments: None,
                    metadata: None,
                    reason: Some(format!("rollback {}", target.id)),
                }),
                (None, None) => {}
            }
        }
        Self {
            summary: format!("rollback refinement {}", target.id),
            rationale: format!(
                "restores the harness snapshots recorded by refinement {}",
                target.id
            ),
            expected_outcome: "the faulty refinement edits are reverted".into(),
            edits,
        }
    }
}

fn normalise_edit(edit: &mut Value) -> bool {
    let Some(fields) = edit.as_object_mut() else {
        return false;
    };
    for key in ["id", "title", "content", "path", "reason"] {
        if fields.get(key).is_some_and(|value| !value.is_string()) {
            fields.remove(key);
        }
    }
    for key in ["reference", "arguments", "metadata"] {
        if fields.get(key).is_some_and(|value| !value.is_object()) {
            fields.remove(key);
        }
    }
    let known = |key: &str, parse: fn(&str) -> bool| {
        fields.get(key).and_then(Value::as_str).is_some_and(parse)
    };
    let supported = known("action", |text| {
        serde_json::from_value::<Action>(Value::String(text.into())).is_ok()
    }) && known("kind", |text| {
        serde_json::from_value::<Kind>(Value::String(text.into())).is_ok()
    });
    if !supported {
        tracing::warn!(edit = %edit, "dropping a refiner edit with an unsupported action or kind");
    }
    supported
}

fn json_object(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let trimmed = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|rest| rest.trim_end_matches("```").trim())
        .unwrap_or(trimmed);
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}').unwrap_or(trimmed.len() - 1);
    (end >= start).then(|| &trimmed[start..=end])
}

fn incomplete_json(candidate: &str) -> bool {
    let mut depth: i64 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in candidate.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            _ => {}
        }
    }
    in_string || depth > 0
}

fn validate(edit: &Edit, id: &str) -> Option<String> {
    let action = edit.action.as_str();
    if edit.kind == Kind::Prompt && id == "base_system_prompt" {
        return Some("the base system prompt is not editable".into());
    }
    if edit.action != Action::Create && id.is_empty() {
        return Some(format!("{action} requires an id"));
    }
    if edit.id.as_deref() == Some("") {
        return Some(format!(
            "{action} requires id to be a non-empty string when provided"
        ));
    }
    if edit.path.as_deref() == Some("") {
        return Some(format!(
            "{action} requires path to be a non-empty string when provided"
        ));
    }
    if edit.action == Action::Delete {
        return None;
    }
    let creating = edit.action == Action::Create;
    if creating && (edit.title.is_none() || edit.content.is_none()) {
        return Some(format!("{action} requires title and content"));
    }
    if edit.kind == Kind::Skill {
        if creating && edit.arguments.is_none() {
            return Some(format!("{action} skill requires arguments"));
        }
        let Some(reference) = &edit.reference else {
            if creating {
                return Some(format!("{action} skill requires a python reference"));
            }
            return None;
        };
        if reference.get("type").and_then(Value::as_str) != Some("python") {
            return Some(format!("{action} skill reference.type must be python"));
        }
        let non_empty = |key: &str| {
            reference
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        };
        if !non_empty("import") && !non_empty("python_import") {
            return Some(format!("{action} skill requires a python import"));
        }
        if !non_empty("callable") && !non_empty("call_pattern") {
            return Some(format!("{action} skill requires callable or call_pattern"));
        }
    }
    None
}

fn validate_update(edit: &Edit) -> Option<String> {
    if edit.action != Action::Update {
        return None;
    }
    let blank = |field: &Option<String>| field.as_deref().is_none_or(str::is_empty);
    if blank(&edit.title) || blank(&edit.content) {
        return Some("update requires title and content".into());
    }
    if edit.kind == Kind::Skill {
        if edit.arguments.is_none() {
            return Some("update skill requires arguments".into());
        }
        if edit.reference.is_none() {
            return Some("update skill requires a python reference".into());
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Map<String, Value>")]
pub struct AppliedEdit {
    #[serde(flatten)]
    pub edit: Edit,
    #[serde(rename = "entry_id")]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Entry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Entry>,
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct StoredAppliedEdit {
    #[serde(flatten)]
    edit: Edit,
    #[serde(default)]
    entry_id: Option<String>,
    #[serde(default)]
    before: Option<Entry>,
    #[serde(default)]
    after: Option<Entry>,
    applied: bool,
    #[serde(default)]
    error: Option<String>,
}

impl TryFrom<Map<String, Value>> for AppliedEdit {
    type Error = serde_json::Error;

    fn try_from(map: Map<String, Value>) -> Result<Self, Self::Error> {
        let stored: StoredAppliedEdit = serde_json::from_value(Value::Object(map))?;
        let id = stored
            .entry_id
            .or_else(|| stored.edit.id.clone())
            .unwrap_or_default();
        Ok(Self {
            edit: stored.edit,
            id,
            before: stored.before,
            after: stored.after,
            applied: stored.applied,
            error: stored.error,
        })
    }
}

impl AppliedEdit {
    fn failed(edit: &Edit, id: String, before: Option<Entry>, error: String) -> Self {
        Self {
            edit: edit.clone(),
            id,
            before,
            after: None,
            applied: false,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefinementSource {
    Auto,
    #[default]
    User,
    #[serde(rename = "self")]
    Agent,
}

impl RefinementSource {
    pub const ALL: [Self; 3] = [Self::Auto, Self::User, Self::Agent];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::User => "user",
            Self::Agent => "self",
        }
    }

    #[must_use]
    pub fn notice_label(self) -> String {
        format!("[{}-refinement]", self.as_str())
    }
}

#[must_use]
pub fn is_refinement_notice(text: &str) -> bool {
    RefinementSource::ALL
        .into_iter()
        .any(|source| text.starts_with(&format!("{}\n\n", source.notice_label())))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefinementResult {
    pub id: String,
    pub summary: String,
    pub rationale: String,
    pub expected_outcome: String,
    pub applied_edits: Vec<AppliedEdit>,
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_of: Option<String>,
}

impl RefinementResult {
    #[must_use]
    pub fn applied_any(&self) -> bool {
        self.applied_edits.iter().any(|edit| edit.applied)
    }

    #[must_use]
    pub fn notice_message(&self, source: RefinementSource) -> String {
        format!("{}\n\n{}", source.notice_label(), self.notice())
    }

    #[must_use]
    pub fn notice(&self) -> String {
        let mut lines = vec![
            "these harness edits are already applied, nothing to do".to_string(),
            format!(
                "summary: {}",
                compact(&self.summary, OVERVIEW_CONTENT_CHARS)
            ),
        ];
        for edit in self.applied_edits.iter().filter(|edit| edit.applied) {
            let entry = edit.after.as_ref().or(edit.before.as_ref());
            lines.push(format!(
                "- {} {} [{}:{}] {}: {}",
                edit.edit.action.past_tense(),
                edit.edit.kind.as_str(),
                entry.map_or(self.scope, |e| e.scope).as_str(),
                edit.id,
                entry.map_or(edit.id.as_str(), |e| e.title.as_str()),
                compact(
                    entry.map_or("", |e| e.content.as_str()),
                    OVERVIEW_CONTENT_CHARS
                )
            ));
        }
        lines.join("\n")
    }

    #[must_use]
    pub fn history_line(&self) -> String {
        let edits = self
            .applied_edits
            .iter()
            .map(|edit| {
                format!(
                    "{} {} {}:{}",
                    if edit.applied { "applied" } else { "failed" },
                    edit.edit.action.as_str(),
                    edit.edit.kind.as_str(),
                    edit.id
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let rollback = self
            .rollback_of
            .as_ref()
            .map(|id| format!(" rollback_of={id}"))
            .unwrap_or_default();
        format!(
            "[{}]{rollback} {}\n{edits}\nexpected outcome: {}",
            self.id, self.summary, self.expected_outcome
        )
    }
}

pub const DIGEST_PREFIX: &str = "Continual harness state at this point:";

#[must_use]
pub fn global_dir() -> PathBuf {
    crate::store::data_dir().join("harness")
}

#[must_use]
pub fn local_dir(session_id: &mush_ai::types::SessionId) -> PathBuf {
    crate::SessionStore::default_artifact_dir(session_id).join("harness")
}

#[must_use]
pub fn load_merged(session_id: &mush_ai::types::SessionId) -> State {
    State::merged(
        &State::load(&global_dir(), Scope::Global),
        &State::load(&local_dir(session_id), Scope::Local),
    )
}

const DIGEST: crate::user_text::TaggedBlock = crate::user_text::TaggedBlock {
    prefix: DIGEST_PREFIX,
    tag: "harness_digest",
};

#[must_use]
pub fn digest_message(state: &State, focus: &Focus) -> mush_ai::types::Message {
    DIGEST.message(&state.fingerprint(), &state.digest(focus))
}

#[must_use]
pub fn extract_digest(text: &str) -> Option<(String, String)> {
    DIGEST.extract(text)
}

#[must_use]
pub fn latest_digest_fingerprint(messages: &[mush_ai::types::Message]) -> Option<String> {
    DIGEST.latest_fingerprint(messages)
}

pub fn append_history(dir: &Path, result: &RefinementResult) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(HISTORY_FILE))?;
    let mut line = serde_json::to_vec(result).map_err(std::io::Error::other)?;
    line.push(b'\n');
    file.write_all(&line)
}

pub fn load_history(dir: &Path) -> Vec<RefinementResult> {
    let Ok(text) = std::fs::read_to_string(dir.join(HISTORY_FILE)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

pub fn history_for_plan(history: &[RefinementResult]) -> String {
    if history.is_empty() {
        return "no prior refinement history.".into();
    }
    let skip = history.len().saturating_sub(20);
    history
        .iter()
        .skip(skip)
        .map(RefinementResult::history_line)
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn new_id() -> String {
    format!(
        "ref_{}",
        &mush_ai::types::SessionId::new().to_string()[..12]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(kind: Kind, title: &str, content: &str) -> Edit {
        Edit {
            action: Action::Create,
            kind,
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

    fn proposal(edits: Vec<Edit>) -> Proposal {
        Proposal {
            summary: "test".into(),
            rationale: "because".into(),
            expected_outcome: "better".into(),
            edits,
        }
    }

    #[test]
    fn a_create_gets_a_slug_id_and_version_one() {
        let mut state = State::default();
        let result = state.apply(
            &proposal(vec![create(
                Kind::Memory,
                "Always run nextest!",
                "cargo nextest run",
            )]),
            "ref_1",
            Scope::Local,
            None,
        );
        assert!(result.applied_edits[0].applied);
        assert_eq!(result.applied_edits[0].id, "always_run_nextest");
        let entry = state.entry(Kind::Memory, "always_run_nextest").unwrap();
        assert_eq!(entry.version, 1);
        assert_eq!(entry.scope, Scope::Local);
        assert_eq!(state.refinements.len(), 1);
        assert_eq!(
            state.refinements[0].changes,
            vec!["create memory:always_run_nextest"]
        );
    }

    #[test]
    fn an_applied_edit_with_an_explicit_id_round_trips() {
        let mut state = State::default();
        let mut edit = create(Kind::Memory, "nextest", "cargo nextest run");
        edit.id = Some("nextest".into());
        let result = state.apply(&proposal(vec![edit]), "ref_1", Scope::Local, None);
        let json = serde_json::to_string(&result).unwrap();
        let back: RefinementResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, result);
    }

    #[test]
    fn an_applied_edit_saved_with_a_duplicated_id_still_loads() {
        let json =
            r#"{"action":"delete","kind":"memory","id":"nextest","id":"nextest","applied":true}"#;
        let edit: AppliedEdit = serde_json::from_str(json).unwrap();
        assert_eq!(edit.id, "nextest");
        assert_eq!(edit.edit.id.as_deref(), Some("nextest"));
        assert!(edit.applied);
    }

    #[test]
    fn update_bumps_the_version_and_keeps_fields_not_mentioned() {
        let mut state = State::default();
        state.apply(
            &proposal(vec![create(Kind::Memory, "tests", "run them")]),
            "ref_1",
            Scope::Local,
            None,
        );
        let mut update = create(Kind::Memory, "tests", "run them twice");
        update.action = Action::Update;
        update.id = Some("tests".into());
        state.apply(&proposal(vec![update]), "ref_2", Scope::Local, None);
        let entry = state.entry(Kind::Memory, "tests").unwrap();
        assert_eq!(entry.version, 2);
        assert_eq!(entry.content, "run them twice");
        assert_eq!(entry.path, "general");
    }

    #[test]
    fn a_skill_without_a_python_reference_is_rejected() {
        let mut state = State::default();
        let mut skill = create(Kind::Skill, "lint", "runs clippy");
        skill.arguments = Some(Map::new());
        let result = state.apply(&proposal(vec![skill]), "ref_1", Scope::Local, None);
        assert!(!result.applied_edits[0].applied);
        assert_eq!(
            result.applied_edits[0].error.as_deref(),
            Some("create skill requires a python reference")
        );
        assert!(state.entries[&Kind::Skill].is_empty());
    }

    #[test]
    fn a_proposed_update_needs_a_title_content_and_for_a_skill_its_call() {
        let mut state = State::default();
        let mut skill = create(Kind::Skill, "lint", "runs clippy");
        skill.arguments = Some(Map::new());
        skill.reference = Some(
            serde_json::from_value(serde_json::json!({
                "type": "python",
                "import": "lint",
                "callable": "run",
            }))
            .unwrap(),
        );
        state.apply(
            &proposal(vec![create(Kind::Memory, "tests", "run them"), skill]),
            "ref_1",
            Scope::Local,
            None,
        );
        let update = |kind: Kind, id: &str, title: Option<&str>, content: Option<&str>| Edit {
            action: Action::Update,
            id: Some(id.into()),
            title: title.map(str::to_string),
            content: content.map(str::to_string),
            ..create(kind, "", "")
        };
        let result = state.apply(
            &proposal(vec![
                update(Kind::Memory, "tests", None, Some("run them twice")),
                update(Kind::Memory, "tests", Some(""), Some("run them twice")),
                update(Kind::Skill, "lint", Some("lint"), Some("runs clippy twice")),
                Edit {
                    arguments: Some(Map::new()),
                    ..update(Kind::Skill, "lint", Some("lint"), Some("runs clippy twice"))
                },
            ]),
            "ref_2",
            Scope::Local,
            None,
        );
        let errors: Vec<_> = result
            .applied_edits
            .iter()
            .map(|edit| edit.error.as_deref())
            .collect();
        assert_eq!(
            errors,
            [
                Some("update requires title and content"),
                Some("update requires title and content"),
                Some("update skill requires arguments"),
                Some("update skill requires a python reference"),
            ]
        );
        assert_eq!(state.entry(Kind::Memory, "tests").unwrap().version, 1);
        assert_eq!(state.entry(Kind::Skill, "lint").unwrap().version, 1);
    }

    #[test]
    fn the_base_system_prompt_is_not_editable() {
        let mut state = State::default();
        let mut edit = create(Kind::Prompt, "base", "be terse");
        edit.id = Some("base_system_prompt".into());
        let result = state.apply(&proposal(vec![edit]), "ref_1", Scope::Global, None);
        assert_eq!(
            result.applied_edits[0].error.as_deref(),
            Some("the base system prompt is not editable")
        );
    }

    #[test]
    fn rollback_restores_before_snapshots_in_reverse() {
        let mut state = State::default();
        state.apply(
            &proposal(vec![create(Kind::Memory, "keep", "v1")]),
            "ref_1",
            Scope::Local,
            None,
        );
        let mut update = create(Kind::Memory, "keep", "v2");
        update.action = Action::Update;
        update.id = Some("keep".into());
        let result = state.apply(
            &proposal(vec![update, create(Kind::Memory, "extra", "new")]),
            "ref_2",
            Scope::Local,
            None,
        );
        let rollback = Proposal::rollback_of(&result);
        assert_eq!(rollback.edits.len(), 2);
        assert_eq!(rollback.edits[0].action, Action::Delete);
        assert_eq!(rollback.edits[1].action, Action::Update);
        state.apply(&rollback, "ref_3", Scope::Local, Some("ref_2"));
        assert_eq!(state.entry(Kind::Memory, "keep").unwrap().content, "v1");
        assert!(state.entry(Kind::Memory, "extra").is_none());
    }

    #[test]
    fn save_load_and_merge_keep_scopes_apart() {
        let dir = tempfile::tempdir().unwrap();
        let mut global = State::default();
        global.apply(
            &proposal(vec![create(Kind::Memory, "shared", "everyone")]),
            "g1",
            Scope::Global,
            None,
        );
        let mut local = State::default();
        local.apply(
            &proposal(vec![
                create(Kind::Memory, "shared", "just here"),
                create(Kind::Prompt, "terse", "short replies"),
            ]),
            "l1",
            Scope::Local,
            None,
        );
        global.save(&dir.path().join("global")).unwrap();
        local.save(&dir.path().join("local")).unwrap();

        let merged = State::merged(
            &State::load(&dir.path().join("global"), Scope::Global),
            &State::load(&dir.path().join("local"), Scope::Local),
        );
        assert_eq!(merged.entries[&Kind::Memory]["shared"].content, "everyone");
        assert_eq!(
            merged.entries[&Kind::Memory]["local:shared"].content,
            "just here"
        );
        assert_eq!(merged.entries[&Kind::Prompt]["terse"].scope, Scope::Local);
        assert_eq!(merged.refinements.len(), 2);
        assert_eq!(
            State::load(&dir.path().join("missing"), Scope::Local),
            State::default()
        );
    }

    #[test]
    fn the_digest_and_fingerprint_follow_the_entries() {
        let mut state = State::default();
        let empty = state.fingerprint();
        assert!(
            state
                .digest(&Focus::default())
                .contains("no saved harness entries yet.")
        );
        assert!(
            state
                .digest(&Focus::default())
                .contains("default to local refinement")
        );
        state.apply(
            &proposal(vec![create(Kind::Memory, "tests", "cargo nextest run")]),
            "ref_1",
            Scope::Local,
            None,
        );
        let digest = state.digest(&Focus::default());
        assert!(digest.contains("memory: 1"), "{digest}");
        assert!(
            digest.contains("- [local:tests] tests (general, v1): cargo nextest run"),
            "{digest}"
        );
        assert!(digest.contains("recent refinements: 1"), "{digest}");
        assert_ne!(state.fingerprint(), empty);
        assert_eq!(state.fingerprint(), state.clone().fingerprint());
    }

    fn crowded_memories() -> State {
        let mut state = State::default();
        let mut edits: Vec<Edit> = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf",
        ]
        .into_iter()
        .map(|name| {
            create(
                Kind::Memory,
                &format!("{name} note"),
                "nothing in particular",
            )
        })
        .collect();
        edits.push(create(
            Kind::Memory,
            "yankee parser",
            "the yankee parser keeps its grammar in crates/grammar",
        ));
        edits.push(create(
            Kind::Memory,
            "zulu deploys",
            "zulu ships through the staging cluster first",
        ));
        state.apply(&proposal(edits), "ref_1", Scope::Global, None);
        state
    }

    fn memory_lines(digest: &str) -> Vec<&str> {
        digest
            .lines()
            .skip_while(|line| !line.starts_with("memory: "))
            .skip(1)
            .take_while(|line| line.starts_with('-') || line.starts_with('('))
            .collect()
    }

    #[test]
    fn without_a_focus_the_digest_lists_entries_by_path_and_title() {
        let digest = crowded_memories().digest(&Focus::default());
        let lines = memory_lines(&digest);
        assert!(lines[0].contains("alpha note"), "{digest}");
        assert!(!digest.contains("ranked by relevance"), "{digest}");
        assert!(!digest.contains("yankee parser"), "{digest}");
        assert!(
            lines.last().unwrap().contains("+3 more memory entries"),
            "{digest}"
        );
    }

    #[test]
    fn the_goal_and_the_newest_texts_pull_their_entries_into_the_digest() {
        let state = crowded_memories();
        let focus = Focus::new(
            Some("fix the yankee parser"),
            [
                "the zulu staging cluster is down",
                "older chat",
                "more chat",
                "chat",
            ],
        );
        let digest = state.digest(&focus);
        let lines = memory_lines(&digest);
        assert!(lines[0].starts_with("(ranked by relevance"), "{digest}");
        assert!(lines[1].contains("yankee parser"), "{digest}");
        assert!(lines[2].contains("zulu deploys"), "{digest}");
        assert!(lines[3].contains("alpha note"), "{digest}");
        assert_eq!(digest, state.digest(&focus), "ranking is deterministic");
        let mush_ai::types::Message::User(ranked) = digest_message(&state, &focus) else {
            unreachable!()
        };
        let (fingerprint, _) = extract_digest(&ranked.text()).unwrap();
        assert_eq!(fingerprint, state.fingerprint());
    }

    #[test]
    fn the_goal_outweighs_the_texts_and_only_the_newest_four_count() {
        let state = crowded_memories();
        let focus = Focus::new(
            Some("the zulu cluster"),
            [
                "yankee",
                "one more",
                "two more",
                "three more",
                "yankee parser grammar",
            ],
        );
        let lines = memory_lines(&state.digest(&focus)).join("\n");
        let zulu = lines.find("zulu deploys").unwrap();
        let yankee = lines.find("yankee parser").unwrap();
        assert!(zulu < yankee, "{lines}");

        let stale = Focus::new(None, ["a", "b", "c", "d", "yankee parser"]);
        assert!(stale.is_empty());
    }

    #[test]
    fn recent_texts_skip_harness_deliveries_and_keep_the_newest_four() {
        use mush_ai::types::{Message, Timestamp, UserContent, UserMessage};
        let user = |text: &str| {
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text(text.into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            )
        };
        let digest = digest_message(&crowded_memories(), &Focus::default());
        let messages = vec![
            user("first"),
            user("second"),
            user("third"),
            user("fourth"),
            digest,
            user("[self-refinement]\n\nthese harness edits are already applied"),
            user("fifth"),
        ];
        assert_eq!(
            Focus::recent_texts(&messages),
            ["fifth", "fourth", "third", "second"]
        );
    }

    #[test]
    fn the_fingerprint_is_a_stable_hash_of_what_the_digest_prints() {
        assert_eq!(
            State::default().fingerprint(),
            "904c9637d991c951520d8ce4102db3989d6872e0eb4768f47c12f57ff0d24ffd"
        );
        let mut state = State::default();
        state.apply(
            &proposal(vec![create(Kind::Memory, "tests", "cargo nextest run")]),
            "ref_1",
            Scope::Local,
            None,
        );
        let before = state.fingerprint();
        let entry = state
            .entries
            .get_mut(&Kind::Memory)
            .and_then(|entries| entries.get_mut("tests"))
            .unwrap();
        entry.metadata.insert("note".into(), Value::Bool(true));
        entry.reference.insert("type".into(), "python".into());
        entry.updated_at = "later".into();
        entry.source = "agent".into();
        assert_eq!(state.fingerprint(), before);
    }

    #[test]
    fn a_proposal_parses_out_of_a_fenced_reply_and_flags_a_cut_off_one() {
        let parsed = Proposal::parse(
            "here you go\n```json\n{\"summary\":\"s\",\"rationale\":\"r\",\"expectedOutcome\":\"o\",\"edits\":[{\"action\":\"create\",\"kind\":\"memory\",\"title\":\"t\",\"content\":\"c\"}]}\n```",
        )
        .unwrap();
        assert_eq!(parsed.expected_outcome, "o");
        assert_eq!(parsed.edits[0].kind, Kind::Memory);
        let cut = Proposal::parse("{\"summary\":\"s\",\"edits\":[{\"action\":\"crea").unwrap_err();
        assert!(cut.contains("stopped before completing"), "{cut}");
        assert!(Proposal::parse("no json here").is_err());
    }

    #[test]
    fn the_digest_message_roundtrips_its_fingerprint() {
        let mut state = State::default();
        state.apply(
            &proposal(vec![create(Kind::Memory, "a", "b")]),
            "ref_1",
            Scope::Local,
            None,
        );
        let message = digest_message(&state, &Focus::default());
        let mush_ai::types::Message::User(user) = &message else {
            unreachable!()
        };
        let (fingerprint, body) = extract_digest(&user.text()).unwrap();
        assert_eq!(fingerprint, state.fingerprint());
        assert_eq!(body, state.digest(&Focus::default()));
        assert_eq!(
            latest_digest_fingerprint(&[message]),
            Some(state.fingerprint())
        );
        assert_eq!(latest_digest_fingerprint(&[]), None);
    }

    #[test]
    fn a_crud_create_slugs_its_id_and_a_second_one_clashes() {
        let mut state = State::default();
        let entry = state
            .upsert(
                &create(Kind::Memory, "Run The Tests", "cargo nextest"),
                Scope::Local,
                "agent",
            )
            .unwrap();
        assert_eq!(entry.id, "run_the_tests");
        assert_eq!(entry.source, "agent");
        assert_eq!(entry.version, 1);
        assert_eq!(
            state.upsert(
                &create(Kind::Memory, "Run The Tests", "x"),
                Scope::Local,
                "agent"
            ),
            Err("run_the_tests already exists".into())
        );
    }

    #[test]
    fn a_crud_update_changes_one_field_and_leaves_the_rest() {
        let mut state = State::default();
        state
            .upsert(
                &create(Kind::Memory, "tests", "run them"),
                Scope::Local,
                "agent",
            )
            .unwrap();
        let mut edit = create(Kind::Memory, "tests", "run them");
        edit.action = Action::Update;
        edit.id = Some("tests".into());
        edit.title = None;
        edit.content = Some("run them twice".into());
        let after = state.upsert(&edit, Scope::Local, "agent").unwrap();
        assert_eq!(after.title, "tests");
        assert_eq!(after.content, "run them twice");
        assert_eq!(after.version, 2);
        let removed = state.remove(Kind::Memory, "tests").unwrap();
        assert_eq!(removed.id, "tests");
        assert_eq!(
            state.remove(Kind::Memory, "tests"),
            Err("tests not found".into())
        );
    }

    #[test]
    fn a_deletion_leaves_a_tombstone_that_survives_saving_until_the_entry_returns() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::default();
        state
            .upsert(
                &create(Kind::Memory, "gone", "soon"),
                Scope::Global,
                "agent",
            )
            .unwrap();
        state.remove(Kind::Memory, "gone").unwrap();
        state.save(dir.path()).unwrap();

        let mut loaded = State::load(dir.path(), Scope::Global);
        let tombstone = &loaded.deleted[&Kind::Memory]["gone"];
        assert_eq!(tombstone.version, 1);
        assert!(tombstone.deleted_at.ends_with('Z'));

        loaded
            .upsert(
                &create(Kind::Memory, "gone", "back"),
                Scope::Global,
                "agent",
            )
            .unwrap();
        loaded.save(dir.path()).unwrap();
        let revived = State::load(dir.path(), Scope::Global);
        assert!(revived.deleted.is_empty());
        assert_eq!(revived.entry(Kind::Memory, "gone").unwrap().content, "back");
        let raw = std::fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        assert!(!raw.contains("deleted"), "{raw}");
    }

    #[test]
    fn search_ranks_a_rare_term_over_a_common_one() {
        let mut state = State::default();
        for (title, content) in [
            ("first", "the worktree gate runs cargo nextest"),
            ("second", "cargo build is slow"),
            ("third", "cargo fmt is fast"),
        ] {
            state
                .upsert(&create(Kind::Memory, title, content), Scope::Local, "agent")
                .unwrap();
        }
        let hits = state.search("cargo worktree", None, 10);
        assert_eq!(hits[0].title, "first");
        assert_eq!(hits.len(), 3);
        assert_eq!(state.search("cargo", Some(Kind::Skill), 10).len(), 0);
        assert_eq!(state.search("zz", None, 10).len(), 0);
        assert_eq!(state.list(Some(Kind::Memory)).len(), 3);
    }

    #[test]
    fn query_terms_split_cjk_into_bigrams_and_off_adjacent_words() {
        assert_eq!(query_terms("修复登录"), ["修复", "复登", "登录"]);
        assert_eq!(query_terms("修复login bug"), ["修复", "login", "bug"]);
        assert_eq!(query_terms("字"), ["字"]);
        assert_eq!(query_terms("naïve мир a rlm?"), ["naïve", "мир", "rlm"]);
        assert_eq!(query_terms("किताब"), ["किताब"]);

        let mut state = State::default();
        state
            .upsert(
                &create(Kind::Memory, "auth", "登录故障"),
                Scope::Local,
                "agent",
            )
            .unwrap();
        assert_eq!(state.search("修复登录", None, 10).len(), 1);
    }

    #[test]
    fn a_mistyped_edit_field_is_dropped_without_failing_the_proposal() {
        let parsed = Proposal::parse(
            r#"{"edits":[
                {"action":"create","kind":"memory","title":"t","content":"c","reference":"nope","metadata":[1],"path":7},
                "not an edit",
                {"action":"rename","kind":"memory","title":"t","content":"c"},
                {"action":"create","kind":"memory","title":"u","content":"d","path":""}
            ]}"#,
        )
        .unwrap();
        assert_eq!(parsed.edits.len(), 2);
        assert_eq!(parsed.edits[0].reference, None);
        assert_eq!(parsed.edits[0].metadata, None);
        assert_eq!(parsed.edits[0].path, None);

        let mut state = State::default();
        let result = state.apply(&parsed, "ref_1", Scope::Local, None);
        assert!(result.applied_edits[0].applied);
        assert_eq!(
            result.applied_edits[1].error.as_deref(),
            Some("create requires path to be a non-empty string when provided")
        );
    }

    #[test]
    fn a_malformed_entry_is_skipped_with_a_diagnostic_and_kept_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut good = State::default();
        good.upsert(
            &create(Kind::Memory, "tests", "cargo nextest run"),
            Scope::Local,
            "agent",
        )
        .unwrap();
        let mut raw = serde_json::to_value(&good).unwrap();
        raw["entries"]["memory"]["broken"] = serde_json::json!({"id": "broken", "title": 3});
        raw["refinements"] = serde_json::json!([{"id": 7}]);
        std::fs::write(dir.path().join(STATE_FILE), raw.to_string()).unwrap();

        let mut state = State::load(dir.path(), Scope::Local);
        assert!(state.entry(Kind::Memory, "tests").is_some());
        let digest = state.digest(&Focus::default());
        assert!(
            digest.contains("harness: skipped malformed entry broken (invalid type: integer `3`"),
            "{digest}"
        );
        assert!(
            digest.contains("harness: skipped malformed refinement event a number id ("),
            "{digest}"
        );
        assert!(
            state
                .overview_for_plan()
                .contains("- harness: skipped malformed entry broken")
        );

        state
            .upsert(&create(Kind::Memory, "more", "text"), Scope::Local, "agent")
            .unwrap();
        state.save(dir.path()).unwrap();
        let saved: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(saved["entries"]["memory"]["broken"]["title"], 3);
        assert!(saved["entries"]["memory"]["more"].is_object());
        assert_eq!(saved["refinements"][0]["id"], 7);
    }

    #[test]
    fn a_state_file_that_does_not_parse_is_never_saved_over() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(STATE_FILE), "{ not json").unwrap();

        let mut state = State::load(dir.path(), Scope::Global);
        assert!(!state.is_empty());
        assert!(
            state
                .digest(&Focus::default())
                .contains("harness: could not read harness_state.json")
        );
        state
            .upsert(
                &create(Kind::Memory, "tests", "text"),
                Scope::Global,
                "agent",
            )
            .unwrap();
        assert!(state.save(dir.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(STATE_FILE)).unwrap(),
            "{ not json"
        );
    }

    #[test]
    fn history_appends_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::default();
        let result = state.apply(
            &proposal(vec![create(Kind::Memory, "a", "b")]),
            "ref_1",
            Scope::Global,
            None,
        );
        append_history(dir.path(), &result).unwrap();
        let history = load_history(dir.path());
        assert_eq!(history, vec![result]);
        assert!(history_for_plan(&history).starts_with("[ref_1] test"));
        assert!(new_id().starts_with("ref_"));
    }
}
