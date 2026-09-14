use std::path::PathBuf;

use mush_supervisor::status::{
    ReadError, RegistrationStore, ReviewSchema, ReviewSource, Source, TaskBinding,
};

use super::TuiConfig;
use crate::app::App;

pub(super) fn store(config: &TuiConfig) -> Result<RegistrationStore, ReadError> {
    RegistrationStore::new(
        mush_session::store::data_dir(),
        config.session_id.to_string(),
        config.cwd.clone(),
    )
}

pub(super) async fn command(app: &mut App, config: &TuiConfig, args: &str) {
    let result = parse(args);
    let edit = match result {
        Ok(Some(edit)) => edit,
        Ok(None) => {
            app.subagents.open = true;
            app.push_system_message("/subagents add /absolute/run\n/subagents remove /absolute/run\n/subagents review <root-number> /absolute/queue.json\n/subagents bind <root-number> <worker> <task-id>\n/subagents unbind <root-number> <worker>\n/subagents unreview <root-number>\n/subagents clear\nup to 64 explicit supervisor roots per session\nreview uses operator_queue_v1, task ids and supervisor associations\nregistrations persist for this session and working directory");
            return;
        }
        Err(()) => {
            app.push_system_message("invalid subagents command, use /subagents for help");
            return;
        }
    };
    let result = match store(config) {
        Ok(store) => tokio::task::spawn_blocking(move || {
            store.update(|r| {
                match edit {
                    Edit::Add(path) => {
                        if !r.roots.iter().any(|s| s.directory == path) {
                            r.roots.push(Source {
                                directory: path,
                                review_queue: None,
                                task_bindings: Vec::new(),
                            });
                        }
                    }
                    Edit::Remove(path) => r.roots.retain(|s| s.directory != path),
                    Edit::Review(index, path) => {
                        let source = r.roots.get_mut(index).ok_or(ReadError::Identity)?;
                        source.review_queue = path.map(|path| ReviewSource {
                            path,
                            schema: ReviewSchema::OperatorQueueV1,
                        });
                        source.task_bindings.clear();
                    }
                    Edit::Bind(index, worker, task_id) => {
                        let source = r.roots.get_mut(index).ok_or(ReadError::Identity)?;
                        source
                            .task_bindings
                            .retain(|binding| binding.worker != worker);
                        if let Some(task_id) = task_id {
                            source.task_bindings.push(TaskBinding { worker, task_id });
                        }
                    }
                    Edit::Clear => r.roots.clear(),
                }
                Ok(())
            })
        })
        .await
        .unwrap_or(Err(ReadError::Unavailable)),
        Err(error) => Err(error),
    };
    match result {
        Ok(_) => {
            app.subagents.invalidate();
            app.subagents.open = true;
        }
        Err(error) => app.push_system_message(format!("subagents registration: {error}")),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Edit {
    Add(PathBuf),
    Remove(PathBuf),
    Review(usize, Option<PathBuf>),
    Bind(usize, String, Option<String>),
    Clear,
}

fn parse(args: &str) -> Result<Option<Edit>, ()> {
    if args.len() > 8192 {
        return Err(());
    }
    let (command, rest) = crate::slash::split_name_and_args(args);
    let path = |s: &str| {
        let path = PathBuf::from(s);
        if path.is_absolute() {
            Ok(path)
        } else {
            Err(())
        }
    };
    match command {
        "" => Ok(None),
        "add" => path(rest).map(Edit::Add).map(Some),
        "remove" => path(rest).map(Edit::Remove).map(Some),
        "clear" if rest.is_empty() => Ok(Some(Edit::Clear)),
        "bind" | "unbind" => {
            let (number, rest) = crate::slash::split_name_and_args(rest);
            let index = number
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .ok_or(())?;
            let (worker, task_id) = crate::slash::split_name_and_args(rest);
            if !mush_supervisor::status::worker_name(worker) {
                return Err(());
            }
            let task_id = if command == "bind" && mush_supervisor::status::task_id(task_id) {
                Some(task_id.to_string())
            } else if command == "unbind" && task_id.is_empty() {
                None
            } else {
                return Err(());
            };
            Ok(Some(Edit::Bind(index, worker.to_string(), task_id)))
        }
        "review" => {
            let (number, rest) = crate::slash::split_name_and_args(rest);
            let index = number
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .ok_or(())?;
            Ok(Some(Edit::Review(index, Some(path(rest)?))))
        }
        "unreview" => {
            let index = rest
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .ok_or(())?;
            Ok(Some(Edit::Review(index, None)))
        }
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registration_commands_persist_with_spaces_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            RegistrationStore::new(dir.path().into(), "synthetic".into(), dir.path().into())
                .unwrap();
        let path = dir.path().join("run with spaces");
        assert_eq!(
            parse(&format!("add {}", path.display())).unwrap(),
            Some(Edit::Add(path.clone()))
        );
        assert_eq!(
            parse("review 1 /a queue.json").unwrap(),
            Some(Edit::Review(0, Some("/a queue.json".into())))
        );
        assert!(parse("review 0 /queue").is_err());
        assert!(parse("add relative").is_err());
        assert_eq!(
            parse("bind 2 worker task with spaces").unwrap(),
            Some(Edit::Bind(
                1,
                "worker".into(),
                Some("task with spaces".into())
            ))
        );
        assert_eq!(
            parse("unbind 2 worker").unwrap(),
            Some(Edit::Bind(1, "worker".into(), None))
        );
        assert!(parse("bind 0 worker task").is_err());
        assert!(parse("bind 1 ../worker task").is_err());
        assert!(parse("bind 1 worker").is_err());
        assert!(parse("bind 1 worker bad\u{1b}task").is_err());
        store
            .update(|r| {
                r.roots.push(Source {
                    directory: path,
                    review_queue: None,
                    task_bindings: Vec::new(),
                });
                Ok(())
            })
            .unwrap();
        assert_eq!(store.load().unwrap().roots.len(), 1);
        store
            .update(|r| {
                r.roots.clear();
                Ok(())
            })
            .unwrap();
        assert!(store.load().unwrap().roots.is_empty());
    }
}

#[cfg(test)]
mod pty;
