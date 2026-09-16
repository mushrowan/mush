use std::collections::BTreeMap;

use super::*;
use mush_supervisor::review::{Pair, Participant};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Generation {
    generation: u32,
    candidate: String,
    directory: PathBuf,
    config_sha256: String,
    pair: Pair,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    request: String,
    reason: String,
    expected: String,
    generation: u32,
    before: Pair,
    after: Pair,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Task {
    authority: PathBuf,
    original: Pair,
    pair: Pair,
    generations: Vec<Generation>,
    reassignments: Vec<Revision>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    tasks: BTreeMap<String, Task>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: 1,
            tasks: BTreeMap::new(),
        }
    }
}

pub(super) fn active(config: &Config) -> Result<()> {
    let ledger: Ledger = decode(&read(
        &config.admission.join("review-tasks.json"),
        8 * 1024 * 1024,
    )?)?;
    if ledger.version != 1 {
        return Err("unsupported review task ledger".into());
    }
    let task = ledger
        .tasks
        .get(&config.task)
        .ok_or("unknown review task")?;
    let last = task
        .generations
        .last()
        .ok_or("review task has no generation")?;
    if last.directory != config.directory
        || task.pair != last.pair
        || last.config_sha256 != digest(&serde_json::to_vec(config)?)
    {
        return Err("review generation or pair was superseded".into());
    }
    Ok(())
}

pub(super) struct Registry {
    directory: PathBuf,
    _lock: Option<Lock>,
    ledger: Ledger,
}

fn participant(
    id: &Identity,
    directory: &Path,
    workspace: &Path,
    manifest: &Path,
) -> Result<Participant> {
    for path in [directory, workspace] {
        if fs::canonicalize(path)? != path {
            return Err("pair paths must be canonical".into());
        }
    }
    Ok(Participant {
        run_id: id.run_id.clone(),
        worker: id.worker.clone(),
        session: id.session.clone(),
        directory: directory.into(),
        workspace: workspace.into(),
        manifest: manifest.into(),
    })
}

fn pair(config: &Config) -> Result<Pair> {
    let manifest: Manifest = decode(&read(&config.producer.manifest, 1024 * 1024)?)?;
    let worker = manifest
        .workers
        .iter()
        .find(|w| w.name == config.producer.identity.worker)
        .ok_or("paired producer missing")?;
    Ok(Pair {
        producer: participant(
            &config.producer.identity,
            &manifest.directory,
            &worker.cwd,
            &config.producer.manifest,
        )?,
        reviewer: participant(
            &config.reviewer,
            &config.reviewer_directory,
            &config.reviewer_cwd,
            &config.reviewer_directory.join("manifest.json"),
        )?,
    })
}

impl Registry {
    pub(super) fn open(directory: &Path) -> Result<Self> {
        Self::load(directory, true)
    }

    pub(super) fn view(directory: &Path) -> Result<Self> {
        Self::load(directory, false)
    }

    fn load(directory: &Path, writer: bool) -> Result<Self> {
        private_directory(directory)?;
        let lock = writer
            .then(|| Lock::acquire(&directory.join("review-tasks.lock")))
            .transpose()?;
        let path = directory.join("review-tasks.json");
        let ledger: Ledger = match read(&path, 8 * 1024 * 1024) {
            Ok(bytes) => decode(&bytes)?,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ledger::default()
            }
            Err(error) => return Err(error),
        };
        if ledger.version != 1 {
            return Err("unsupported review task ledger".into());
        }
        Ok(Self {
            directory: directory.into(),
            _lock: lock,
            ledger,
        })
    }

    pub(super) fn inspect(&self, task: &str) -> Result<Value> {
        let task = self
            .ledger
            .tasks
            .get(task)
            .ok_or("unknown task, establish its initial review pair")?;
        Ok(json!({"receipt":digest(&serde_json::to_vec(task)?),"history":task}))
    }

    pub(super) fn pair(&self, task: &str) -> Result<Pair> {
        Ok(self
            .ledger
            .tasks
            .get(task)
            .ok_or("unknown review task")?
            .pair
            .clone())
    }

    pub(super) fn latest(&self, task: &str) -> Result<PathBuf> {
        Ok(self
            .ledger
            .tasks
            .get(task)
            .and_then(|t| t.generations.last())
            .ok_or("unknown review task")?
            .directory
            .clone())
    }

    pub(super) fn generations(&self, task: &str) -> Result<Vec<PathBuf>> {
        Ok(self
            .ledger
            .tasks
            .get(task)
            .ok_or("unknown review task")?
            .generations
            .iter()
            .map(|g| g.directory.clone())
            .collect())
    }

    pub(super) fn historical_pair(&self, task: &str, generation: u32) -> Result<Pair> {
        Ok(self
            .ledger
            .tasks
            .get(task)
            .and_then(|t| t.generations.iter().find(|g| g.generation == generation))
            .ok_or("unknown review generation")?
            .pair
            .clone())
    }

    pub(super) fn current(&self, config: &Config) -> Result<()> {
        let task = self
            .ledger
            .tasks
            .get(&config.task)
            .ok_or("review task is not registered")?;
        let last = task
            .generations
            .last()
            .ok_or("review task has no generation")?;
        if last.directory != config.directory
            || last.generation != config.generation
            || last.config_sha256 != digest(&serde_json::to_vec(config)?)
            || task.pair != last.pair
        {
            return Err("review task generation or pair was superseded".into());
        }
        Ok(())
    }

    pub(super) fn check(&self, config: &Config) -> Result<()> {
        if !identifier(&config.task) || config.task.len() > 80 {
            return Err("invalid stable review task".into());
        }
        let proposed = pair(config)?;
        if let Some(prepared) = &config.rereview
            && (prepared.manifest != proposed.reviewer.manifest
                || prepared.head != config.candidate
                || config.producer.head != config.candidate
                || prepared.identity.run_id != config.reviewer.run_id
                || prepared.identity.worker != config.reviewer.worker
                || prepared.identity.session != config.reviewer.session
                || prepared.identity.round.checked_add(1) != Some(config.reviewer.round)
                || prepared.identity.assignment.checked_add(1) != Some(config.reviewer.assignment))
        {
            return Err("review preparation changed the stored pair or counters".into());
        }
        let Some(task) = self.ledger.tasks.get(&config.task) else {
            if config.generation != 1 || config.predecessor.is_some() {
                return Err("new review task must start at generation one".into());
            }
            if self
                .ledger
                .tasks
                .values()
                .any(|t| t.authority == config.authority_file)
            {
                return Err(
                    "authority already belongs to a review task, use continue-review".into(),
                );
            }
            return Ok(());
        };
        if task.pair != proposed {
            return Err("review pair changed, use review-reassign with expected receipt and context-loss reason".into());
        }
        if task.authority != config.authority_file {
            return Err("task authority location changed".into());
        }
        if let Some(existing) = task
            .generations
            .iter()
            .find(|g| g.generation == config.generation)
        {
            if existing.directory == config.directory
                && existing.config_sha256 == digest(&serde_json::to_vec(config)?)
            {
                return Ok(());
            }
            return Err("review generation already registered".into());
        }
        let last = task
            .generations
            .last()
            .ok_or("review task has no generation")?;
        if config.rereview.is_none() {
            return Err("next generation needs reviewer preparation, use continue-review".into());
        }
        if config.generation
            != last
                .generation
                .checked_add(1)
                .ok_or("review generation exhausted")?
            || config.predecessor.as_ref() != Some(&last.config_sha256)
        {
            return Err(
                "stale review predecessor, inspect review-pair and use continue-review".into(),
            );
        }
        let previous = load(&last.directory)?;
        if last.directory.join("STOP").exists() || last.directory.join("DISABLED").exists() {
            return Err("previous review generation is stopped or disabled".into());
        }
        let saved = state(&last.directory)?;
        if saved.capture.is_none()
            || saved.launch_claimed && saved.producer != "attempt_finished"
            || saved.review_launch_claimed && saved.reviewer_action != "attempt_finished"
        {
            return Err("previous review generation is incomplete or launch is unresolved".into());
        }
        if config.producer.identity.session == previous.producer.identity.session
            && config.producer.identity.round < previous.producer.identity.round
            || config.reviewer.session == previous.reviewer.session
                && (config.reviewer.round <= previous.reviewer.round
                    || config.reviewer.assignment <= previous.reviewer.assignment)
        {
            return Err("review counters did not advance".into());
        }
        let bytes = read(&last.directory.join("capture/verdict.json"), 256 * 1024)?;
        if saved.capture.as_ref() != Some(&digest(&bytes)) {
            return Err("previous review capture changed".into());
        }
        let verdict: Verdict = decode(&bytes)?;
        if matches!(
            verdict.result,
            VerdictResult::Blocked | VerdictResult::Error
        ) {
            return Err("existing review decision is blocked, resolve it before advancing".into());
        }
        Ok(())
    }

    pub(super) fn record(&mut self, config: &Config) -> Result<()> {
        self.check(config)?;
        let proposed = pair(config)?;
        if self.ledger.tasks.len() >= 128 && !self.ledger.tasks.contains_key(&config.task) {
            return Err("review task ledger is full".into());
        }
        let task = self
            .ledger
            .tasks
            .entry(config.task.clone())
            .or_insert_with(|| Task {
                authority: config.authority_file.clone(),
                original: proposed.clone(),
                pair: proposed,
                generations: vec![],
                reassignments: vec![],
            });
        if task
            .generations
            .iter()
            .any(|g| g.generation == config.generation)
        {
            return Ok(());
        }
        if task.generations.len() >= 128 {
            return Err("review generation budget exhausted".into());
        }
        task.generations.push(Generation {
            generation: config.generation,
            candidate: config.candidate.clone(),
            directory: config.directory.clone(),
            config_sha256: digest(&serde_json::to_vec(config)?),
            pair: task.pair.clone(),
        });
        self.save()
    }

    fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec(&self.ledger)?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("review task ledger is full".into());
        }
        replace(&self.directory.join("review-tasks.json"), &bytes)?;
        Ok(())
    }

    pub(super) fn reassign(&mut self, request: &Reassignment) -> Result<()> {
        let task = self
            .ledger
            .tasks
            .get_mut(&request.task)
            .ok_or("unknown review task")?;
        if let Some(old) = task
            .reassignments
            .iter()
            .find(|r| r.request == request.request)
        {
            if old.expected == request.expected
                && old.after == request.pair
                && old.reason == request.reason
                && old.generation == request.generation
            {
                return Ok(());
            }
            return Err("reassignment request id collision".into());
        }
        if !identifier(&request.request)
            || request.reason.trim().is_empty()
            || request.reason.len() > 4096
            || request.expected != digest(&serde_json::to_vec(task)?)
            || task
                .generations
                .last()
                .is_none_or(|g| g.generation != request.generation)
        {
            return Err("stale reassignment receipt or missing context-loss reason".into());
        }
        let last = task.generations.last().ok_or("missing review generation")?;
        let _router = Lock::acquire(&last.directory.join("router.lock"))?;
        let config = load(&last.directory)?;
        if stopped(&config) {
            return Err("review task is stopped or revoked".into());
        }
        let saved = state(&last.directory)?;
        let bytes = read(&last.directory.join("capture/verdict.json"), 256 * 1024)?;
        if saved.capture.as_ref() != Some(&digest(&bytes)) {
            return Err("previous review capture changed".into());
        }
        let verdict: Verdict = decode(&bytes)?;
        if matches!(
            verdict.result,
            VerdictResult::Blocked | VerdictResult::Error
        ) {
            return Err("blocked review cannot be bypassed by reassignment".into());
        }
        if saved.launch_claimed && saved.producer != "attempt_finished"
            || saved.review_launch_claimed && saved.reviewer_action != "attempt_finished"
        {
            return Err("reconcile the previous launch before reassignment".into());
        }
        let mut locks = Vec::new();
        let mut sessions = Vec::new();
        for actor in [
            &task.pair.producer,
            &task.pair.reviewer,
            &request.pair.producer,
            &request.pair.reviewer,
        ] {
            if !locks.iter().any(|(path, _)| path == &actor.directory) {
                locks.push((
                    actor.directory.clone(),
                    Lock::acquire(&actor.directory.join("supervisor.lock"))?,
                ));
            }
            if !sessions
                .iter()
                .any(|(path, session, _)| path == &actor.directory && session == &actor.session)
            {
                sessions.push((
                    actor.directory.clone(),
                    actor.session.clone(),
                    terminal(actor)?,
                ));
            }
        }
        if task.reassignments.len() >= 128 {
            return Err("reassignment history is full".into());
        }
        task.reassignments.push(Revision {
            request: request.request.clone(),
            reason: request.reason.clone(),
            expected: request.expected.clone(),
            generation: request.generation,
            before: task.pair.clone(),
            after: request.pair.clone(),
        });
        task.pair.clone_from(&request.pair);
        self.save()
    }
}

fn terminal(actor: &Participant) -> Result<File> {
    if !identifier(&actor.worker) || !identifier(&actor.run_id) || !identifier(&actor.session) {
        return Err("invalid pair participant".into());
    }
    for path in [&actor.directory, &actor.workspace, &actor.manifest] {
        if fs::canonicalize(path)? != *path {
            return Err("pair paths must be canonical".into());
        }
    }
    let manifest: Manifest = decode(&read(&actor.manifest, 1024 * 1024)?)?;
    let job = manifest
        .workers
        .iter()
        .find(|w| w.name == actor.worker)
        .ok_or("replacement worker missing")?;
    if manifest.directory != actor.directory
        || job.cwd != actor.workspace
        || actor.directory.join("STOP").exists()
    {
        return Err("pair location changed or worker is stopped".into());
    }
    let dir = actor.directory.join(&actor.worker);
    let state: Value = decode(&read(&dir.join("state.json"), 1024 * 1024)?)?;
    if state["state"] != "done"
        || state["session_id"] != actor.session
        || state["terminal_event"]["run_id"] != actor.run_id
        || state["terminal_event"]["reason"] != "completed"
    {
        return Err(
            "blocked, failed or missing worker cannot be reassigned, resolve its existing decision"
                .into(),
        );
    }
    let session = mush_supervisor::admission::session_lock(
        &dir.join("mush-data/sessions"),
        &actor.session,
        None,
    )?;
    read(
        &dir.join("mush-data/sessions")
            .join(format!("{}.json", actor.session)),
        64 * 1024 * 1024,
    )?;
    let round = state["round"]
        .as_u64()
        .ok_or("pair checkpoint round missing")?;
    let checkpoint: Value = decode(&read(
        &dir.join(format!("round-{round:03}.json")),
        1024 * 1024,
    )?)?;
    if checkpoint["session_id"] != actor.session || checkpoint["state"] != "completed" {
        return Err("pair checkpoint changed or is incomplete".into());
    }
    Ok(session)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reassignment {
    task: String,
    request: String,
    expected: String,
    generation: u32,
    reason: String,
    pair: Pair,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    round: u32,
    assignment: u32,
    manifest_sha256: String,
    executable_sha256: String,
    state_sha256: String,
    report_sha256: String,
    checkpoint_sha256: String,
    session_sha256: String,
    prompt_sha256: String,
    head: String,
    workspace_sha256: String,
    brief: String,
}

impl Snapshot {
    fn bind(self, actor: &Participant) -> Producer {
        Producer {
            manifest: actor.manifest.clone(),
            manifest_sha256: self.manifest_sha256,
            executable_sha256: self.executable_sha256,
            identity: Identity {
                run_id: actor.run_id.clone(),
                worker: actor.worker.clone(),
                session: actor.session.clone(),
                round: self.round,
                assignment: self.assignment,
            },
            state_sha256: self.state_sha256,
            report_sha256: self.report_sha256,
            checkpoint_sha256: self.checkpoint_sha256,
            session_sha256: self.session_sha256,
            prompt_sha256: self.prompt_sha256,
            head: self.head,
            workspace_sha256: self.workspace_sha256,
            correction: self.brief,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Continuation {
    directory: PathBuf,
    expected: String,
    candidate: String,
    until: DateTime<Utc>,
    producer: Snapshot,
    reviewer: Snapshot,
    artifacts: Vec<Artifact>,
    regression_commit: Option<String>,
}

pub(super) fn continuation(directory: &Path, path: &Path) -> Result<Config> {
    let previous = load(directory)?;
    let bytes = read(path, 256 * 1024)?;
    let request: Continuation = decode(&bytes)?;
    if request.candidate != request.producer.head || request.candidate != request.reviewer.head {
        return Err("successor candidate must match both prepared revisions".into());
    }
    if request.directory.exists() {
        let existing = load(&request.directory)?;
        if existing.continuation_sha256.as_ref() == Some(&digest(&bytes))
            && existing.predecessor.as_ref() == Some(&digest(&serde_json::to_vec(&previous)?))
        {
            return Ok(existing);
        }
        return Err("continuation differs from registered generation".into());
    }
    let registry = Registry::open(&previous.admission)?;
    if registry.inspect(&previous.task)?["receipt"] != request.expected {
        return Err("stale task receipt, inspect review-pair".into());
    }
    let pair = registry.pair(&previous.task)?;
    let mut next = previous.clone();
    next.directory = request.directory;
    next.predecessor = Some(digest(&serde_json::to_vec(&previous)?));
    next.generation = previous
        .generation
        .checked_add(1)
        .ok_or("review generation exhausted")?;
    next.candidate = request.candidate;
    next.until = request.until;
    next.producer = request.producer.bind(&pair.producer);
    let reviewer = request.reviewer.bind(&pair.reviewer);
    next.reviewer = reviewer.identity.clone();
    next.reviewer.round = next
        .reviewer
        .round
        .checked_add(1)
        .ok_or("review rounds exhausted")?;
    next.reviewer.assignment = next
        .reviewer
        .assignment
        .checked_add(1)
        .ok_or("review assignments exhausted")?;
    next.reviewer_directory = pair.reviewer.directory;
    next.reviewer_cwd = pair.reviewer.workspace;
    next.rereview = Some(reviewer);
    next.artifacts = request.artifacts;
    next.regression_commit = request.regression_commit;
    next.continuation_sha256 = Some(digest(&bytes));
    Ok(next)
}
