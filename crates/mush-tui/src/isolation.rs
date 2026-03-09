//! VCS-based file isolation for multi-pane agents
//!
//! provides worktree (git) and jj change isolation backends so forked
//! panes can work on separate filesystem snapshots without conflicts

use std::path::{Path, PathBuf};
use std::process::Output;

use thiserror::Error;

use crate::pane::PaneId;

/// per-pane isolation state, stored on the pane
#[derive(Debug, Clone)]
pub enum PaneIsolation {
    /// git worktree at a specific path with a branch name
    Worktree { path: PathBuf, branch: String },
    /// jj change with a specific change id
    Jj { change_id: String },
}

/// result of creating a worktree
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
}

/// result of creating a jj change
pub struct JjChangeInfo {
    pub change_id: String,
}

#[derive(Debug, Error)]
pub enum IsolationError {
    #[error("failed to {action}: {source}")]
    Io {
        action: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to {action}: {detail}")]
    CommandFailed {
        action: &'static str,
        detail: String,
    },
    #[error("got empty change id from jj")]
    EmptyChangeId,
}

fn command_detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }

    format!("exit status {}", output.status)
}

async fn run_command(
    command: &mut tokio::process::Command,
    action: &'static str,
) -> Result<Output, IsolationError> {
    command
        .output()
        .await
        .map_err(|source| IsolationError::Io { action, source })
}

fn ensure_success(output: Output, action: &'static str) -> Result<Output, IsolationError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(IsolationError::CommandFailed {
            action,
            detail: command_detail(&output),
        })
    }
}

// -- worktree operations --

fn worktree_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".mush").join("worktrees")
}

fn worktree_path(repo_root: &Path, pane_id: PaneId) -> PathBuf {
    worktree_dir(repo_root).join(format!("pane-{}", pane_id.as_u32()))
}

fn worktree_branch(pane_id: PaneId) -> String {
    format!("mush-pane-{}", pane_id.as_u32())
}

pub async fn create_worktree(
    repo_root: &Path,
    pane_id: PaneId,
) -> Result<WorktreeInfo, IsolationError> {
    let wt_path = worktree_path(repo_root, pane_id);
    let branch = worktree_branch(pane_id);

    tokio::fs::create_dir_all(worktree_dir(repo_root))
        .await
        .map_err(|source| IsolationError::Io {
            action: "create worktree dir",
            source,
        })?;

    if wt_path.exists() {
        let _ = remove_worktree(repo_root, pane_id).await;
    }

    let output = {
        let mut command = tokio::process::Command::new("git");
        command
            .args(["worktree", "add", "-b", branch.as_str()])
            .arg(&wt_path)
            .arg("HEAD")
            .current_dir(repo_root);
        run_command(&mut command, "run git worktree add").await?
    };

    if output.status.success() {
        return Ok(WorktreeInfo {
            path: wt_path,
            branch,
        });
    }

    let detail = command_detail(&output);
    if detail.contains("already exists") {
        let mut delete_branch = tokio::process::Command::new("git");
        delete_branch
            .args(["branch", "-D", branch.as_str()])
            .current_dir(repo_root);
        let _ = run_command(&mut delete_branch, "delete stale worktree branch").await;

        let retry = {
            let mut command = tokio::process::Command::new("git");
            command
                .args(["worktree", "add", "-b", branch.as_str()])
                .arg(&wt_path)
                .arg("HEAD")
                .current_dir(repo_root);
            run_command(&mut command, "run git worktree add").await?
        };
        ensure_success(retry, "run git worktree add")?;
    } else {
        return Err(IsolationError::CommandFailed {
            action: "run git worktree add",
            detail,
        });
    }

    Ok(WorktreeInfo {
        path: wt_path,
        branch,
    })
}

pub async fn remove_worktree(repo_root: &Path, pane_id: PaneId) -> Result<(), IsolationError> {
    let wt_path = worktree_path(repo_root, pane_id);
    let branch = worktree_branch(pane_id);

    let output = {
        let mut command = tokio::process::Command::new("git");
        command
            .args(["worktree", "remove", "--force"])
            .arg(&wt_path)
            .current_dir(repo_root);
        run_command(&mut command, "run git worktree remove").await?
    };

    if !output.status.success() {
        let mut prune = tokio::process::Command::new("git");
        prune.args(["worktree", "prune"]).current_dir(repo_root);
        let _ = run_command(&mut prune, "run git worktree prune").await;
        let _ = tokio::fs::remove_dir_all(&wt_path).await;
    }

    let mut delete_branch = tokio::process::Command::new("git");
    delete_branch
        .args(["branch", "-D", branch.as_str()])
        .current_dir(repo_root);
    let _ = run_command(&mut delete_branch, "delete worktree branch").await;

    Ok(())
}

pub async fn merge_worktree(repo_root: &Path, pane_id: PaneId) -> Result<String, IsolationError> {
    let branch = worktree_branch(pane_id);

    let output = {
        let mut command = tokio::process::Command::new("git");
        command
            .args(["merge", "--no-edit", branch.as_str()])
            .current_dir(repo_root);
        run_command(&mut command, "run git merge").await?
    };
    let output = ensure_success(output, "merge worktree branch")?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();

    Ok(format!("merged {branch}: {stdout}"))
}

pub async fn cleanup_stale_worktrees(repo_root: &Path) -> usize {
    let wt_dir = worktree_dir(repo_root);
    if !wt_dir.exists() {
        return 0;
    }

    let mut prune = tokio::process::Command::new("git");
    prune.args(["worktree", "prune"]).current_dir(repo_root);
    let _ = run_command(&mut prune, "run git worktree prune").await;

    let mut cleaned = 0;
    if let Ok(mut entries) = tokio::fs::read_dir(&wt_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("pane-")
                && let Ok(metadata) = entry.metadata().await
                && metadata.is_dir()
                && let Some(id_str) = name_str.strip_prefix("pane-")
                && let Ok(id) = id_str.parse::<u32>()
            {
                let _ = remove_worktree(repo_root, PaneId::new(id)).await;
                cleaned += 1;
            }
        }
    }

    if let Ok(mut entries) = tokio::fs::read_dir(&wt_dir).await
        && entries.next_entry().await.ok().flatten().is_none()
    {
        let _ = tokio::fs::remove_dir(&wt_dir).await;
        let _ = tokio::fs::remove_dir(repo_root.join(".mush")).await;
    }

    cleaned
}

// -- jj operations --

pub async fn create_jj_change(repo_root: &Path) -> Result<JjChangeInfo, IsolationError> {
    let output = {
        let mut command = tokio::process::Command::new("jj");
        command
            .args(["new", "@", "-m", "mush agent working change"])
            .current_dir(repo_root);
        run_command(&mut command, "run jj new").await?
    };
    ensure_success(output, "run jj new")?;

    let id_output = {
        let mut command = tokio::process::Command::new("jj");
        command
            .args(["log", "-r", "@", "--no-graph", "-T", "change_id"])
            .current_dir(repo_root);
        run_command(&mut command, "get jj change id").await?
    };
    let id_output = ensure_success(id_output, "get jj change id")?;

    let change_id = String::from_utf8_lossy(&id_output.stdout)
        .trim()
        .to_string();
    if change_id.is_empty() {
        return Err(IsolationError::EmptyChangeId);
    }

    Ok(JjChangeInfo { change_id })
}

pub async fn edit_jj_change(repo_root: &Path, change_id: &str) -> Result<(), IsolationError> {
    let output = {
        let mut command = tokio::process::Command::new("jj");
        command.args(["edit", change_id]).current_dir(repo_root);
        run_command(&mut command, "run jj edit").await?
    };
    ensure_success(output, "run jj edit")?;
    Ok(())
}

pub async fn squash_jj_change(repo_root: &Path, change_id: &str) -> Result<String, IsolationError> {
    let output = {
        let mut command = tokio::process::Command::new("jj");
        command
            .args(["squash", "-r", change_id])
            .current_dir(repo_root);
        run_command(&mut command, "run jj squash").await?
    };
    let output = ensure_success(output, "run jj squash")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub async fn abandon_jj_change(repo_root: &Path, change_id: &str) -> Result<(), IsolationError> {
    let output = {
        let mut command = tokio::process::Command::new("jj");
        command.args(["abandon", change_id]).current_dir(repo_root);
        run_command(&mut command, "run jj abandon").await?
    };
    ensure_success(output, "run jj abandon")?;
    Ok(())
}

pub async fn describe_jj_change(
    repo_root: &Path,
    change_id: &str,
) -> Result<String, IsolationError> {
    let output = {
        let mut command = tokio::process::Command::new("jj");
        command
            .args([
                "log",
                "-r",
                change_id,
                "--no-graph",
                "-T",
                r#"separate("\n", change_id.shortest(8), description, if(empty, "(empty)"))"#,
            ])
            .current_dir(repo_root);
        run_command(&mut command, "describe jj change").await?
    };
    let output = ensure_success(output, "describe jj change")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn worktree_paths() {
        let root = Path::new("/repo");
        let id = PaneId::new(3);
        assert_eq!(
            worktree_path(root, id),
            PathBuf::from("/repo/.mush/worktrees/pane-3")
        );
        assert_eq!(worktree_branch(id), "mush-pane-3");
    }

    #[test]
    fn worktree_dir_structure() {
        let root = Path::new("/project");
        assert_eq!(
            worktree_dir(root),
            PathBuf::from("/project/.mush/worktrees")
        );
    }

    #[test]
    fn command_detail_falls_back_to_status() {
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: vec![],
            stderr: vec![],
        };
        assert!(command_detail(&output).contains("exit status"));
    }

    #[test]
    fn command_detail_prefers_stderr() {
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: b"stdout".to_vec(),
            stderr: b"stderr".to_vec(),
        };
        assert_eq!(command_detail(&output), "stderr");
    }
}
