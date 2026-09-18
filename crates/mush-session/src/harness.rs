use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub schema: u32,
    pub entries: BTreeMap<Kind, BTreeMap<String, Entry>>,
    pub refinements: Vec<RefinementEvent>,
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
        }
    }
}

impl State {
    pub fn load(dir: &Path, scope: Scope) -> Self {
        let Ok(bytes) = std::fs::read(dir.join(STATE_FILE)) else {
            return Self::default();
        };
        let Ok(mut state) = serde_json::from_slice::<Self>(&bytes) else {
            return Self::default();
        };
        for kind in Kind::ALL {
            state.entries.entry(kind).or_default();
        }
        for entry in state.entries.values_mut().flat_map(BTreeMap::values_mut) {
            entry.scope = scope;
        }
        state
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(STATE_FILE);
        let tmp = dir.join(format!("{STATE_FILE}.tmp"));
        let mut json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
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
        merged
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.values().all(BTreeMap::is_empty)
    }

    fn get(&self, kind: Kind, id: &str) -> Option<&Entry> {
        self.entries.get(&kind)?.get(id)
    }

    #[must_use]
    pub fn digest(&self) -> String {
        let mut lines = vec![
            "# continual harness state".to_string(),
            String::new(),
            "local entries belong to this session, global entries persist across sessions. \
             these are compact summaries, use them as routing hints and inspect or refine \
             the entry only when detail matters. the base system prompt is immutable, prompt \
             entries are supplemental notes."
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
        let mut total = 0;
        for kind in Kind::ALL {
            let entries = self.entries.get(&kind).map(BTreeMap::len).unwrap_or(0);
            total += entries;
            lines.push(format!("{}: {entries}", kind.as_str()));
            let mut sorted: Vec<&Entry> = self.entries[&kind].values().collect();
            sorted.sort_by(|a, b| (&a.path, &a.title, &a.id).cmp(&(&b.path, &b.title, &b.id)));
            for entry in sorted.iter().take(OVERVIEW_ENTRIES_PER_KIND) {
                lines.push(entry_line(entry, OVERVIEW_CONTENT_CHARS));
            }
            if entries > OVERVIEW_ENTRIES_PER_KIND {
                lines.push(format!(
                    "- +{} more {} entries",
                    entries - OVERVIEW_ENTRIES_PER_KIND,
                    kind.as_str()
                ));
            }
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
        lines.join("\n").trim().to_string()
    }

    #[must_use]
    pub fn fingerprint(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        for kind in Kind::ALL {
            for (id, entry) in &self.entries[&kind] {
                (
                    kind.as_str(),
                    id,
                    &entry.title,
                    &entry.content,
                    &entry.path,
                    entry.scope.as_str(),
                    entry.version,
                    serde_json::to_string(&entry.reference).unwrap_or_default(),
                    serde_json::to_string(&entry.arguments).unwrap_or_default(),
                )
                    .hash(&mut hasher);
            }
        }
        for event in &self.refinements {
            (&event.id, &event.trigger, &event.changes, &event.outcome).hash(&mut hasher);
        }
        format!("{:016x}", hasher.finish())
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
            if let Some(error) = validate(edit, &entry_id) {
                applied.push(AppliedEdit::failed(edit, entry_id, None, error));
                continue;
            }
            let before = self.get(edit.kind, &entry_id).cloned();
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
                (Action::Delete, Some(_)) => {
                    self.entries.entry(edit.kind).or_default().remove(&entry_id);
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
                    let after = Entry {
                        id: entry_id.clone(),
                        kind: edit.kind,
                        title: edit
                            .title
                            .clone()
                            .or_else(|| before.as_ref().map(|b| b.title.clone()))
                            .unwrap_or_else(|| entry_id.clone()),
                        content: edit
                            .content
                            .clone()
                            .or_else(|| before.as_ref().map(|b| b.content.clone()))
                            .unwrap_or_default(),
                        path: edit
                            .path
                            .clone()
                            .or_else(|| before.as_ref().map(|b| b.path.clone()))
                            .unwrap_or_else(|| "general".into()),
                        scope: before.as_ref().map_or(scope, |b| b.scope),
                        reference: edit
                            .reference
                            .clone()
                            .or_else(|| before.as_ref().map(|b| b.reference.clone()))
                            .unwrap_or_default(),
                        arguments: edit
                            .arguments
                            .clone()
                            .or_else(|| before.as_ref().map(|b| b.arguments.clone()))
                            .unwrap_or_default(),
                        metadata: edit
                            .metadata
                            .clone()
                            .or_else(|| before.as_ref().map(|b| b.metadata.clone()))
                            .unwrap_or_default(),
                        source: "refine".into(),
                        created_at: before.as_ref().map_or_else(now, |b| b.created_at.clone()),
                        updated_at: now(),
                        version: before.as_ref().map_or(1, |b| b.version + 1),
                    };
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

fn entry_line(entry: &Entry, content_chars: usize) -> String {
    let reference = if entry.kind == Kind::Skill && !entry.reference.is_empty() {
        format!(
            " ref={}",
            compact(
                &Value::Object(entry.reference.clone()).to_string(),
                content_chars
            )
        )
    } else {
        String::new()
    };
    let arguments = if entry.kind == Kind::Skill && !entry.arguments.is_empty() {
        format!(
            " args={}",
            compact(
                &Value::Object(entry.arguments.clone()).to_string(),
                content_chars
            )
        )
    } else {
        String::new()
    };
    format!(
        "- [{}:{}] {} ({}, v{}){reference}{arguments}: {}",
        entry.scope.as_str(),
        entry.id,
        entry.title,
        entry.path,
        entry.version,
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

fn now() -> String {
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
        if !value.is_object() {
            return Err("refiner json must be an object".into());
        }
        let mut proposal: Self = serde_json::from_value(value)
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
    if edit.kind == Kind::Prompt && id == "base_system_prompt" {
        return Some("the base system prompt is not editable".into());
    }
    if edit.action != Action::Create && id.is_empty() {
        return Some(format!("{} requires an id", edit.action.as_str()));
    }
    if edit.action == Action::Delete {
        return None;
    }
    if edit.title.is_none() || edit.content.is_none() {
        return Some(format!(
            "{} requires title and content",
            edit.action.as_str()
        ));
    }
    if edit.kind == Kind::Skill {
        if edit.arguments.is_none() {
            return Some(format!("{} skill requires arguments", edit.action.as_str()));
        }
        let Some(reference) = &edit.reference else {
            return Some(format!(
                "{} skill requires a python reference",
                edit.action.as_str()
            ));
        };
        if reference.get("type").and_then(Value::as_str) != Some("python") {
            return Some(format!(
                "{} skill reference.type must be python",
                edit.action.as_str()
            ));
        }
        let non_empty = |key: &str| {
            reference
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        };
        if !non_empty("import") && !non_empty("python_import") {
            return Some(format!(
                "{} skill requires a python import",
                edit.action.as_str()
            ));
        }
        if !non_empty("callable") && !non_empty("call_pattern") {
            return Some(format!(
                "{} skill requires callable or call_pattern",
                edit.action.as_str()
            ));
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedEdit {
    #[serde(flatten)]
    pub edit: Edit,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Entry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Entry>,
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
    pub fn notice(&self) -> String {
        let mut lines = vec![compact(&self.summary, OVERVIEW_CONTENT_CHARS)];
        for edit in self.applied_edits.iter().filter(|edit| edit.applied) {
            let entry = edit.after.as_ref().or(edit.before.as_ref());
            lines.push(format!(
                "- {} {} [{}:{}] {}: {}",
                edit.edit.action.as_str(),
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
        let failed = self
            .applied_edits
            .iter()
            .filter(|edit| !edit.applied)
            .count();
        if failed > 0 {
            lines.push(format!("- {failed} edit(s) rejected"));
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

#[must_use]
pub fn digest_message(state: &State) -> mush_ai::types::Message {
    mush_ai::types::Message::User(
        mush_ai::types::UserMessage::builder()
            .content(mush_ai::types::UserContent::Text(format!(
                "{DIGEST_PREFIX}\n\n<harness_digest fingerprint=\"{}\">\n{}\n</harness_digest>",
                state.fingerprint(),
                state.digest()
            )))
            .timestamp_ms(mush_ai::types::Timestamp::now())
            .build(),
    )
}

#[must_use]
pub fn extract_digest(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix(DIGEST_PREFIX)?.trim_start();
    let rest = rest.strip_prefix("<harness_digest fingerprint=\"")?;
    let (fingerprint, rest) = rest.split_once("\">")?;
    let body = rest.trim().strip_suffix("</harness_digest>")?.trim();
    Some((fingerprint.to_string(), body.to_string()))
}

#[must_use]
pub fn latest_digest_fingerprint(messages: &[mush_ai::types::Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| match message {
        mush_ai::types::Message::User(user) => extract_digest(&user.text()).map(|(fp, _)| fp),
        _ => None,
    })
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
        let entry = state.get(Kind::Memory, "always_run_nextest").unwrap();
        assert_eq!(entry.version, 1);
        assert_eq!(entry.scope, Scope::Local);
        assert_eq!(state.refinements.len(), 1);
        assert_eq!(
            state.refinements[0].changes,
            vec!["create memory:always_run_nextest"]
        );
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
        let entry = state.get(Kind::Memory, "tests").unwrap();
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
        assert_eq!(state.get(Kind::Memory, "keep").unwrap().content, "v1");
        assert!(state.get(Kind::Memory, "extra").is_none());
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
        assert!(state.digest().contains("no saved harness entries yet."));
        state.apply(
            &proposal(vec![create(Kind::Memory, "tests", "cargo nextest run")]),
            "ref_1",
            Scope::Local,
            None,
        );
        let digest = state.digest();
        assert!(digest.contains("memory: 1"), "{digest}");
        assert!(
            digest.contains("- [local:tests] tests (general, v1): cargo nextest run"),
            "{digest}"
        );
        assert!(digest.contains("recent refinements: 1"), "{digest}");
        assert_ne!(state.fingerprint(), empty);
        assert_eq!(state.fingerprint(), state.clone().fingerprint());
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
        let message = digest_message(&state);
        let mush_ai::types::Message::User(user) = &message else {
            unreachable!()
        };
        let (fingerprint, body) = extract_digest(&user.text()).unwrap();
        assert_eq!(fingerprint, state.fingerprint());
        assert_eq!(body, state.digest());
        assert_eq!(
            latest_digest_fingerprint(&[message]),
            Some(state.fingerprint())
        );
        assert_eq!(latest_digest_fingerprint(&[]), None);
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
