use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::nonpoison::Mutex;
use std::time::Duration;

use mush_ai::types::{Message, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_ID_CHARS: usize = 48;
pub const MAX_QUESTION_CHARS: usize = 1_000;
pub const MAX_CONTEXT_CHARS: usize = 4_000;
pub const MAX_OPTIONS: usize = 9;
pub const MAX_OPTION_CHARS: usize = 200;
pub const MAX_DETAIL_CHARS: usize = 500;
pub const MAX_REFS: usize = 8;
pub const MAX_REF_CHARS: usize = 400;
pub const MAX_ANSWER_CHARS: usize = 4_000;
pub const MAX_OPEN_PER_AGENT: usize = 20;
const KEEP_CLOSED: usize = 100;
const DIGEST_BELOW: usize = 12;
const STORE_FILE: &str = "decisions.json";
const LOG_FILE: &str = "decisions.jsonl";

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DecisionKey {
    pub session: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asker {
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child: Option<String>,
    pub name: String,
    #[serde(default)]
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestors: Vec<String>,
}

impl Asker {
    #[must_use]
    pub fn label(&self) -> String {
        let mut parts: Vec<&str> = self.path.iter().map(String::as_str).collect();
        parts.push(&self.name);
        parts.join(" \u{203a} ")
    }

    #[must_use]
    pub fn parent_session(&self) -> Option<&str> {
        self.ancestors.last().map(String::as_str)
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.child.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Answer {
    Pick {
        option: usize,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Own {
        text: String,
    },
    Clarify {
        question: String,
    },
    Later,
}

impl Answer {
    #[must_use]
    pub fn sendable(&self) -> bool {
        !matches!(self, Self::Later)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub answer: Answer,
    pub revision: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DecisionStatus {
    Open,
    Clarifying {
        question: String,
        at: Timestamp,
    },
    Answered {
        answer: Answer,
        at: Timestamp,
    },
    Withdrawn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        at: Timestamp,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub key: DecisionKey,
    pub asker: Asker,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<DecisionOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lean: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
    #[serde(default)]
    pub blocking: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assume_at: Option<Timestamp>,
    pub status: DecisionStatus,
    pub asked_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default)]
    pub revision: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<Draft>,
    #[serde(default)]
    pub delivered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clarified: Option<String>,
}

impl Decision {
    #[must_use]
    pub fn waiting_on_user(&self) -> bool {
        matches!(self.status, DecisionStatus::Open)
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        matches!(
            self.status,
            DecisionStatus::Answered { .. } | DecisionStatus::Withdrawn { .. }
        )
    }

    #[must_use]
    pub fn lean_label(&self) -> Option<&str> {
        self.lean
            .and_then(|index| self.options.get(index))
            .map(|option| option.label.as_str())
    }

    #[must_use]
    pub fn draft_is_stale(&self) -> bool {
        self.draft
            .as_ref()
            .is_some_and(|draft| draft.revision < self.revision)
    }

    #[must_use]
    pub fn past_assumption(&self, now: Timestamp) -> bool {
        self.assume_at.is_some_and(|at| at <= now)
    }
}

#[must_use]
pub fn letter(index: usize) -> char {
    u8::try_from(index)
        .ok()
        .filter(|index| *index < 26)
        .map_or('?', |index| char::from(b'a' + index))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lean {
    Index(usize),
    Label(String),
}

#[derive(Debug, Clone, Default)]
pub struct Ask {
    pub id: String,
    pub question: String,
    pub options: Vec<DecisionOption>,
    pub lean: Option<Lean>,
    pub context: Option<String>,
    pub refs: Vec<String>,
    pub blocking: bool,
    pub assume_after: Option<Duration>,
    pub supersedes: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Change {
    pub question: Option<String>,
    pub options: Option<Vec<DecisionOption>>,
    pub lean: Option<Lean>,
    pub context: Option<String>,
    pub refs: Option<Vec<String>>,
    pub blocking: Option<bool>,
    pub assume_after: Option<Duration>,
}

impl Ask {
    pub fn from_payload(payload: &Value) -> Result<Self, String> {
        let id = text(payload, "id").ok_or("id is required")?;
        check_id(&id)?;
        let question = text(payload, "question").ok_or("question is required")?;
        let change = Change::from_payload(payload)?;
        Ok(Self {
            id,
            question,
            options: change.options.unwrap_or_default(),
            lean: change.lean,
            context: change.context,
            refs: change.refs.unwrap_or_default(),
            blocking: change.blocking.unwrap_or(false),
            assume_after: change.assume_after,
            supersedes: text(payload, "supersedes"),
        })
    }
}

impl Change {
    pub fn from_payload(payload: &Value) -> Result<Self, String> {
        let options = match payload.get("options") {
            None | Some(Value::Null) => None,
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .map(parse_option)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Some(_) => return Err("options must be a list".into()),
        };
        let lean = match payload.get("lean") {
            None | Some(Value::Null) => None,
            Some(Value::Number(number)) => Some(Lean::Index(
                number
                    .as_u64()
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or("lean must be an option index or label")?,
            )),
            Some(Value::String(label)) => Some(Lean::Label(label.clone())),
            Some(_) => return Err("lean must be an option index or label".into()),
        };
        let refs = match payload.get("refs") {
            None | Some(Value::Null) => None,
            Some(Value::String(one)) => Some(vec![one.clone()]),
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| "refs must be strings".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Some(_) => return Err("refs must be a list of strings".into()),
        };
        let blocking = match payload.get("blocking") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(blocking)) => Some(*blocking),
            Some(_) => return Err("blocking must be True or False".into()),
        };
        let assume_after = match payload.get("assume_after") {
            None | Some(Value::Null) => None,
            Some(value) => Some(parse_duration(value)?),
        };
        Ok(Self {
            question: text(payload, "question"),
            options,
            lean,
            context: text(payload, "context"),
            refs,
            blocking,
            assume_after,
        })
    }
}

fn text(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn parse_option(value: &Value) -> Result<DecisionOption, String> {
    match value {
        Value::String(label) => Ok(DecisionOption {
            label: label.trim().to_owned(),
            detail: None,
        }),
        Value::Object(fields) => Ok(DecisionOption {
            label: fields
                .get("label")
                .and_then(Value::as_str)
                .map(|label| label.trim().to_owned())
                .ok_or("each option needs a label")?,
            detail: fields
                .get("detail")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|detail| !detail.is_empty())
                .map(str::to_owned),
        }),
        _ => Err("each option is a string or {label, detail}".into()),
    }
}

fn parse_duration(value: &Value) -> Result<Duration, String> {
    let invalid = || "assume_after is seconds or a duration such as \"90m\" or \"2h\"".to_string();
    if let Some(seconds) = value.as_u64() {
        return Ok(Duration::from_secs(seconds));
    }
    let text = value.as_str().map(str::trim).ok_or_else(invalid)?;
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().map_err(|_| invalid())?;
    let scale = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(invalid()),
    };
    Ok(Duration::from_secs(number.saturating_mul(scale)))
}

fn check_id(id: &str) -> Result<(), String> {
    let valid = !id.is_empty()
        && id.chars().count() <= MAX_ID_CHARS
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "a decision id is 1 to {MAX_ID_CHARS} letters, digits, '-', '_' or '.'"
        ))
    }
}

fn check_len(what: &str, text: &str, max: usize) -> Result<(), String> {
    if text.chars().count() > max {
        return Err(format!("{what} is longer than {max} characters"));
    }
    Ok(())
}

fn check_options(options: &[DecisionOption]) -> Result<(), String> {
    if options.len() > MAX_OPTIONS {
        return Err(format!("a decision takes at most {MAX_OPTIONS} options"));
    }
    for (index, option) in options.iter().enumerate() {
        if option.label.is_empty() {
            return Err("an option label cannot be empty".into());
        }
        check_len("an option label", &option.label, MAX_OPTION_CHARS)?;
        if let Some(detail) = &option.detail {
            check_len("an option detail", detail, MAX_DETAIL_CHARS)?;
        }
        if options[..index]
            .iter()
            .any(|other| other.label.eq_ignore_ascii_case(&option.label))
        {
            return Err(format!("the option {:?} is listed twice", option.label));
        }
    }
    Ok(())
}

fn check_refs(refs: &[String]) -> Result<(), String> {
    if refs.len() > MAX_REFS {
        return Err(format!("a decision takes at most {MAX_REFS} refs"));
    }
    refs.iter()
        .try_for_each(|reference| check_len("a ref", reference, MAX_REF_CHARS))
}

fn resolve_lean(options: &[DecisionOption], lean: Option<&Lean>) -> Result<Option<usize>, String> {
    match lean {
        None => Ok(None),
        Some(_) if options.is_empty() => {
            Err("a lean needs options; leave it out for a free-text question".into())
        }
        Some(Lean::Index(index)) if *index < options.len() => Ok(Some(*index)),
        Some(Lean::Index(index)) => Err(format!(
            "lean {index} is out of range for {} options",
            options.len()
        )),
        Some(Lean::Label(label)) => options
            .iter()
            .position(|option| option.label.eq_ignore_ascii_case(label.trim()))
            .map(Some)
            .ok_or_else(|| format!("lean {label:?} is not one of the options")),
    }
}

fn assume_at(after: Option<Duration>, now: Timestamp) -> Option<Timestamp> {
    after.map(|after| {
        Timestamp::from_ms(
            now.as_ms()
                .saturating_add(u64::try_from(after.as_millis()).unwrap_or(u64::MAX)),
        )
    })
}

fn kept_draft(draft: Draft, options: &[DecisionOption], revision: u32) -> Draft {
    match draft.answer {
        Answer::Pick { label, note, .. } => {
            match options.iter().position(|option| option.label == label) {
                Some(option) => Draft {
                    answer: Answer::Pick {
                        option,
                        label,
                        note,
                    },
                    revision,
                },
                None => Draft {
                    answer: Answer::Own {
                        text: match note {
                            Some(note) => format!("{label}. {note}"),
                            None => label,
                        },
                    },
                    revision: 0,
                },
            }
        }
        answer => Draft {
            answer,
            revision: draft.revision,
        },
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    decisions: Vec<Decision>,
    #[serde(default)]
    news: Vec<News>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct News {
    recipient: String,
    line: String,
}

impl Store {
    fn open_with_user(&self) -> usize {
        self.decisions
            .iter()
            .filter(|decision| decision.waiting_on_user())
            .count()
    }

    fn find_open(&mut self, key: &DecisionKey) -> Option<&mut Decision> {
        self.decisions
            .iter_mut()
            .find(|decision| &decision.key == key && !decision.is_closed())
    }

    fn prune(&mut self) {
        let closed: Vec<(Timestamp, usize)> = self
            .decisions
            .iter()
            .enumerate()
            .filter(|(_, decision)| {
                decision.is_closed()
                    && (decision.delivered
                        || matches!(decision.status, DecisionStatus::Withdrawn { .. }))
            })
            .map(|(index, decision)| (decision.updated_at, index))
            .collect();
        if closed.len() <= KEEP_CLOSED {
            return;
        }
        let mut oldest = closed;
        oldest.sort();
        let mut doomed: Vec<usize> = oldest
            .iter()
            .take(oldest.len() - KEEP_CLOSED)
            .map(|(_, index)| *index)
            .collect();
        doomed.sort_unstable_by(|a, b| b.cmp(a));
        for index in doomed {
            self.decisions.remove(index);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub decision: Decision,
    pub updated: bool,
    pub open_with_user: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub asker: Asker,
    pub keys: Vec<DecisionKey>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DecisionsCommand {
    Refresh,
    Draft {
        key: DecisionKey,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<Answer>,
    },
    Send {
        keys: Vec<DecisionKey>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    Mine(&'a str),
    Family(&'a str),
}

pub struct Decisions {
    dir: Mutex<Option<PathBuf>>,
    store: Mutex<Store>,
    revision: AtomicU64,
    changed: tokio::sync::Notify,
}

impl Decisions {
    #[must_use]
    pub fn new(dir: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            store: Mutex::new(load(dir.as_ref())),
            dir: Mutex::new(dir),
            revision: AtomicU64::new(0),
            changed: tokio::sync::Notify::new(),
        })
    }

    pub fn rebind(&self, dir: Option<PathBuf>) {
        let store = load(dir.as_ref());
        {
            let mut current = self.store.lock();
            *current = store;
            *self.dir.lock() = dir;
        }
        self.bump();
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
        self.changed.notify_waiters();
    }

    #[must_use]
    pub fn visible(&self) -> Vec<Decision> {
        let mut open: Vec<Decision> = self
            .store
            .lock()
            .decisions
            .iter()
            .filter(|decision| !decision.is_closed())
            .cloned()
            .collect();
        open.sort_by_key(|decision| decision.asked_at);
        open
    }

    #[must_use]
    pub fn open_with_user(&self) -> usize {
        self.store.lock().open_with_user()
    }

    #[must_use]
    pub fn list(&self, scope: Scope<'_>) -> Vec<Decision> {
        let store = self.store.lock();
        store
            .decisions
            .iter()
            .filter(|decision| match scope {
                Scope::Mine(session) => decision.key.session == session,
                Scope::Family(session) => {
                    decision.key.session == session
                        || decision.asker.ancestors.iter().any(|a| a == session)
                }
            })
            .cloned()
            .collect()
    }

    fn mutate<T>(
        &self,
        apply: impl FnOnce(&mut Store, &mut Vec<Value>) -> Result<T, String>,
    ) -> Result<T, String> {
        let result = {
            let mut store = self.store.lock();
            let mut next = store.clone();
            let mut log = Vec::new();
            let result = apply(&mut next, &mut log)?;
            next.prune();
            *store = next;
            self.persist(&store, &log);
            result
        };
        self.bump();
        Ok(result)
    }

    pub fn ask(&self, asker: Asker, ask: Ask) -> Result<Asked, String> {
        check_id(&ask.id)?;
        check_len("the question", &ask.question, MAX_QUESTION_CHARS)?;
        check_options(&ask.options)?;
        check_refs(&ask.refs)?;
        if let Some(context) = &ask.context {
            check_len("the context", context, MAX_CONTEXT_CHARS)?;
        }
        let lean = resolve_lean(&ask.options, ask.lean.as_ref())?;
        if ask.supersedes.as_deref() == Some(ask.id.as_str()) {
            return Err("a decision cannot supersede itself; update it instead".into());
        }
        self.mutate(|store, log| {
            let now = Timestamp::now();
            let key = DecisionKey {
                session: asker.session.clone(),
                id: ask.id.clone(),
            };
            if let Some(decision) = store.find_open(&key) {
                decision.asker = asker;
                decision.question = ask.question;
                decision.options = ask.options;
                decision.lean = lean;
                decision.context = ask.context;
                decision.refs = ask.refs;
                decision.blocking = ask.blocking;
                decision.assume_at = assume_at(ask.assume_after, now);
                touch(decision, now);
                log.push(logged("updated", decision));
                let decision = decision.clone();
                return Ok(Asked {
                    decision,
                    updated: true,
                    open_with_user: store.open_with_user(),
                });
            }
            let open_mine = store
                .decisions
                .iter()
                .filter(|decision| decision.key.session == key.session && !decision.is_closed())
                .count();
            if open_mine >= MAX_OPEN_PER_AGENT {
                return Err(format!(
                    "you already have {MAX_OPEN_PER_AGENT} decisions open with the user; withdraw \
                     or merge some first"
                ));
            }
            store.decisions.retain(|decision| decision.key != key);
            let mut decision = Decision {
                key,
                asker,
                question: ask.question,
                options: ask.options,
                lean,
                context: ask.context,
                refs: ask.refs,
                blocking: ask.blocking,
                assume_at: assume_at(ask.assume_after, now),
                status: DecisionStatus::Open,
                asked_at: now,
                updated_at: now,
                revision: 1,
                draft: None,
                delivered: false,
                clarified: None,
            };
            if let Some(old_id) = ask.supersedes {
                let old_key = DecisionKey {
                    session: decision.key.session.clone(),
                    id: old_id.clone(),
                };
                let old = store
                    .find_open(&old_key)
                    .ok_or_else(|| format!("there is no open decision {old_id} to supersede"))?;
                old.status = DecisionStatus::Withdrawn {
                    reason: Some(format!("superseded by {}", decision.key.id)),
                    at: now,
                };
                old.updated_at = now;
                if let Some(draft) = old.draft.take() {
                    let mut kept = kept_draft(draft, &decision.options, 1);
                    if !matches!(kept.answer, Answer::Pick { .. }) {
                        kept.revision = 0;
                    }
                    decision.draft = Some(kept);
                }
                log.push(logged("superseded", old));
            }
            log.push(logged("asked", &decision));
            store.decisions.push(decision.clone());
            Ok(Asked {
                decision,
                updated: false,
                open_with_user: store.open_with_user(),
            })
        })
    }

    pub fn update(&self, session: &str, id: &str, change: Change) -> Result<Decision, String> {
        if let Some(question) = &change.question {
            check_len("the question", question, MAX_QUESTION_CHARS)?;
        }
        if let Some(options) = &change.options {
            check_options(options)?;
        }
        if let Some(refs) = &change.refs {
            check_refs(refs)?;
        }
        if let Some(context) = &change.context {
            check_len("the context", context, MAX_CONTEXT_CHARS)?;
        }
        self.mutate(|store, log| {
            let now = Timestamp::now();
            let key = DecisionKey {
                session: session.to_owned(),
                id: id.to_owned(),
            };
            let decision = store
                .find_open(&key)
                .ok_or_else(|| format!("you have no open decision {id}"))?;
            if let Some(question) = change.question {
                decision.question = question;
            }
            let old_lean = decision.lean_label().map(str::to_owned);
            if let Some(options) = change.options {
                decision.options = options;
            }
            decision.lean = match change.lean {
                Some(lean) => resolve_lean(&decision.options, Some(&lean))?,
                None => old_lean.and_then(|label| {
                    decision
                        .options
                        .iter()
                        .position(|option| option.label == label)
                }),
            };
            if let Some(context) = change.context {
                decision.context = Some(context);
            }
            if let Some(refs) = change.refs {
                decision.refs = refs;
            }
            if let Some(blocking) = change.blocking {
                decision.blocking = blocking;
            }
            if change.assume_after.is_some() {
                decision.assume_at = assume_at(change.assume_after, now);
            }
            touch(decision, now);
            log.push(logged("updated", decision));
            Ok(decision.clone())
        })
    }

    pub fn withdraw(
        &self,
        session: &str,
        id: &str,
        reason: Option<String>,
    ) -> Result<Decision, String> {
        self.mutate(|store, log| {
            let key = DecisionKey {
                session: session.to_owned(),
                id: id.to_owned(),
            };
            let decision = store
                .find_open(&key)
                .ok_or_else(|| format!("you have no open decision {id}"))?;
            decision.status = DecisionStatus::Withdrawn {
                reason,
                at: Timestamp::now(),
            };
            decision.updated_at = Timestamp::now();
            decision.draft = None;
            log.push(logged("withdrawn", decision));
            Ok(decision.clone())
        })
    }

    pub fn set_draft(&self, key: &DecisionKey, answer: Option<Answer>) -> Result<(), String> {
        self.mutate(|store, _| {
            let decision = store
                .find_open(key)
                .ok_or_else(|| format!("{} is no longer open", key.id))?;
            decision.draft = match answer {
                None => None,
                Some(answer) => Some(Draft {
                    answer: checked_answer(decision, answer)?,
                    revision: decision.revision,
                }),
            };
            Ok(())
        })
    }

    pub fn send(&self, keys: &[DecisionKey]) -> Result<Vec<Outgoing>, String> {
        self.mutate(|store, log| {
            let now = Timestamp::now();
            let mut sent: Vec<DecisionKey> = Vec::new();
            let mut news = Vec::new();
            for key in keys {
                let Some(decision) = store.find_open(key) else {
                    continue;
                };
                let Some(draft) = decision.draft.take() else {
                    continue;
                };
                if !draft.answer.sendable() {
                    decision.draft = Some(draft);
                    continue;
                }
                decision.status = match draft.answer {
                    Answer::Clarify { question } => {
                        DecisionStatus::Clarifying { question, at: now }
                    }
                    answer => DecisionStatus::Answered { answer, at: now },
                };
                decision.updated_at = now;
                decision.delivered = false;
                log.push(logged("sent", decision));
                if let Some(parent) = decision.asker.parent_session() {
                    news.push(News {
                        recipient: parent.to_owned(),
                        line: parent_line(decision),
                    });
                }
                sent.push(key.clone());
            }
            store.news.extend(news);
            let mut outgoing: Vec<Outgoing> = Vec::new();
            for key in &sent {
                let Some(decision) = store.decisions.iter().find(|d| &d.key == key) else {
                    continue;
                };
                match outgoing
                    .iter_mut()
                    .find(|out| out.asker.session == decision.key.session)
                {
                    Some(out) => out.keys.push(key.clone()),
                    None => outgoing.push(Outgoing {
                        asker: decision.asker.clone(),
                        keys: vec![key.clone()],
                        text: String::new(),
                    }),
                }
            }
            for out in &mut outgoing {
                out.text = compose_answers(store, &out.keys);
            }
            Ok(outgoing)
        })
    }

    pub fn mark_delivered(&self, keys: &[DecisionKey], delivered: bool) {
        let _ = self.mutate(|store, _| {
            for decision in &mut store.decisions {
                if keys.contains(&decision.key) {
                    decision.delivered = delivered;
                }
            }
            Ok(())
        });
    }

    pub fn post_news(&self, recipient: &str, line: String) {
        let _ = self.mutate(|store, _| {
            store.news.push(News {
                recipient: recipient.to_owned(),
                line,
            });
            Ok(())
        });
    }

    #[must_use]
    pub fn take_news(&self, session: &str) -> Option<String> {
        {
            let store = self.store.lock();
            let pending = store.news.iter().any(|news| news.recipient == session)
                || store
                    .decisions
                    .iter()
                    .any(|decision| undelivered(decision, session));
            if !pending {
                return None;
            }
        }
        self.mutate(|store, _| {
            let mut parts = Vec::new();
            let keys: Vec<DecisionKey> = store
                .decisions
                .iter()
                .filter(|decision| undelivered(decision, session))
                .map(|decision| decision.key.clone())
                .collect();
            if !keys.is_empty() {
                parts.push(compose_answers(store, &keys));
                for decision in &mut store.decisions {
                    if keys.contains(&decision.key) {
                        decision.delivered = true;
                    }
                }
            }
            let lines: Vec<String> = store
                .news
                .iter()
                .filter(|news| news.recipient == session)
                .map(|news| news.line.clone())
                .collect();
            store.news.retain(|news| news.recipient != session);
            if !lines.is_empty() {
                parts.push(lines.join("\n"));
            }
            Ok((!parts.is_empty()).then(|| parts.join("\n\n")))
        })
        .ok()
        .flatten()
    }

    #[must_use]
    pub fn digest(&self, session: &str) -> Option<String> {
        let store = self.store.lock();
        let mine: Vec<&Decision> = store
            .decisions
            .iter()
            .filter(|decision| decision.key.session == session && !decision.is_closed())
            .collect();
        let below: Vec<&Decision> = store
            .decisions
            .iter()
            .filter(|decision| {
                decision.waiting_on_user() && decision.asker.ancestors.iter().any(|a| a == session)
            })
            .collect();
        if mine.is_empty() && below.is_empty() {
            return None;
        }
        let mut out = String::new();
        if !mine.is_empty() {
            out.push_str("Yours, still open:\n");
            for decision in &mine {
                out.push_str(&digest_line(decision));
                out.push('\n');
            }
        }
        if !below.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("From agents below you, waiting on the user:\n");
            for decision in below.iter().take(DIGEST_BELOW) {
                out.push_str(&format!(
                    "- {} [{}] {}\n",
                    decision.asker.label(),
                    decision.key.id,
                    decision.question
                ));
            }
            if below.len() > DIGEST_BELOW {
                out.push_str(&format!(
                    "- and {} more; `rlm.decisions.list(scope=\"family\")` has them all\n",
                    below.len() - DIGEST_BELOW
                ));
            }
        }
        out.push_str(
            "\nAnswers arrive as a message from the user. `rlm.decisions.list()` shows them.",
        );
        Some(out)
    }

    pub async fn wait(
        &self,
        session: &str,
        ids: &[String],
        timeout: Duration,
    ) -> Result<Vec<Decision>, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let changed = self.changed.notified();
            let current: Vec<Decision> = {
                let store = self.store.lock();
                ids.iter()
                    .map(|id| {
                        store
                            .decisions
                            .iter()
                            .find(|decision| {
                                decision.key.session == session && &decision.key.id == id
                            })
                            .cloned()
                            .ok_or_else(|| format!("you have no decision {id}"))
                    })
                    .collect::<Result<_, _>>()?
            };
            let settled = current.iter().all(|decision| !decision.waiting_on_user());
            if settled || tokio::time::Instant::now() >= deadline {
                return Ok(current);
            }
            tokio::select! {
                () = changed => {}
                () = tokio::time::sleep_until(deadline) => {}
            }
        }
    }

    fn persist(&self, store: &Store, log: &[Value]) {
        let Some(dir) = self.dir.lock().clone() else {
            return;
        };
        crate::persist::write_json(&dir.join(STORE_FILE), store);
        if log.is_empty() {
            return;
        }
        let lines: String = log
            .iter()
            .filter_map(|entry| serde_json::to_string(entry).ok())
            .map(|line| line + "\n")
            .collect();
        crate::persist::append_private(&dir.join(LOG_FILE), &lines);
    }
}

fn load(dir: Option<&PathBuf>) -> Store {
    crate::persist::load_json(dir.map(|dir| dir.join(STORE_FILE)).as_deref()).unwrap_or_default()
}

fn touch(decision: &mut Decision, now: Timestamp) {
    decision.revision += 1;
    decision.updated_at = now;
    if let DecisionStatus::Clarifying { question, .. } = &decision.status {
        decision.clarified = Some(question.clone());
        decision.status = DecisionStatus::Open;
    }
    if let Some(draft) = decision.draft.take() {
        let revision = decision.revision;
        let kept = kept_draft(draft, &decision.options, revision);
        decision.draft = Some(kept);
    }
}

fn logged(event: &str, decision: &Decision) -> Value {
    let mut decision = decision.clone();
    decision.draft = None;
    json!({
        "at": Timestamp::now(),
        "event": event,
        "decision": decision,
    })
}

fn undelivered(decision: &Decision, session: &str) -> bool {
    decision.key.session == session
        && !decision.delivered
        && matches!(
            decision.status,
            DecisionStatus::Answered { .. } | DecisionStatus::Clarifying { .. }
        )
}

fn checked_answer(decision: &Decision, answer: Answer) -> Result<Answer, String> {
    let note = |note: Option<String>| -> Result<Option<String>, String> {
        let note = note
            .map(|note| note.trim().to_owned())
            .filter(|note| !note.is_empty());
        if let Some(note) = &note {
            check_len("a note", note, MAX_ANSWER_CHARS)?;
        }
        Ok(note)
    };
    match answer {
        Answer::Pick {
            option,
            note: given,
            ..
        } => {
            let label = decision
                .options
                .get(option)
                .map(|option| option.label.clone())
                .ok_or_else(|| format!("{} has no option {}", decision.key.id, letter(option)))?;
            Ok(Answer::Pick {
                option,
                label,
                note: note(given)?,
            })
        }
        Answer::Own { text } | Answer::Clarify { question: text } if text.trim().is_empty() => {
            Err("an answer needs some text".into())
        }
        Answer::Own { text } => {
            check_len("an answer", &text, MAX_ANSWER_CHARS)?;
            Ok(Answer::Own {
                text: text.trim().to_owned(),
            })
        }
        Answer::Clarify { question } => {
            check_len("a question", &question, MAX_ANSWER_CHARS)?;
            Ok(Answer::Clarify {
                question: question.trim().to_owned(),
            })
        }
        Answer::Later => Ok(Answer::Later),
    }
}

#[must_use]
pub fn answer_text(decision: &Decision, answer: &Answer) -> String {
    match answer {
        Answer::Pick {
            option,
            label,
            note,
        } => {
            let mut text = format!("{}) {label}", letter(*option));
            if decision.lean == Some(*option) {
                text.push_str(" (your lean)");
            }
            if let Some(note) = note {
                text.push_str(&format!(". Note: {note}"));
            }
            text
        }
        Answer::Own { text } => format!("In my own words: {text}"),
        Answer::Clarify { question } => format!(
            "Not yet, I need more first: {question} Update the decision with \
             `rlm.decisions.update(\"{}\", context=...)` and it comes back to me.",
            decision.key.id
        ),
        Answer::Later => "Later.".into(),
    }
}

fn compose_answers(store: &Store, keys: &[DecisionKey]) -> String {
    let mut out = String::from("Answers to your decisions:");
    for key in keys {
        let Some(decision) = store.decisions.iter().find(|d| &d.key == key) else {
            continue;
        };
        let answer = match &decision.status {
            DecisionStatus::Answered { answer, .. } => answer.clone(),
            DecisionStatus::Clarifying { question, .. } => Answer::Clarify {
                question: question.clone(),
            },
            _ => continue,
        };
        out.push_str(&format!(
            "\n\n[{}] {}\n{}",
            decision.key.id,
            decision.question,
            answer_text(decision, &answer)
        ));
    }
    let Some(session) = keys.first().map(|key| key.session.as_str()) else {
        return out;
    };
    let open: Vec<String> = store
        .decisions
        .iter()
        .filter(|decision| decision.key.session == session && decision.waiting_on_user())
        .map(|decision| format!("[{}]", decision.key.id))
        .collect();
    if !open.is_empty() {
        out.push_str(&format!("\n\nStill open with me: {}.", open.join(", ")));
    }
    out
}

fn parent_line(decision: &Decision) -> String {
    let what = match &decision.status {
        DecisionStatus::Answered { answer, .. } => answer_text(decision, answer),
        DecisionStatus::Clarifying { question, .. } => format!("asked them to clarify: {question}"),
        _ => String::new(),
    };
    format!(
        "- the user answered {}'s [{}]: {what}",
        decision.asker.name, decision.key.id
    )
}

#[must_use]
pub fn asked_line(decision: &Decision) -> String {
    let mut line = format!(
        "- {} asked the user [{}]: {}",
        decision.asker.name, decision.key.id, decision.question
    );
    match decision.lean_label() {
        Some(lean) if decision.blocking => {
            line.push_str(&format!(" (blocking; it leans {lean})"));
        }
        Some(lean) => line.push_str(&format!(" (it carries on with {lean})")),
        None if decision.blocking => line.push_str(" (blocking)"),
        None => {}
    }
    line
}

fn digest_line(decision: &Decision) -> String {
    let mut line = format!("- [{}] {}", decision.key.id, decision.question);
    if !decision.options.is_empty() {
        let options: Vec<String> = decision
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| format!("{}) {}", letter(index), option.label))
            .collect();
        line.push_str(&format!(" Options: {}.", options.join(", ")));
    }
    if let Some(lean) = decision.lean {
        line.push_str(&format!(" Your lean: {}.", letter(lean)));
    }
    if decision.blocking {
        line.push_str(" Blocking.");
    }
    if let DecisionStatus::Clarifying { question, .. } = &decision.status {
        line.push_str(&format!(
            " The user asked you to clarify: {question} Update the decision to answer."
        ));
    }
    line
}

#[must_use]
pub fn agent_view(decision: &Decision) -> Value {
    let (status, answer, clarification, reason) = match &decision.status {
        DecisionStatus::Open => ("open", None, decision.clarified.clone(), None),
        DecisionStatus::Clarifying { question, .. } => {
            ("clarifying", None, Some(question.clone()), None)
        }
        DecisionStatus::Answered { answer, .. } => {
            ("answered", Some(answer_text(decision, answer)), None, None)
        }
        DecisionStatus::Withdrawn { reason, .. } => ("withdrawn", None, None, reason.clone()),
    };
    json!({
        "id": decision.key.id,
        "asker": decision.asker.label(),
        "status": status,
        "question": decision.question,
        "options": decision.options.iter().map(|option| option.label.clone()).collect::<Vec<_>>(),
        "lean": decision.lean_label(),
        "blocking": decision.blocking,
        "answer": answer,
        "clarification": clarification,
        "reason": reason,
        "asked": decision.asked_at.age_display(),
    })
}

impl crate::session::SessionRuntime {
    pub fn refresh_decisions(&mut self) -> Option<String> {
        let body = self
            .decisions
            .as_ref()?
            .take_news(self.session_id.as_str())?;
        self.conversation
            .append_message(mush_session::decisions::news_message(&body));
        Some(body)
    }
}

#[must_use]
pub fn digest_after_compaction(
    decisions: &Decisions,
    session: &str,
    compacted: &[Message],
) -> Option<Message> {
    let body = decisions.digest(session)?;
    let fingerprint = mush_session::decisions::fingerprint(&body);
    if mush_session::decisions::latest_digest_fingerprint(compacted).as_deref()
        == Some(fingerprint.as_str())
    {
        return None;
    }
    Some(mush_session::decisions::digest_message(&body))
}

#[cfg(test)]
mod tests;
