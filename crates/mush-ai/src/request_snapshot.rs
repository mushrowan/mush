use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestFingerprint {
    pub system: u64,
    pub tools: u64,
    pub messages_prefix: u64,
    pub last_user: u64,
    pub messages_tail: u64,
}

impl RequestFingerprint {
    #[must_use]
    pub fn from_json_sections(system_json: &str, tools_json: &str, messages_json: &str) -> Self {
        let (prefix_json, last_user_json, tail_json) = split_last_user(messages_json);
        Self {
            system: hash_str(system_json),
            tools: hash_str(tools_json),
            messages_prefix: hash_str(&prefix_json),
            last_user: hash_str(&last_user_json),
            messages_tail: hash_str(&tail_json),
        }
    }
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn split_last_user(messages_json: &str) -> (String, String, String) {
    let Ok(serde_json::Value::Array(arr)) =
        serde_json::from_str::<serde_json::Value>(messages_json)
    else {
        return (messages_json.to_string(), String::new(), String::new());
    };

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
            let tail: Vec<_> = arr[idx + 1..].to_vec();
            (
                serde_json::to_string(&prefix).unwrap_or_default(),
                serde_json::to_string(last).unwrap_or_default(),
                serde_json::to_string(&tail).unwrap_or_default(),
            )
        }
        None => (messages_json.to_string(), String::new(), String::new()),
    }
}

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

pub const SNAPSHOT_RETENTION: usize = 5;

static SNAPSHOT_SEQ: AtomicU64 = AtomicU64::new(0);

static LAST_MESSAGES_PREFIX: AtomicU64 = AtomicU64::new(0);
static LAST_MESSAGES_PREFIX_SET: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static LAST_USED_CONTEXT_MANAGEMENT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static LAST_USED_CONTEXT_MANAGEMENT_SET: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[must_use]
pub fn last_messages_prefix_fingerprint() -> Option<u64> {
    if LAST_MESSAGES_PREFIX_SET.load(Ordering::Relaxed) {
        Some(LAST_MESSAGES_PREFIX.load(Ordering::Relaxed))
    } else {
        None
    }
}

#[must_use]
pub fn last_used_context_management() -> Option<bool> {
    if LAST_USED_CONTEXT_MANAGEMENT_SET.load(Ordering::Relaxed) {
        Some(LAST_USED_CONTEXT_MANAGEMENT.load(Ordering::Relaxed))
    } else {
        None
    }
}

const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
];

const SENSITIVE_JSON_FIELDS: &[&str] = &[
    "access_token",
    "api_key",
    "apikey",
    "authorization",
    "client_secret",
    "cookie",
    "id_token",
    "refresh_token",
    "secret",
    "set_cookie",
    "set-cookie",
    "token",
];

pub const REDACTED_HEADER_VALUE: &str = "<redacted>";

pub(crate) fn is_sensitive_header(name: &str) -> bool {
    SENSITIVE_HEADERS
        .iter()
        .any(|s| s.eq_ignore_ascii_case(name))
}

pub(crate) fn is_sensitive_json_field(name: &str) -> bool {
    let normalised = name.replace(['-', '_'], "").to_ascii_lowercase();
    SENSITIVE_JSON_FIELDS.iter().any(|field| {
        field
            .replace(['-', '_'], "")
            .eq_ignore_ascii_case(&normalised)
    })
}

fn redact_snapshot_body(body_json: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(body_json) {
        Ok(mut value) => {
            redact_json_value(&mut value, None);
            value
        }
        Err(_) => serde_json::Value::String(redact_secret_text(body_json)),
    }
}

fn body_uses_context_management(body_json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body_json)
        .ok()
        .and_then(|body| body.get("context_management").cloned())
        .is_some_and(|context_management| !context_management.is_null())
}

fn redact_json_value(value: &mut serde_json::Value, field_name: Option<&str>) {
    if field_name.is_some_and(is_sensitive_json_field) {
        *value = serde_json::Value::String(REDACTED_HEADER_VALUE.to_string());
        return;
    }

    match value {
        serde_json::Value::String(text) => {
            *text = redact_secret_text(text);
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value(value, None);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                redact_json_value(value, Some(key));
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn redact_secret_text(text: &str) -> String {
    text.split_whitespace()
        .map(redact_secret_word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_secret_word(word: &str) -> String {
    let trimmed = word.trim_matches(|ch: char| ch.is_ascii_punctuation());
    if looks_like_secret(trimmed) {
        word.replacen(trimmed, REDACTED_HEADER_VALUE, 1)
    } else {
        word.to_string()
    }
}

pub(crate) fn looks_like_secret(text: &str) -> bool {
    if text.len() < 12 {
        return false;
    }
    text.starts_with("sk-")
        || text.starts_with("sk_")
        || text.starts_with("sk-ant-")
        || text.starts_with("sess-")
        || text.starts_with("eyJ")
}

pub fn dump(
    provider: &str,
    fingerprint: &RequestFingerprint,
    body_json: &str,
    headers: &[(&str, &str)],
) -> Option<PathBuf> {
    dump_in(&snapshot_dir(), provider, fingerprint, body_json, headers)
}

pub fn dump_in(
    dir: &Path,
    provider: &str,
    fingerprint: &RequestFingerprint,
    body_json: &str,
    headers: &[(&str, &str)],
) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;

    let seq = SNAPSHOT_SEQ.fetch_add(1, Ordering::Relaxed);
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let filename = format!("{provider}-{ts_ms:013}-{seq:04}.json");
    let path = dir.join(&filename);

    let headers_json: serde_json::Map<String, serde_json::Value> = headers
        .iter()
        .map(|(name, value)| {
            let safe = if is_sensitive_header(name) {
                REDACTED_HEADER_VALUE.to_string()
            } else {
                (*value).to_string()
            };
            ((*name).to_string(), serde_json::Value::String(safe))
        })
        .collect();

    let uses_context_management = body_uses_context_management(body_json);

    let envelope = serde_json::json!({
        "_snapshot": {
            "provider": provider,
            "timestamp_ms": ts_ms,
            "fingerprint": fingerprint,
            "headers": headers_json,
        },
        "body": redact_snapshot_body(body_json)
    });

    match serde_json::to_string_pretty(&envelope) {
        Ok(json) => {
            if crate::private_io::write_private(&path, json).is_err() {
                return None;
            }
            LAST_MESSAGES_PREFIX.store(fingerprint.messages_prefix, Ordering::Relaxed);
            LAST_MESSAGES_PREFIX_SET.store(true, Ordering::Relaxed);
            LAST_USED_CONTEXT_MANAGEMENT.store(uses_context_management, Ordering::Relaxed);
            LAST_USED_CONTEXT_MANAGEMENT_SET.store(true, Ordering::Relaxed);
            prune_old_snapshots(dir, provider);
            Some(path)
        }
        Err(_) => None,
    }
}

#[must_use]
pub fn recent_snapshots(provider: &str, limit: usize) -> Vec<PathBuf> {
    recent_snapshots_in(&snapshot_dir(), provider, limit)
}

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
    fn dump_records_headers_and_redacts_authorization() {
        let dir = tempfile::tempdir().unwrap();
        let fp = RequestFingerprint::from_json_sections("[]", "[]", "[]");
        let headers = [
            ("anthropic-beta", "claude-code-20250219,oauth-2025-04-20"),
            ("anthropic-version", "2023-06-01"),
            ("authorization", "Bearer sk-secret-do-not-leak"),
            ("Authorization", "Bearer also-redact-mixed-case"),
            ("x-api-key", "shhhh"),
        ];
        let path = dump_in(dir.path(), "p", &fp, "{}", &headers).expect("dump succeeds");
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let recorded = parsed["_snapshot"]["headers"]
            .as_object()
            .expect("headers object present");
        assert_eq!(
            recorded["anthropic-beta"],
            "claude-code-20250219,oauth-2025-04-20"
        );
        assert_eq!(recorded["anthropic-version"], "2023-06-01");
        assert_eq!(recorded["authorization"], "<redacted>");
        let dump_text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !dump_text.contains("sk-secret"),
            "authorization value leaked into dump: {dump_text}"
        );
        assert!(
            !dump_text.contains("also-redact"),
            "mixed-case authorization leaked into dump: {dump_text}"
        );
        assert!(
            !dump_text.contains("shhhh"),
            "x-api-key leaked into dump: {dump_text}"
        );
    }

    #[test]
    fn dump_redacts_secret_fields_and_secret_shaped_strings_from_body() {
        let dir = tempfile::tempdir().unwrap();
        let fp = RequestFingerprint::from_json_sections("[]", "[]", "[]");
        let body = serde_json::json!({
            "api_key": "sk-body-field-secret-123",
            "access_token": "plain-access-token-value",
            "messages": [{
                "role": "user",
                "content": "please use Bearer sk-inline-secret-456 and token sk-ant-oat-inline-789"
            }],
            "safe": "keep me"
        });

        let path = dump_in(dir.path(), "p", &fp, &body.to_string(), &[]).expect("dump succeeds");
        let dump_text = std::fs::read_to_string(&path).unwrap();

        assert!(
            dump_text.contains("keep me"),
            "safe text should remain: {dump_text}"
        );
        assert!(
            !dump_text.contains("sk-body-field-secret-123"),
            "api key leaked: {dump_text}"
        );
        assert!(
            !dump_text.contains("plain-access-token-value"),
            "access token leaked: {dump_text}"
        );
        assert!(
            !dump_text.contains("sk-inline-secret-456"),
            "bearer leaked: {dump_text}"
        );
        assert!(
            !dump_text.contains("sk-ant-oat-inline-789"),
            "oauth token leaked: {dump_text}"
        );
        assert!(
            dump_text.contains(REDACTED_HEADER_VALUE),
            "redaction marker missing: {dump_text}"
        );
    }

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
    fn fingerprint_detects_drift_after_last_user_message() {
        let sys = r#"[]"#;
        let tools = r#"[]"#;
        let base = r#"{"role":"user","content":[{"type":"text","text":"u"}]}"#;
        let extra_a = r#"{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"read","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[]}]}"#;
        let extra_b = r#"{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"write","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":[]}]}"#;
        let msgs_a = format!("[{base},{extra_a}]");
        let msgs_b = format!("[{base},{extra_b}]");
        let fp_a = RequestFingerprint::from_json_sections(sys, tools, &msgs_a);
        let fp_b = RequestFingerprint::from_json_sections(sys, tools, &msgs_b);
        assert_eq!(fp_a.last_user, fp_b.last_user, "last_user is identical");
        assert_eq!(
            fp_a.messages_prefix, fp_b.messages_prefix,
            "messages_prefix is identical"
        );
        assert_ne!(
            fp_a.messages_tail, fp_b.messages_tail,
            "messages_tail must change when trailing turns differ"
        );
    }

    #[test]
    fn fingerprint_skips_tool_result_when_finding_last_user() {
        let sys = r#"[]"#;
        let tools = r#"[]"#;
        let msgs = r#"[{"role":"user","content":[{"type":"text","text":"u1"}]},{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"read","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[]}]}]"#;
        let fp = RequestFingerprint::from_json_sections(sys, tools, msgs);
        assert!(!fp.last_user.to_string().is_empty());
        assert!(
            fp.messages_prefix != fp.last_user,
            "prefix and last_user should differ for a non-trivial conversation"
        );
    }

    #[test]
    fn dump_publishes_messages_prefix_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let fp = RequestFingerprint::from_json_sections(
            "[]",
            "[]",
            r#"[{"role":"user","content":[{"type":"text","text":"hi"}]}]"#,
        );
        dump_in(dir.path(), "anthropic", &fp, "{}", &[]).expect("dump succeeds");
        let observed = last_messages_prefix_fingerprint();
        assert_eq!(
            observed,
            Some(fp.messages_prefix),
            "dump should publish the prefix fingerprint atomically"
        );
    }

    #[test]
    fn dump_publishes_context_management_presence() {
        let dir = tempfile::tempdir().unwrap();
        let fp = RequestFingerprint::from_json_sections("[]", "[]", "[]");

        dump_in(
            dir.path(),
            "anthropic",
            &fp,
            r#"{"model":"test","context_management":{"edits":[]}}"#,
            &[],
        )
        .expect("dump succeeds");
        assert_eq!(last_used_context_management(), Some(true));

        dump_in(dir.path(), "anthropic", &fp, r#"{"model":"test"}"#, &[]).expect("dump succeeds");
        assert_eq!(last_used_context_management(), Some(false));
    }

    #[test]
    fn dump_writes_file_with_fingerprint_header() {
        let dir = tempfile::tempdir().unwrap();
        let fp = RequestFingerprint::from_json_sections("[]", "[]", "[]");
        let body = r#"{"model":"test","messages":[]}"#;
        let path = dump_in(dir.path(), "testprovider", &fp, body, &[]).expect("dump succeeds");
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
            dump_in(dir.path(), "p", &fp, &format!(r#"{{"call":{i}}}"#), &[]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let paths = recent_snapshots_in(dir.path(), "p", 2);
        assert_eq!(paths.len(), 2);
        let first = std::fs::read_to_string(&paths[0]).unwrap();
        assert!(first.contains(r#""call": 2"#));
    }

    #[test]
    fn dump_prunes_to_retention_count() {
        let dir = tempfile::tempdir().unwrap();
        let fp = RequestFingerprint::from_json_sections("[]", "[]", "[]");
        for i in 0..(SNAPSHOT_RETENTION + 3) {
            dump_in(dir.path(), "pr", &fp, &format!(r#"{{"call":{i}}}"#), &[]).unwrap();
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
