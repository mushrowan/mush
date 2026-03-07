//! VCS-based file isolation for multi-pane agents
//!
//! provides worktree (git) and jj change isolation backends so forked
//! panes can work on separate filesystem snapshots without conflicts

use std::path::{Path, PathBuf};

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

// -- worktree operations --

/// directory where worktrees are stored
fn worktree_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".mush").join("worktrees")
}

fn worktree_path(repo_root: &Path, pane_id: PaneId) -> PathBuf {
    worktree_dir(repo_root).join(format!("pane-{}", pane_id.as_u32()))
}

fn worktree_branch(pane_id: PaneId) -> String {
    format!("mush-pane-{}", pane_id.as_u32())
}

/// create a git worktree for a pane. uses the colocated .git dir
/// if we're in a jj repo, otherwise uses the repo's .git directly
pub async fn create_worktree(repo_root: &Path, pane_id: PaneId) -> Result<WorktreeInfo, String> {
    let wt_path = worktree_path(repo_root, pane_id);
    let branch = worktree_branch(pane_id);

    // ensure parent dir exists
    tokio::fs::create_dir_all(worktree_dir(repo_root))
        .await
        .map_err(|e| format!("failed to create worktree dir: {e}"))?;

    // clean up if leftover from a previous session
    if wt_path.exists() {
        remove_worktree(repo_root, pane_id).await.ok();
    }

    let output = tokio::process::Command::new("git")
        .args(["worktree", "add", "-b", &branch])
        .arg(&wt_path)
        .arg("HEAD")
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| format!("failed to run git worktree add: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // branch might already exist from a previous session, try without -b
        if stderr.contains("already exists") {
            // delete the stale branch first
            let _ = tokio::process::Command::new("git")
                .args(["branch", "-D", &branch])
                .current_dir(repo_root)
                .output()
                .await;
            // retry
            let retry = tokio::process::Command::new("git")
                .args(["worktree", "add", "-b", &branch])
                .arg(&wt_path)
                .arg("HEAD")
                .current_dir(repo_root)
                .output()
                .await
                .map_err(|e| format!("failed to run git worktree add (retry): {e}"))?;
            if !retry.status.success() {
                return Err(format!(
                    "git worktree add failed: {}",
                    String::from_utf8_lossy(&retry.stderr)
                ));
            }
        } else {
            return Err(format!("git worktree add failed: {stderr}"));
        }
    }

    Ok(WorktreeInfo {
        path: wt_path,
        branch,
    })
}

/// remove a git worktree and its branch
pub async fn remove_worktree(repo_root: &Path, pane_id: PaneId) -> Result<(), String> {
    let wt_path = worktree_path(repo_root, pane_id);
    let branch = worktree_branch(pane_id);

    // remove the worktree
    let output = tokio::process::Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&wt_path)
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| format!("failed to run git worktree remove: {e}"))?;

    if !output.status.success() {
        // if the path doesn't exist, try pruning
        let _ = tokio::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(repo_root)
            .output()
            .await;
        // also try removing the directory manually
        let _ = tokio::fs::remove_dir_all(&wt_path).await;
    }

    // delete the branch
    let _ = tokio::process::Command::new("git")
        .args(["branch", "-D", &branch])
        .current_dir(repo_root)
        .output()
        .await;

    Ok(())
}

/// merge a worktree's branch back into the current branch
pub async fn merge_worktree(repo_root: &Path, pane_id: PaneId) -> Result<String, String> {
    let branch = worktree_branch(pane_id);

    let output = tokio::process::Command::new("git")
        .args(["merge", "--no-edit", &branch])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| format!("failed to run git merge: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        return Err(format!("merge failed (may have conflicts): {stderr}"));
    }

    Ok(format!("merged {branch}: {stdout}"))
}

/// clean up stale worktrees from previous sessions
pub async fn cleanup_stale_worktrees(repo_root: &Path) -> usize {
    let wt_dir = worktree_dir(repo_root);
    if !wt_dir.exists() {
        return 0;
    }

    // prune any worktrees whose paths no longer exist
    let _ = tokio::process::Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(repo_root)
        .output()
        .await;

    // remove any leftover pane directories
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

    // remove .mush/worktrees if empty
    if let Ok(mut entries) = tokio::fs::read_dir(&wt_dir).await
        && entries.next_entry().await.ok().flatten().is_none()
    {
        let _ = tokio::fs::remove_dir(&wt_dir).await;
        let _ = tokio::fs::remove_dir(repo_root.join(".mush")).await;
    }

    cleaned
}

// -- jj operations --

/// create a new jj change for a forked pane
pub async fn create_jj_change(repo_root: &Path) -> Result<JjChangeInfo, String> {
    // create a new change on top of the current working copy
    let output = tokio::process::Command::new("jj")
        .args(["new", "@", "-m", "mush agent working change"])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| format!("failed to run jj new: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "jj new failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // get the change id of the new working copy
    let id_output = tokio::process::Command::new("jj")
        .args(["log", "-r", "@", "--no-graph", "-T", "change_id"])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| format!("failed to get change id: {e}"))?;

    let change_id = String::from_utf8_lossy(&id_output.stdout).trim().to_string();
    if change_id.is_empty() {
        return Err("got empty change id from jj".into());
    }

    Ok(JjChangeInfo { change_id })
}

/// switch to a jj change (for pane-aware operations)
pub async fn edit_jj_change(repo_root: &Path, change_id: &str) -> Result<(), String> {
    let output = tokio::process::Command::new("jj")
        .args(["edit", change_id])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| format!("failed to run jj edit: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "jj edit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// squash a jj change into its parent
pub async fn squash_jj_change(repo_root: &Path, change_id: &str) -> Result<String, String> {
    let output = tokio::process::Command::new("jj")
        .args(["squash", "-r", change_id])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| format!("failed to run jj squash: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() {
        return Err(format!(
            "jj squash failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(stdout.trim().to_string())
}

/// abandon a jj change (discard all its modifications)
pub async fn abandon_jj_change(repo_root: &Path, change_id: &str) -> Result<(), String> {
    let output = tokio::process::Command::new("jj")
        .args(["abandon", change_id])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| format!("failed to run jj abandon: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "jj abandon failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// get a short description of what changed in a jj change
pub async fn describe_jj_change(repo_root: &Path, change_id: &str) -> Result<String, String> {
    let output = tokio::process::Command::new("jj")
        .args([
            "log",
            "-r",
            change_id,
            "--no-graph",
            "-T",
            r#"separate("\n", change_id.shortest(8), description, if(empty, "(empty)"))"#,
        ])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| format!("failed to describe jj change: {e}"))?;

    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
