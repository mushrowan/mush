use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mush_ai::types::{ThinkingLevel, Timestamp};
use mush_kernel::HostError;
use serde_json::{Value, json};

use super::{
    ChildSummary, CreateSessionRequest, Family, Identity, OBSERVE_DEFAULT_CHARS,
    OBSERVE_DEFAULT_LIMIT, SessionSpawner,
};
use crate::connection::{ClientCommand, HostCommands};
use crate::goal::{Goal, GoalCommand, GoalStore};
use crate::harness::HarnessStore;
use crate::notes::Notes;
use crate::schedule::{Delivery, Job, JobSource, JobStatus, ScheduleCommand, Scheduler};

pub struct RlmHandler {
    family: Arc<Family>,
    identity: Identity,
    goal: Option<Arc<GoalStore>>,
    scheduler: Option<Arc<Scheduler>>,
    notes: Option<Arc<Notes>>,
    harness: Option<Arc<HarnessStore>>,
    sessions: Option<SessionSpawner>,
    host_commands: Option<Arc<HostCommands>>,
    compaction_callable: bool,
}

#[derive(Clone, Copy)]
struct Payload<'a>(&'a Value);

impl Payload<'_> {
    fn text(self, key: &str) -> Option<String> {
        self.0.get(key).and_then(Value::as_str).map(str::to_owned)
    }

    fn required(self, key: &str) -> Result<String, HostError> {
        self.text(key)
            .ok_or_else(|| HostError(format!("{key} is required")))
    }

    fn instructions(self) -> Option<String> {
        self.text("instructions")
            .filter(|text| !text.trim().is_empty())
    }

    fn flag(self, key: &str) -> bool {
        self.0.get(key).and_then(Value::as_bool).unwrap_or(false)
    }

    fn number(self, key: &str) -> Option<u64> {
        self.0.get(key).and_then(Value::as_u64)
    }

    fn object(self, key: &str) -> Option<serde_json::Map<String, Value>> {
        self.0.get(key).and_then(Value::as_object).cloned()
    }

    fn targets(self) -> Vec<String> {
        self.0
            .get("targets")
            .and_then(Value::as_array)
            .map(|targets| {
                targets
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn to_json(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn summaries(children: &[ChildSummary]) -> Value {
    Value::Array(children.iter().map(summary_json).collect())
}

impl RlmHandler {
    #[must_use]
    pub fn new(family: Arc<Family>, identity: Identity) -> Self {
        Self {
            family,
            identity,
            goal: None,
            scheduler: None,
            notes: None,
            harness: None,
            sessions: None,
            host_commands: None,
            compaction_callable: true,
        }
    }

    #[must_use]
    pub fn with_compaction_callable(mut self, callable: bool) -> Self {
        self.compaction_callable = callable;
        self
    }

    #[must_use]
    pub fn with_host_commands(mut self, commands: Arc<HostCommands>) -> Self {
        self.host_commands = Some(commands);
        self
    }

    #[must_use]
    pub fn with_session_spawner(mut self, spawner: SessionSpawner) -> Self {
        self.sessions = Some(spawner);
        self
    }

    #[must_use]
    pub fn with_notes(mut self, notes: Arc<Notes>) -> Self {
        self.notes = Some(notes);
        self
    }

    #[must_use]
    pub fn with_harness(mut self, harness: Arc<HarnessStore>) -> Self {
        self.harness = Some(harness);
        self
    }

    #[must_use]
    pub fn with_goal(mut self, goal: Arc<GoalStore>) -> Self {
        self.goal = Some(goal);
        self
    }

    #[must_use]
    pub fn with_scheduler(mut self, scheduler: Arc<Scheduler>) -> Self {
        self.scheduler = Some(scheduler);
        self
    }

    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    fn rlm_heartbeats(&self) -> Result<(&Arc<Scheduler>, Vec<Job>), HostError> {
        let scheduler = self
            .scheduler
            .as_ref()
            .ok_or("heartbeats are not available here")?;
        let jobs = scheduler
            .jobs()
            .into_iter()
            .filter(|job| job.source == JobSource::RlmHeartbeat)
            .collect();
        Ok((scheduler, jobs))
    }

    fn goal_store(&self) -> Result<&Arc<GoalStore>, HostError> {
        Ok(self.goal.as_ref().ok_or("goals are not available here")?)
    }

    fn harness_store(&self) -> Result<&Arc<HarnessStore>, HostError> {
        Ok(self
            .harness
            .as_ref()
            .ok_or("the harness is not available here")?)
    }

    fn commands(&self, missing: &'static str) -> Result<&Arc<HostCommands>, HostError> {
        Ok(self.host_commands.as_ref().ok_or(missing)?)
    }

    async fn spawn(&self, payload: Payload<'_>) -> Result<Value, HostError> {
        let prompt = payload.required("prompt")?;
        let name = payload.required("name")?;
        let thinking = requested_thinking(payload.text("thinking"))?;
        let handle = self
            .family
            .spawn(
                &self.identity,
                prompt,
                name,
                payload.text("model"),
                thinking,
                &self.family,
            )
            .await?;
        Ok(to_json(handle))
    }

    async fn collect(&self, payload: Payload<'_>) -> Result<Value, HostError> {
        let timeout = Duration::from_millis(payload.number("timeout_ms").unwrap_or(0));
        let children = self
            .family
            .collect(&self.identity, payload.targets(), timeout)
            .await?;
        Ok(summaries(&children))
    }

    fn set_paused(&self, payload: Payload<'_>, pausing: bool) -> Result<Value, HostError> {
        let pause = pausing.then(|| mush_session::Pause {
            at: Timestamp::now(),
            reason: payload.text("reason"),
            by: Some(self.identity.name.clone()),
        });
        let children =
            self.family
                .set_paused(&self.identity, &payload.targets(), pause.as_ref())?;
        Ok(summaries(&children))
    }

    fn progress_note(&self, payload: Payload<'_>) -> Result<Value, HostError> {
        let text = payload.required("text")?;
        Ok(match self.family.progress_note(&self.identity, &text)? {
            None => json!({ "accepted": true }),
            Some(retry) => json!({
                "accepted": false,
                "retry_after_ms": u64::try_from(retry.as_millis()).unwrap_or(u64::MAX),
            }),
        })
    }

    async fn send_message(&self, payload: Payload<'_>) -> Result<Value, HostError> {
        let message = payload.required("message")?;
        let receiver_role = payload.text("receiver_role");
        let receiver_name = payload.text("receiver_name");
        match payload.text("target").as_deref() {
            Some("all") => {
                if receiver_role.is_some() || receiver_name.is_some() {
                    return Err(HostError(
                        "agent_message.send broadcast cannot be combined with \
                         receiver_role/receiver_name"
                            .into(),
                    ));
                }
                let broadcast = self
                    .family
                    .broadcast(&self.identity, &message, &self.family)
                    .await?;
                Ok(json!({
                    "skipped_hibernating": broadcast.hibernating,
                    "receipts": broadcast
                        .receipts
                        .into_iter()
                        .map(|(target, receipt)| match receipt {
                            Ok(receipt) => json!(receipt),
                            Err(error) => json!({ "target": target, "error": error }),
                        })
                        .collect::<Vec<_>>()
                }))
            }
            Some(_) => Err(HostError(
                "positional agent_message.send targets are not supported; use \
                 receiver_role and receiver_name"
                    .into(),
            )),
            None => {
                let receiver_role = receiver_role
                    .ok_or(r#"receiver_role must be "parent", "sibling", or "child""#)?;
                let receipt = self
                    .family
                    .send_message(
                        &self.identity,
                        &receiver_role,
                        receiver_name.as_deref(),
                        &message,
                        &self.family,
                    )
                    .await?;
                Ok(json!(receipt))
            }
        }
    }

    fn compaction_status(&self) -> Result<Value, HostError> {
        let commands = self.commands("compaction is not available here")?;
        let usage = commands.context_usage();
        Ok(json!({
            "tokens": usage.tokens,
            "context_window": usage.context_window,
            "percent": usage.percent(),
            "scheduled": commands.compaction_queued(),
        }))
    }

    fn compact(&self, payload: Payload<'_>) -> Result<Value, HostError> {
        if !self.compaction_callable {
            return Err(HostError(
                "compaction from the agent is turned off (compaction.agent_callable)".into(),
            ));
        }
        let commands = self.commands("compaction is not available here")?;
        if commands.compaction_queued() {
            return Ok(json!({
                "scheduled": false,
                "note": "a compaction is already queued for the end of this turn",
            }));
        }
        let focus = mush_session::compact::CompactionFocus {
            context_budget: None,
            instructions: payload.instructions(),
        };
        commands.push(ClientCommand::Compact { focus, auto: false });
        Ok(json!({ "scheduled": true, "note": "compaction runs after this turn ends" }))
    }

    fn refine(&self, payload: Payload<'_>, scheduling: bool) -> Result<Value, HostError> {
        let commands = self.commands("refinement is not available here")?;
        let pending = commands.pending_refine();
        if !scheduling {
            return Ok(json!({ "pending": pending, "in_flight": false }));
        }
        commands.replace_refine(mush_session::refine::RefineRequest {
            instructions: payload.instructions(),
            rollback: None,
            global: payload.flag("global"),
            source: mush_session::harness::RefinementSource::Agent,
        });
        Ok(
            json!({ "scheduled": true, "note": "refinement runs after this turn ends; applied edits reach your context as a refinement notice" }),
        )
    }

    async fn recent_messages(&self, payload: Payload<'_>) -> Result<Value, HostError> {
        let target = payload.required("target")?;
        let limit = payload
            .number("limit")
            .map_or(OBSERVE_DEFAULT_LIMIT, |value| value as usize);
        let max_chars = payload
            .number("max_chars")
            .map_or(OBSERVE_DEFAULT_CHARS, |value| value as usize);
        let messages = self
            .family
            .recent_messages(&self.identity, &target, limit, max_chars)
            .await?;
        Ok(json!({ "messages": messages }))
    }

    async fn create_session(&self, payload: Payload<'_>) -> Result<Value, HostError> {
        if self.identity.depth > 0 {
            return Err(HostError(
                "only the root agent starts new sessions; ask your parent".into(),
            ));
        }
        let spawner = self
            .sessions
            .as_ref()
            .ok_or("new sessions are not available here")?;
        let prompt = payload.required("prompt")?;
        let created = (spawner)(CreateSessionRequest {
            prompt,
            name: payload.text("name"),
            model: payload.text("model"),
            thinking: requested_thinking(payload.text("thinking"))?,
            cwd: payload.text("cwd").map(PathBuf::from),
        })
        .await?;
        Ok(to_json(created))
    }

    fn notes(&self, kind: &str, payload: Payload<'_>) -> Result<Value, HostError> {
        let notes = self.notes.as_ref().ok_or("notes are not available here")?;
        let text = match kind {
            "rlm.note" => notes.append(&payload.required("text")?)?,
            "rlm.clear_notes" => {
                notes.replace("");
                String::new()
            }
            _ => notes.read(),
        };
        Ok(json!({ "notes": text }))
    }

    fn list_heartbeats(&self, payload: Payload<'_>) -> Result<Value, HostError> {
        let (_, jobs) = self.rlm_heartbeats()?;
        let include_inactive = payload.flag("include_inactive");
        let jobs: Vec<_> = jobs
            .into_iter()
            .filter(|job| {
                include_inactive || matches!(job.status, JobStatus::Active | JobStatus::Paused)
            })
            .collect();
        Ok(to_json(jobs))
    }

    fn create_heartbeat(&self, payload: Payload<'_>) -> Result<Value, HostError> {
        let (scheduler, _) = self.rlm_heartbeats()?;
        let prompt = payload
            .text("instruction")
            .or_else(|| payload.text("prompt"))
            .ok_or("instruction is required")?;
        let delivery = heartbeat_delivery(payload.text("delivery_mode"))?;
        let expression =
            heartbeat_interval(payload.text("interval").unwrap_or_else(|| "5m".into()));
        let jobs = scheduler.apply(ScheduleCommand::Add {
            expression,
            prompt,
            label: payload.text("label"),
            source: JobSource::RlmHeartbeat,
        })?;
        let mut created = jobs
            .iter()
            .rev()
            .find(|job| job.source == JobSource::RlmHeartbeat)
            .cloned();
        if let (Some(job), Some(delivery)) = (&created, delivery) {
            let jobs = scheduler.apply(ScheduleCommand::Edit {
                id: job.id.clone(),
                prompt: None,
                expression: None,
                label: None,
                delivery: Some(delivery),
            })?;
            let id = job.id.clone();
            created = jobs.into_iter().find(|job| job.id == id);
        }
        Ok(to_json(created))
    }

    fn change_heartbeat(&self, payload: Payload<'_>, deleting: bool) -> Result<Value, HostError> {
        let (scheduler, jobs) = self.rlm_heartbeats()?;
        let id = payload.required("id")?;
        if !jobs.iter().any(|job| job.id == id) {
            return Err(HostError(format!("no rlm heartbeat {id}")));
        }
        let commands = if deleting {
            vec![ScheduleCommand::Cancel(id.clone())]
        } else {
            heartbeat_updates(payload, &id)?
        };
        let mut jobs = Vec::new();
        for command in commands {
            jobs = scheduler.apply(command)?;
        }
        Ok(to_json(jobs.into_iter().find(|job| job.id == id)))
    }

    fn create_goal(&self, payload: Payload<'_>) -> Result<Value, HostError> {
        let objective = payload.required("objective")?;
        let token_budget = match payload.0.get("token_budget") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .filter(|budget| *budget > 0)
                    .ok_or("token_budget must be a positive integer")?,
            ),
        };
        self.goal_store()?
            .create_from_host(objective, token_budget)?;
        Ok(self.goal_reply(false))
    }

    fn complete_goal(&self, payload: Payload<'_>) -> Result<Value, HostError> {
        self.goal_store()?.apply(GoalCommand::Complete {
            summary: payload.text("summary"),
        })?;
        Ok(self.goal_reply(true))
    }

    fn harness_call(&self, kind: &str, payload: Payload<'_>) -> Result<Value, HostError> {
        use mush_session::harness::{Action, Edit, Kind as EntryKind};

        let store = self.harness_store()?;
        let global = payload.flag("global");
        let entry_kind = || -> Result<Option<EntryKind>, HostError> {
            let Some(raw) = payload.text("kind") else {
                return Ok(None);
            };
            EntryKind::parse(&raw).map(Some).ok_or_else(|| {
                HostError(format!(
                    "kind must be prompt, memory, skill or subagent, not {raw}"
                ))
            })
        };
        let required_kind =
            || -> Result<EntryKind, HostError> { Ok(entry_kind()?.ok_or("kind is required")?) };
        let entries = |entries: Vec<mush_session::harness::Entry>| json!({ "entries": entries });

        match kind {
            "harness.list" => Ok(entries(store.list(entry_kind()?, global))),
            "harness.overview" => Ok(json!({ "overview": store.overview(global) })),
            "harness.search" => {
                let query = payload.required("query")?;
                let limit = payload.number("limit").unwrap_or(10).clamp(1, 100) as usize;
                Ok(entries(store.search(&query, entry_kind()?, limit, global)))
            }
            "harness.record_refinement" => {
                let trigger = payload.required("trigger")?;
                let changes = match payload.0.get("changes") {
                    Some(Value::String(one)) => vec![one.clone()],
                    Some(Value::Array(many)) => many
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    _ => Vec::new(),
                };
                let event = mush_session::harness::RefinementEvent {
                    id: payload
                        .text("id")
                        .unwrap_or_else(mush_session::harness::new_id),
                    trigger,
                    changes,
                    evidence: payload.text("evidence").unwrap_or_default(),
                    outcome: payload.text("outcome").unwrap_or_default(),
                    created_at: mush_session::harness::now(),
                };
                let recorded = store.record_refinement(global, event)?;
                Ok(json!({ "refinement": recorded }))
            }
            "harness.get" => {
                let found = store.find(required_kind()?, &payload.required("id")?, global);
                Ok(json!({ "entry": found.map(|(_, entry)| entry) }))
            }
            "harness.create" | "harness.update" => {
                let creating = kind == "harness.create";
                let edit = Edit {
                    action: if creating {
                        Action::Create
                    } else {
                        Action::Update
                    },
                    kind: required_kind()?,
                    id: payload.text("id"),
                    title: payload.text("title"),
                    content: payload.text("content"),
                    path: payload.text("path"),
                    reference: payload.object("reference"),
                    arguments: payload.object("arguments"),
                    metadata: payload.object("metadata"),
                    reason: None,
                };
                let entry = if creating {
                    store.create(global, &edit)
                } else {
                    store.update(&payload.required("id")?, global, &edit)
                }?;
                Ok(json!({ "entry": entry }))
            }
            "harness.delete" => Ok(json!({
                "deleted": store
                    .delete(required_kind()?, &payload.required("id")?, global)
                    .is_ok(),
            })),
            other => Err(HostError(format!("unknown request {other}"))),
        }
    }

    fn goal_reply(&self, completing: bool) -> Value {
        let goal = self.goal.as_ref().and_then(|goal| goal.get());
        let report = goal
            .as_ref()
            .filter(|_| completing)
            .and_then(Goal::completion_budget_report);
        let remaining = goal.as_ref().and_then(Goal::remaining_tokens);
        let goal = goal.map(|goal| {
            let seconds = goal.elapsed_secs();
            let mut value = to_json(goal);
            if let Some(object) = value.as_object_mut() {
                object.insert("time_used_seconds".into(), seconds.into());
            }
            value
        });
        json!({
            "goal": goal,
            "remaining_tokens": remaining,
            "completion_budget_report": report,
        })
    }
}

fn heartbeat_updates(payload: Payload<'_>, id: &str) -> Result<Vec<ScheduleCommand>, HostError> {
    let mut commands = Vec::new();
    let prompt = payload
        .text("instruction")
        .or_else(|| payload.text("prompt"));
    let expression = payload.text("interval").map(heartbeat_interval);
    let label = payload.text("label");
    let delivery = heartbeat_delivery(payload.text("delivery_mode"))?;
    if prompt.is_some() || expression.is_some() || label.is_some() || delivery.is_some() {
        commands.push(ScheduleCommand::Edit {
            id: id.to_owned(),
            prompt,
            expression,
            label,
            delivery,
        });
    }
    match payload.text("status").as_deref() {
        None => {}
        Some("pause" | "paused") => commands.push(ScheduleCommand::Pause(id.to_owned())),
        Some("resume" | "active") => commands.push(ScheduleCommand::Resume(id.to_owned())),
        Some("cancel" | "cancelled" | "clear") => {
            commands.push(ScheduleCommand::Cancel(id.to_owned()));
        }
        Some(other) => {
            return Err(HostError(format!(
                "status must be pause or resume, not {other}"
            )));
        }
    }
    if commands.is_empty() {
        return Err(HostError(
            "update needs instruction, interval, label, status or delivery_mode".into(),
        ));
    }
    Ok(commands)
}

fn heartbeat_interval(interval: String) -> String {
    if interval.trim_start().starts_with("every ") {
        interval
    } else {
        format!("every {interval}")
    }
}

fn heartbeat_delivery(mode: Option<String>) -> Result<Option<Delivery>, HostError> {
    match mode.as_deref() {
        None => Ok(None),
        Some("steer") => Ok(Some(Delivery::Steer)),
        Some("follow_up") => Ok(Some(Delivery::FollowUp)),
        Some(other) => Err(HostError(format!(
            "delivery_mode must be steer or follow_up, not {other}"
        ))),
    }
}

fn requested_thinking(level: Option<String>) -> Result<Option<ThinkingLevel>, HostError> {
    match level.as_deref().map(ThinkingLevel::from_effort_str) {
        Some(ThinkingLevel::Other(other)) => Err(HostError(format!(
            "thinking must be off, minimal, low, medium, high or xhigh, not {other}"
        ))),
        known => Ok(known),
    }
}

fn summary_json(child: &ChildSummary) -> Value {
    let mut json = to_json(child);
    if let Some(fields) = json.as_object_mut() {
        fields.insert("state".into(), json!(child.activity(Timestamp::now())));
    }
    json
}

#[async_trait::async_trait]
impl mush_kernel::HostRequestHandler for RlmHandler {
    async fn handle(&self, kind: &str, payload: Value) -> Result<Value, HostError> {
        let payload = Payload(&payload);
        match kind {
            "rlm.spawn" => self.spawn(payload).await,
            "rlm.list_subagents" => Ok(summaries(&self.family.children_of(&self.identity))),
            "rlm.collect" => self.collect(payload).await,
            "rlm.pause_subagents" => self.set_paused(payload, true),
            "rlm.resume_subagents" => self.set_paused(payload, false),
            "rlm.delete_subagent" => {
                let child = self
                    .family
                    .delete(&self.identity, &payload.required("target")?)
                    .await?;
                Ok(summary_json(&child))
            }
            "rlm.progress_note" => self.progress_note(payload),
            "agent_message.send" => self.send_message(payload).await,
            "compact.status" => self.compaction_status(),
            "compact.run" => self.compact(payload),
            "harness.list"
            | "harness.get"
            | "harness.search"
            | "harness.overview"
            | "harness.create"
            | "harness.update"
            | "harness.delete"
            | "harness.record_refinement" => self.harness_call(kind, payload),
            "refine.run" => self.refine(payload, true),
            "refine.status" => self.refine(payload, false),
            "agent_observe.list_agents" => Ok(json!({
                "agents": self.family.roster(&self.identity),
            })),
            "agent_observe.get_agent" => {
                let agent = self
                    .family
                    .get_agent(&self.identity, &payload.required("target")?)?;
                Ok(json!({ "agent": agent }))
            }
            "agent_observe.recent_messages" => self.recent_messages(payload).await,
            "rlm.create_session" => self.create_session(payload).await,
            "rlm.note" | "rlm.notes" | "rlm.clear_notes" => self.notes(kind, payload),
            "rlm_heartbeat.list" => self.list_heartbeats(payload),
            "rlm_heartbeat.create" => self.create_heartbeat(payload),
            "rlm_heartbeat.update" => self.change_heartbeat(payload, false),
            "rlm_heartbeat.delete" => self.change_heartbeat(payload, true),
            "goal.get" => Ok(self.goal_reply(false)),
            "goal.create" => self.create_goal(payload),
            "goal.complete" => self.complete_goal(payload),
            other => Err(HostError(format!("unknown host request {other}"))),
        }
    }
}
