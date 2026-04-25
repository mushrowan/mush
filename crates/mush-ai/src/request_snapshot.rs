//! diagnostic snapshots of outgoing LLM requests for cache-bust
//! triage. providers write a copy of each request body (plus section
//! hashes) to a rotating dir so a later anomaly detector can diff the
//! two most recent payloads and see exactly which bytes drifted.
//!
//! intentionally minimal: one flat dir, a small ring of files, and a
//! tiny metadata header embedded at the top of every dump. no
//! subscriber wiring, no async channel plumbing. the dir is gitignored
//! via the user's XDG state path, which is already agent-owned.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// per-section hashes. lets a bust diagnostic compare two consecutive
/// requests and tell which section changed (system prompt, tools,
/// message prefix, or just the last user message). the hash is a
/// best-effort fingerprint: matching hashes imply identical bytes at
/// the hash moment, mismatches pinpoint the section that drifted
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestFingerprint {
    pub system: u64,
    pub tools: u64,
    /// hash of everything in the message array except the tail user
    /// message (that one always changes between calls by design, so
    /// excluding it surfaces upstream drift)
    pub messages_prefix: u64,
    pub last_user: u64,
}

impl RequestFingerprint {
    /// compute fingerprints from already-serialised json sections.
    /// callers supply pre-serialised bytes so providers don't double-
    /// encode and so we hash *exactly* the bytes the wire will see.
    /// `messages_json` is expected to be a top-level json array
    #[must_use]
    pub fn from_json_sections(system_json: &str, tools_json: &str, messages_json: &str) -> Self {
        let (prefix_json, last_user_json) = split_last_user(messages_json);
        Self {
            system: hash_str(system_json),
            tools: hash_str(tools_json),
            messages_prefix: hash_str(&prefix_json),
            last_user: hash_str(&last_user_json),
        }
    }
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// return (prefix_json, last_user_json) where the last user message
/// (identified by `role: "user"` and not containing a `tool_result`
/// block) is split out. falls back to (all, "") if the input doesn't
/// parse or has no user message
fn split_last_user(messages_json: &str) -> (String, String) {
    let Ok(serde_json::Value::Array(arr)) =
        serde_json::from_str::<serde_json::Value>(messages_json)
    else {
        return (messages_json.to_string(), String::new());
    };

    // find the last message with role=user that is NOT a tool_result
    // (tool_results are role=user in anthropic's wire format but we
    // want the actual user text message the breakpoint lands on)
    let last_user_idx = arr.iter().enumerate().rev().find_map(|(i, msg)| {
        let role = msg.get("role").and_then(|r| r.as_str())?;
        if role != "user" {
            return None;
        }
        let is_tool_result = msg
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
            })
            .unwrap_or(false);
        if is_tool_result { None } else { Some(i) }
    });

    match last_user_idx {
        Some(idx) => {
            let prefix: Vec<_> = arr[..idx].to_vec();
            let last = &arr[idx];
            (
                serde_json::to_string(&prefix).unwrap_or_default(),
                serde_json::to_string(last).unwrap_or_default(),
            )
        }
        None => (messages_json.to_string(), String::new()),
    }
}

/// directory where request snapshots are written. respects
/// `XDG_STATE_HOME` then `$HOME/.local/state`, falling back to
/// `.mush/request-snapshots` in the cwd for unusual environments
#[must_use]
pub fn snapshot_dir() -> PathBuf {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(state).join("mush/request-snapshots")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/state/mush/request-snapshots")
    } else {
        PathBuf::from(".mush/request-snapshots")
    }
}

/// how many snapshot files to keep on disk per provider
pub const SNAPSHOT_RETENTION: usize = 5;

/// monotonically increasing sequence for filename disambiguation within
/// the same wall-clock second. resets on process restart, which is
/// fine since we sort dumps by mtime anyway
static SNAPSHOT_SEQ: AtomicU64 = AtomicU64::new(0);

/// most recent `messages_prefix` fingerprint published by a provider's
/// `dump` call. read by the cache-bust classifier so it can spot
/// history mutations across consecutive turns. `0` means "never set",
/// which (a) can't be confused with a real hash because hashing happens
/// inside `RequestFingerprint::from_json_sections` and (b) is only used
/// as a sentinel through [`last_messages_prefix_fingerprint`].
///
/// process-global rather than per-pane: today there's at most one
/// in-flight LLM call per pane and panes don't stream simultaneously
/// often enough to matter for classification accuracy. multi-pane
/// concurrency can reshape this into per-stream state if it ever
/// causes false negatives
static LAST_MESSAGES_PREFIX: AtomicU64 = AtomicU64::new(0);
static LAST_MESSAGES_PREFIX_SET: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// read the most recent `messages_prefix` fingerprint published by
/// [`dump`]. returns `None` before the first dump of the process
#[must_use]
pub fn last_messages_prefix_fingerprint() -> Option<u64> {
    if LAST_MESSAGES_PREFIX_SET.load(Ordering::Relaxed) {
        Some(LAST_MESSAGES_PREFIX.load(Ordering::Relaxed))
    } else {
        None
    }
}

/// write a request snapshot: a JSON file containing the fingerprint
/// as a `_snapshot` header followed by the raw body. returns the path
/// on success. rotates the dir to keep at most [`SNAPSHOT_RETENTION`]
/// files per provider. uses the default [`snapshot_dir`]
pub fn dump(provider: &str, fingerprint: &RequestFingerprint, body_json: &str) -> Option<PathBuf> {
    dump_in(&snapshot_dir(), provider, fingerprint, body_json)
}

/// same as [`dump`] but writes to an explicit dir. used by tests to
/// avoid racing the process-global `XDG_STATE_HOME` environment var
pub fn dump_in(
    dir: &Path,
    provider: &str,
    fingerprint: &RequestFingerprint,
    body_json: &str,
) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;

    let seq = SNAPSHOT_SEQ.fetch_add(1, Ordering::Relaxed);
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let filename = format!("{provider}-{ts_ms:013}-{seq:04}.json");
    let path = dir.join(&filename);

    // embed fingerprint as a sibling top-level field by wrapping the
    // body in a small envelope. keeps the file valid json while making
    // the fingerprint trivially greppable
    let envelope = serde_json::json!({
        "_snapshot": {
            "provider": provider,
            "timestamp_ms": ts_ms,
            "fingerprint": fingerprint,
        },
        "body": match serde_json::from_str::<serde_json::Value>(body_json) {
            Ok(v) => v,
            Err(_) => serde_json::Value::String(body_json.to_string()),
        }
    });

    match serde_json::to_string_pretty(&envelope) {
        Ok(json) => {
            if std::fs::write(&path, json).is_err() {
                return None;
            }
            // publish the prefix fingerprint so the cache-bust classifier
            // can compare against the previous turn's value. done after
            // the file write so observers only see the value once it's
            // safely durable
            LAST_MESSAGES_PREFIX.store(fingerprint.messages_prefix, Ordering::Relaxed);
            LAST_MESSAGES_PREFIX_SET.store(true, Ordering::Relaxed);
            prune_old_snapshots(dir, provider);
            Some(path)
        }
        Err(_) => None,
    }
}

/// list the most recent snapshots for a provider, newest first,
/// capped at `limit`. used by cache-bust diagnostics to reference
/// the bodies that were in flight when the bust happened. uses the
/// default [`snapshot_dir`]
#[must_use]
pub fn recent_snapshots(provider: &str, limit: usize) -> Vec<PathBuf> {
    recent_snapshots_in(&snapshot_dir(), provider, limit)
}

/// same as [`recent_snapshots`] but reads from an explicit dir
#[must_use]
pub fn recent_snapshots_in(dir: &Path, provider: &str, limit: usize) -> Vec<PathBuf> {
    let prefix = format!("{provider}-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut matches: Vec<(PathBuf, std::time::SystemTime)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_string();
            if !name.starts_with(&prefix) || !name.ends_with(".json") {
                return None;
            }
            let mtime = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((path, mtime))
        })
        .collect();
    matches.sort_by_key(|b| std::cmp::Reverse(b.1));
    matches.into_iter().take(limit).map(|(p, _)| p).collect()
}

/// delete older snapshots beyond the retention count
fn prune_old_snapshots(dir: &Path, provider: &str) {
    let prefix = format!("{provider}-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_string();
            if !name.starts_with(&prefix) || !name.ends_with(".json") {
                return None;
            }
            let mtime = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((path, mtime))
        })
        .collect();
    if files.len() <= SNAPSHOT_RETENTION {
        return;
    }
    files.sort_by_key(|b| std::cmp::Reverse(b.1));
    for (old, _) in files.into_iter().skip(SNAPSHOT_RETENTION) {
        let _ = std::fs::remove_file(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_for_identical_input() {
        let sys = r#"[{"type":"text","text":"hi"}]"#;
        let tools = r#"[{"name":"read"}]"#;
        let msgs = r#"[{"role":"user","content":[{"type":"text","text":"go"}]}]"#;
        let a = RequestFingerprint::from_json_sections(sys, tools, msgs);
        let b = RequestFingerprint::from_json_sections(sys, tools, msgs);
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_changes_when_tools_reordered() {
        // json key/value reordering should be reflected in the hash so
        // callers can spot serialisation-order drift between calls
        let sys = r#"[]"#;
        let msgs = r#"[]"#;
        let a = RequestFingerprint::from_json_sections(
            sys,
            r#"[{"name":"read"},{"name":"write"}]"#,
            msgs,
        );
        let b = RequestFingerprint::from_json_sections(
            sys,
            r#"[{"name":"write"},{"name":"read"}]"#,
            msgs,
        );
        assert_ne!(a.tools, b.tools, "reordered tools should hash differently");
        assert_eq!(a.system, b.system);
        assert_eq!(a.messages_prefix, b.messages_prefix);
    }

    #[test]
    fn fingerprint_isolates_last_user_from_prefix() {
        // adding a user message on the end should change last_user but
        // keep messages_prefix stable. that's the whole point of the
        // split: cache busts that change the prefix are real anomalies,
        // whereas a new last-user message is expected every turn
        let sys = r#"[]"#;
        let tools = r#"[]"#;
        let before = r#"[{"role":"user","content":[{"type":"text","text":"a"}]}]"#;
        let after = r#"[{"role":"user","content":[{"type":"text","text":"a"}]},{"role":"assistant","content":[{"type":"text","text":"b"}]},{"role":"user","content":[{"type":"text","text":"c"}]}]"#;
        let a = RequestFingerprint::from_json_sections(sys, tools, before);
        let b = RequestFingerprint::from_json_sections(sys, tools, after);
        assert_ne!(a.last_user, b.last_user);
        assert_ne!(
            a.messages_prefix, b.messages_prefix,
            "prefix grew (a turned from last-user into prefix)"
        );
    }

    #[test]
    fn fingerprint_skips_tool_result_when_finding_last_user() {
        // tool_result messages are role=user in anthropic's wire
        // format. they must not be treated as "the last user" or the
        // fingerprint split moves with every tool call and prefix
        // drift is hidden inside last_user
        let sys = r#"[]"#;
        let tools = r#"[]"#;
        let msgs = r#"[{"role":"user","content":[{"type":"text","text":"u1"}]},{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"read","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[]}]}]"#;
        let fp = RequestFingerprint::from_json_sections(sys, tools, msgs);
        // last_user should be u1 (the real user text), not the tool_result
        assert!(!fp.last_user.to_string().is_empty());
        assert!(
            fp.messages_prefix != fp.last_user,
            "prefix and last_user should differ for a non-trivial conversation"
        );
    }

    #[test]
    fn dump_publishes_messages_prefix_fingerprint() {
        // dump must publish the prefix fingerprint so the cache-bust
        // classifier can compare prev vs curr without re-reading files
        let dir = tempfile::tempdir().unwrap();
        let fp = RequestFingerprint::from_json_sections(
            "[]",
            "[]",
            r#"[{"role":"user","content":[{"type":"text","text":"hi"}]}]"#,
        );
        dump_in(dir.path(), "anthropic", &fp, "{}").expect("dump succeeds");
        let observed = last_messages_prefix_fingerprint();
        assert_eq!(
            observed,
            Some(fp.messages_prefix),
            "dump should publish the prefix fingerprint atomically"
        );
    }

    #[test]
    fn dump_writes_file_with_fingerprint_header() {
        let dir = tempfile::tempdir().unwrap();
        let fp = RequestFingerprint::from_json_sections("[]", "[]", "[]");
        let body = r#"{"model":"test","messages":[]}"#;
        let path = dump_in(dir.path(), "testprovider", &fp, body).expect("dump succeeds");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("_snapshot"));
        assert!(contents.contains("fingerprint"));
        assert!(contents.contains("testprovider"));
        assert!(contents.contains(r#""body""#));
    }

    #[test]
    fn recent_snapshots_returns_newest_first_limited() {
        let dir = tempfile::tempdir().unwrap();
        let fp = RequestFingerprint::from_json_sections("[]", "[]", "[]");
        for i in 0..3 {
            dump_in(dir.path(), "p", &fp, &format!(r#"{{"call":{i}}}"#)).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let paths = recent_snapshots_in(dir.path(), "p", 2);
        assert_eq!(paths.len(), 2);
        // newest first: the file with the highest mtime should come first
        let first = std::fs::read_to_string(&paths[0]).unwrap();
        assert!(first.contains(r#""call": 2"#));
    }

    #[test]
    fn dump_prunes_to_retention_count() {
        let dir = tempfile::tempdir().unwrap();
        let fp = RequestFingerprint::from_json_sections("[]", "[]", "[]");
        for i in 0..(SNAPSHOT_RETENTION + 3) {
            dump_in(dir.path(), "pr", &fp, &format!(r#"{{"call":{i}}}"#)).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let all = recent_snapshots_in(dir.path(), "pr", usize::MAX);
        assert_eq!(
            all.len(),
            SNAPSHOT_RETENTION,
            "rotation should leave exactly {SNAPSHOT_RETENTION} files"
        );
    }
}
