use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use clap_complete::aot::{Bash, Elvish, Fish, Generator, Zsh};
use clap_complete::engine::CompletionCandidate;
use clap_complete::env::{self, EnvCompleter, Shells};
use clap_complete_nushell::Nushell;
use mush_daemon::WorkerDescriptor;
use mush_runtime::{Job, JobStatus};
use mush_session::SessionStore;

const NEWEST_SESSIONS: usize = 200;
const DAEMON_WAIT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Nushell,
    Elvish,
}

pub fn write(shell: Shell, command: clap::Command, out: &mut dyn Write) -> std::io::Result<()> {
    let mut command = match shell {
        Shell::Nushell => optional_values_as_switches(command),
        _ => command,
    };
    command.set_bin_name(command.get_name().to_owned());
    command.build();
    let generator: &dyn Generator = match shell {
        Shell::Bash => &Bash,
        Shell::Zsh => &Zsh,
        Shell::Fish => &Fish,
        Shell::Nushell => &Nushell,
        Shell::Elvish => &Elvish,
    };
    generator.try_generate(&command, out)
}

// nushell externs can't express an optional flag value, and a typed flag rejects a bare --resume
fn optional_values_as_switches(command: clap::Command) -> clap::Command {
    command
        .mut_args(|arg| match arg.get_num_args() {
            Some(range) if range.min_values() == 0 && range.takes_values() => arg.num_args(0),
            _ => arg,
        })
        .mut_subcommands(optional_values_as_switches)
}

pub const SHELLS: Shells<'static> = Shells(&[
    &env::Bash,
    &env::Elvish,
    &env::Fish,
    &env::Powershell,
    &AutoloadedZsh,
]);

struct AutoloadedZsh;

impl EnvCompleter for AutoloadedZsh {
    fn name(&self) -> &'static str {
        env::Zsh.name()
    }

    fn is(&self, name: &str) -> bool {
        env::Zsh.is(name)
    }

    fn write_registration(
        &self,
        var: &str,
        name: &str,
        bin: &str,
        completer: &str,
        buf: &mut dyn Write,
    ) -> std::io::Result<()> {
        env::Zsh.write_registration(var, name, bin, completer, buf)?;
        let name = name.replace('-', "_");
        writeln!(
            buf,
            "if [[ ${{funcstack[1]}} = _{bin} ]]; then _clap_dynamic_completer_{name} \"$@\"; fi"
        )
    }

    fn write_complete(
        &self,
        cmd: &mut clap::Command,
        args: Vec<OsString>,
        current_dir: Option<&Path>,
        buf: &mut dyn Write,
    ) -> std::io::Result<()> {
        env::Zsh.write_complete(cmd, args, current_dir, buf)
    }
}

pub fn saved_sessions(current: &OsStr) -> Vec<CompletionCandidate> {
    session_candidates(
        &SessionStore::default_dir(),
        &current.to_string_lossy(),
        NEWEST_SESSIONS,
    )
}

pub fn live_agents(current: &OsStr) -> Vec<CompletionCandidate> {
    agent_candidates(&live(), &current.to_string_lossy(), false)
}

pub fn live_agents_and_children(current: &OsStr) -> Vec<CompletionCandidate> {
    agent_candidates(&live(), &current.to_string_lossy(), true)
}

pub fn scheduled_jobs(current: &OsStr) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy();
    live()
        .iter()
        .flat_map(|worker| {
            let jobs = SessionStore::default_artifact_dir(&worker.session_id.as_str().into())
                .join("scheduled-jobs.json");
            job_candidates(worker, &jobs_at(&jobs), &prefix)
        })
        .collect()
}

fn live() -> Vec<WorkerDescriptor> {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Vec::new();
    };
    runtime
        .block_on(async { tokio::time::timeout(DAEMON_WAIT, crate::agents::live()).await })
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

fn session_candidates(dir: &Path, prefix: &str, newest: usize) -> Vec<CompletionCandidate> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .is_some_and(|id| id.starts_with(prefix))
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files
        .into_iter()
        .take(newest)
        .filter_map(|(_, path)| mush_session::load_meta(&path).ok())
        .filter(mush_tui::session_picker::listed)
        .map(|meta| {
            CompletionCandidate::new(meta.id.as_str())
                .help(Some(meta.title.unwrap_or(meta.cwd).into()))
        })
        .collect()
}

fn agent_candidates(
    workers: &[WorkerDescriptor],
    prefix: &str,
    children: bool,
) -> Vec<CompletionCandidate> {
    let mut candidates = Vec::new();
    for worker in workers {
        let title = worker_title(worker);
        if worker.session_id.starts_with(prefix) {
            candidates
                .push(CompletionCandidate::new(&worker.session_id).help(Some((&title).into())));
        }
        if children {
            candidates.extend(
                worker
                    .children
                    .iter()
                    .filter(|child| child.session_id.starts_with(prefix))
                    .map(|child| {
                        CompletionCandidate::new(&child.session_id)
                            .help(Some(format!("{} in {title}", child.name).into()))
                    }),
            );
        }
    }
    candidates
}

fn worker_title(worker: &WorkerDescriptor) -> String {
    worker
        .title
        .clone()
        .unwrap_or_else(|| worker.cwd.display().to_string())
}

fn jobs_at(path: &Path) -> Vec<Job> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn job_candidates(
    worker: &WorkerDescriptor,
    jobs: &[Job],
    prefix: &str,
) -> Vec<CompletionCandidate> {
    let title = worker_title(worker);
    jobs.iter()
        .filter(|job| matches!(job.status, JobStatus::Active | JobStatus::Paused))
        .filter(|job| job.id.starts_with(prefix))
        .map(|job| {
            let what = job
                .label
                .clone()
                .unwrap_or_else(|| job.prompt.lines().next().unwrap_or_default().to_owned());
            CompletionCandidate::new(&job.id).help(Some(
                format!("{what} ({}) on {title}", job.expression).into(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_daemon::ChildDescriptor;
    use mush_runtime::{JobSource, ScheduleCommand, Scheduler};
    use mush_session::{Lifecycle, Session};

    fn saved(store: &SessionStore, id: &str, title: Option<&str>, age_secs: u64) -> Session {
        let mut session = Session::new("model", "/work/place");
        session.meta.id = id.into();
        session.meta.title = title.map(str::to_owned);
        store.save(&session).unwrap();
        let modified = SystemTime::now() - Duration::from_secs(age_secs);
        std::fs::File::options()
            .write(true)
            .open(store.session_path(&session.meta.id))
            .unwrap()
            .set_modified(modified)
            .unwrap();
        session
    }

    fn shown(candidates: &[CompletionCandidate]) -> Vec<(String, String)> {
        candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.get_value().to_string_lossy().into_owned(),
                    candidate
                        .get_help()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                )
            })
            .collect()
    }

    #[test]
    fn sessions_complete_newest_first_with_what_the_picker_lists() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let older = saved(&store, "1111000000000000", Some("older"), 40);
        let archived = saved(&store, "2222000000000000", Some("archived"), 30);
        store
            .set_lifecycle(&archived.meta.id, Lifecycle::Archived)
            .unwrap();
        let untitled = saved(&store, "3333000000000000", None, 20);
        let mut child = Session::new("model", "/work/place");
        child.meta.parent = Some(older.meta.id.clone());
        store.save(&child).unwrap();

        assert_eq!(
            shown(&session_candidates(dir.path(), "", NEWEST_SESSIONS)),
            [
                (untitled.meta.id.to_string(), "/work/place".to_owned()),
                (older.meta.id.to_string(), "older".to_owned()),
            ]
        );
    }

    #[test]
    fn a_prefix_and_the_newest_bound_limit_which_sessions_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        saved(&store, "1111aaaa00000000", Some("oldest"), 30);
        saved(&store, "2222bbbb00000000", Some("middle"), 20);
        saved(&store, "2222cccc00000000", Some("newest"), 10);

        let titles = |prefix: &str, bound: usize| -> Vec<String> {
            shown(&session_candidates(dir.path(), prefix, bound))
                .into_iter()
                .map(|(_, title)| title)
                .collect()
        };
        assert_eq!(titles("", 2), ["newest", "middle"]);
        assert_eq!(titles("2222", 3), ["newest", "middle"]);
        assert_eq!(titles("2222", 1), ["newest"]);
        assert_eq!(titles("2222b", 3), ["middle"]);
        assert!(titles("3", 3).is_empty());
    }

    #[test]
    fn a_missing_session_directory_completes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(session_candidates(&dir.path().join("absent"), "", NEWEST_SESSIONS).is_empty());
    }

    #[test]
    fn live_agents_complete_by_prefix_and_children_only_where_asked() {
        let worker =
            |id: &str, title: Option<&str>, children: Vec<ChildDescriptor>| WorkerDescriptor {
                session_id: id.into(),
                title: title.map(str::to_owned),
                cwd: "/work/place".into(),
                children,
                ..WorkerDescriptor::default()
            };
        let child = ChildDescriptor {
            session_id: "aa22".into(),
            name: "scout".into(),
            ..ChildDescriptor::default()
        };
        let workers = [
            worker("aa11", Some("planner"), vec![child]),
            worker("bb11", None, Vec::new()),
        ];

        assert_eq!(
            shown(&agent_candidates(&workers, "", false)),
            [
                ("aa11".to_owned(), "planner".to_owned()),
                ("bb11".to_owned(), "/work/place".to_owned()),
            ]
        );
        assert_eq!(
            shown(&agent_candidates(&workers, "aa", true)),
            [
                ("aa11".to_owned(), "planner".to_owned()),
                ("aa22".to_owned(), "scout in planner".to_owned()),
            ]
        );
        assert!(agent_candidates(&workers, "cc", true).is_empty());
    }

    #[test]
    fn active_and_paused_jobs_complete_with_what_they_run_and_where() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scheduled-jobs.json");
        let scheduler = Scheduler::new(Some(path.clone()));
        let add = |expression: &str, prompt: &str, label: Option<&str>| {
            scheduler
                .apply(ScheduleCommand::Add {
                    expression: expression.into(),
                    prompt: prompt.into(),
                    label: label.map(str::to_owned),
                    source: JobSource::Schedule,
                })
                .unwrap()
                .last()
                .unwrap()
                .id
                .clone()
        };
        let build = add("every 10m", "check the build\nthen report", None);
        let nightly = add("every 1d", "tidy up", Some("nightly"));
        let gone = add("in 5m", "never mind", None);
        scheduler
            .apply(ScheduleCommand::Pause(nightly.clone()))
            .unwrap();
        scheduler.apply(ScheduleCommand::Cancel(gone)).unwrap();
        let worker = WorkerDescriptor {
            session_id: "aa11".into(),
            title: Some("planner".into()),
            ..WorkerDescriptor::default()
        };

        let jobs = jobs_at(&path);
        assert_eq!(
            shown(&job_candidates(&worker, &jobs, "")),
            [
                (
                    build.clone(),
                    "check the build (every 10m) on planner".to_owned()
                ),
                (nightly, "nightly (every 1d) on planner".to_owned()),
            ]
        );
        assert_eq!(shown(&job_candidates(&worker, &jobs, &build)).len(), 1);
        assert!(jobs_at(&dir.path().join("absent.json")).is_empty());
    }
}
