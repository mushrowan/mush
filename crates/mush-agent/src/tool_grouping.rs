//! tool-call grouping for safe parallel execution
//!
//! when an agent emits multiple tool calls in a single turn, calls that
//! mutate the same file must be serialised to avoid silent data loss
//! (read-modify-write races where the last writer wins). this module
//! provides the path-keying and grouping primitives used by both the
//! `batch` tool and the agent loop's native parallel-call path.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// tool names that mutate a single file via a `path` argument
const FILE_MUTATING: &[&str] = &["edit", "write"];

/// return a normalised path key if the tool mutates a single file by
/// path. returns `None` for tools that don't fit the (path-arg, single
/// file) pattern (read-only tools, multi-file `apply_patch`, etc).
///
/// the returned string is a lexically-cleaned form of the input path so
/// that `./foo.rs` and `foo.rs` map to the same key. it does not touch
/// the filesystem (no symlink resolution) and does not collapse `..`.
pub fn file_path_key(tool_name: &str, args: &Value) -> Option<String> {
    if !FILE_MUTATING
        .iter()
        .any(|t| tool_name.eq_ignore_ascii_case(t))
    {
        return None;
    }
    let path_str = args.get("path").and_then(Value::as_str)?;
    Some(normalise_path_key(path_str))
}

fn normalise_path_key(path: &str) -> String {
    let cleaned: PathBuf = Path::new(path)
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    cleaned.to_string_lossy().into_owned()
}

/// partition a list of items into groups where each group must be
/// executed sequentially. items with the same key share a group; items
/// with `None` keys get their own singleton group. groups are returned
/// in the order their first item appears so caller-side ordering is
/// preserved.
pub fn group_by_path<T, F>(items: Vec<T>, key_fn: F) -> Vec<Vec<(usize, T)>>
where
    F: Fn(&T) -> Option<String>,
{
    let mut groups: Vec<Vec<(usize, T)>> = Vec::new();
    let mut path_to_group: HashMap<String, usize> = HashMap::new();
    for (i, item) in items.into_iter().enumerate() {
        match key_fn(&item) {
            Some(key) => {
                if let Some(&idx) = path_to_group.get(&key) {
                    groups[idx].push((i, item));
                } else {
                    let idx = groups.len();
                    path_to_group.insert(key, idx);
                    groups.push(vec![(i, item)]);
                }
            }
            None => groups.push(vec![(i, item)]),
        }
    }
    groups
}

/// run a list of items through `exec_fn`, executing items that share a
/// path key sequentially within their group while running different
/// groups concurrently. results come back in submission order so callers
/// can zip them with the original input.
///
/// the closure receives the item by value because the inner futures need
/// to own their input so the outer `join_all` can drive them concurrently.
pub async fn execute_grouped<T, K, R, Fut, F>(items: Vec<T>, key_fn: K, exec_fn: F) -> Vec<R>
where
    K: Fn(&T) -> Option<String>,
    F: Fn(T) -> Fut + Clone,
    Fut: std::future::Future<Output = R>,
{
    let groups = group_by_path(items, key_fn);
    let total: usize = groups.iter().map(Vec::len).sum();

    let group_futs = groups.into_iter().map(|group| {
        let exec_fn = exec_fn.clone();
        async move {
            let mut out = Vec::with_capacity(group.len());
            for (i, item) in group {
                out.push((i, exec_fn(item).await));
            }
            out
        }
    });
    let mut indexed: Vec<(usize, R)> = futures::future::join_all(group_futs)
        .await
        .into_iter()
        .flatten()
        .collect();
    indexed.sort_by_key(|(i, _)| *i);
    debug_assert_eq!(indexed.len(), total);
    indexed.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn file_path_key_for_edit_returns_path() {
        let args = json!({"path": "src/main.rs", "oldText": "x", "newText": "y"});
        assert_eq!(file_path_key("edit", &args).as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn file_path_key_for_write_returns_path() {
        let args = json!({"path": "out.txt", "content": "hello"});
        assert_eq!(file_path_key("write", &args).as_deref(), Some("out.txt"));
    }

    #[test]
    fn file_path_key_case_insensitive_tool_name() {
        let args = json!({"path": "f.txt"});
        assert_eq!(file_path_key("Edit", &args).as_deref(), Some("f.txt"));
        assert_eq!(file_path_key("WRITE", &args).as_deref(), Some("f.txt"));
    }

    #[test]
    fn file_path_key_for_read_returns_none() {
        let args = json!({"path": "f.txt"});
        assert!(file_path_key("read", &args).is_none());
    }

    #[test]
    fn file_path_key_for_apply_patch_returns_none() {
        // apply_patch can touch many files - cannot be grouped by single path
        let args = json!({"patch_text": "..."});
        assert!(file_path_key("apply_patch", &args).is_none());
    }

    #[test]
    fn file_path_key_missing_path_returns_none() {
        assert!(file_path_key("edit", &json!({})).is_none());
    }

    #[test]
    fn file_path_key_normalises_dot_slash() {
        let args1 = json!({"path": "./foo.rs"});
        let args2 = json!({"path": "foo.rs"});
        assert_eq!(
            file_path_key("edit", &args1),
            file_path_key("edit", &args2),
            "./foo.rs and foo.rs must share the same key",
        );
    }

    #[test]
    fn file_path_key_normalises_inner_dot_segments() {
        let args1 = json!({"path": "a/./b/c"});
        let args2 = json!({"path": "a/b/c"});
        assert_eq!(file_path_key("edit", &args1), file_path_key("edit", &args2));
    }

    #[test]
    fn file_path_key_keeps_distinct_paths_distinct() {
        assert_ne!(
            file_path_key("edit", &json!({"path": "a.rs"})),
            file_path_key("edit", &json!({"path": "b.rs"})),
        );
    }

    #[test]
    fn group_by_path_groups_same_file_together() {
        let items = vec![
            ("edit", json!({"path": "foo.rs"})),
            ("edit", json!({"path": "bar.rs"})),
            ("edit", json!({"path": "foo.rs"})),
        ];
        let groups = group_by_path(items, |(name, args)| file_path_key(name, args));
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2, "foo.rs edits share a group");
        assert_eq!(groups[0][0].0, 0);
        assert_eq!(groups[0][1].0, 2);
        assert_eq!(groups[1].len(), 1, "bar.rs is alone");
        assert_eq!(groups[1][0].0, 1);
    }

    #[test]
    fn group_by_path_normalised_paths_share_group() {
        let items = vec![
            ("edit", json!({"path": "./foo.rs"})),
            ("edit", json!({"path": "foo.rs"})),
        ];
        let groups = group_by_path(items, |(name, args)| file_path_key(name, args));
        assert_eq!(
            groups.len(),
            1,
            "syntactic variants of same path group together"
        );
        assert_eq!(groups[0].len(), 2);
    }

    #[test]
    fn group_by_path_read_only_tools_run_in_parallel() {
        let items = vec![
            ("read", json!({"path": "foo.rs"})),
            ("read", json!({"path": "foo.rs"})),
            ("read", json!({"path": "foo.rs"})),
        ];
        let groups = group_by_path(items, |(name, args)| file_path_key(name, args));
        assert_eq!(groups.len(), 3, "read calls all get their own group");
    }

    #[test]
    fn group_by_path_preserves_first_appearance_order() {
        let items = vec![
            ("edit", json!({"path": "z.rs"})),
            ("edit", json!({"path": "a.rs"})),
            ("edit", json!({"path": "z.rs"})),
        ];
        let groups = group_by_path(items, |(name, args)| file_path_key(name, args));
        assert_eq!(groups.len(), 2);
        // z.rs group came first because it appears first
        assert_eq!(groups[0][0].0, 0);
        assert_eq!(groups[1][0].0, 1);
    }

    #[test]
    fn group_by_path_empty_input() {
        let items: Vec<(&str, Value)> = vec![];
        let groups = group_by_path(items, |(name, args)| file_path_key(name, args));
        assert!(groups.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn execute_grouped_serialises_same_path_calls() {
        // simulate the read-modify-write race that motivates this fix.
        // each call reads its file's counter, sleeps to widen the race
        // window, then writes counter+1 back. without sequencing the
        // two same-file calls would both read 0 and write 1, losing
        // one increment. with sequencing both increments land.
        use std::collections::HashMap;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let counters: Arc<Mutex<HashMap<String, u32>>> = Arc::new(Mutex::new(HashMap::new()));

        let calls = vec![
            ("edit", json!({"path": "foo.rs", "tag": "a"})),
            ("edit", json!({"path": "foo.rs", "tag": "b"})),
            ("edit", json!({"path": "bar.rs", "tag": "c"})),
        ];

        let counters_for_exec = counters.clone();
        let results = execute_grouped(
            calls,
            |(name, args)| file_path_key(name, args),
            move |(_, args)| {
                let counters = counters_for_exec.clone();
                async move {
                    let path = args["path"].as_str().unwrap().to_string();
                    let v = counters.lock().await.get(&path).copied().unwrap_or(0);
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    counters.lock().await.insert(path, v + 1);
                    args["tag"].as_str().unwrap().to_string()
                }
            },
        )
        .await;

        // results come back in submission order
        assert_eq!(results, vec!["a", "b", "c"]);

        let final_counters = counters.lock().await.clone();
        assert_eq!(
            final_counters.get("foo.rs"),
            Some(&2),
            "both foo.rs edits must land sequentially (counter races would drop one)"
        );
        assert_eq!(final_counters.get("bar.rs"), Some(&1), "bar.rs ran once");
    }

    #[tokio::test]
    async fn execute_grouped_runs_independent_calls_in_parallel() {
        // sleep + record start time per call. independent calls (no
        // shared path key) should overlap, so total wall time is closer
        // to one delay than to N delays
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let items: Vec<(&str, Value)> = (0..4)
            .map(|i| ("read", json!({"path": format!("f{i}.rs")})))
            .collect();
        let _ = execute_grouped(
            items,
            |(name, args)| file_path_key(name, args),
            |_| async {
                tokio::time::sleep(Duration::from_millis(50)).await;
            },
        )
        .await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(150),
            "4 parallel 50ms calls should finish well under 200ms (took {elapsed:?})"
        );
    }
}
