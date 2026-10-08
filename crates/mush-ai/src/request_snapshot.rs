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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestDigest {
    pub fingerprint: RequestFingerprint,
    pub context_management: bool,
    pub cache_ttl_secs: u16,
    pub snapshot: Option<PathBuf>,
    pub previous_snapshot: Option<PathBuf>,
}

static LAST_BY_SESSION: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, RequestDigest>>,
> = std::sync::LazyLock::new(Default::default);

#[must_use]
pub fn last_request(session: &str) -> Option<RequestDigest> {
    LAST_BY_SESSION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(session)
        .cloned()
}

pub fn record_request(
    session: &str,
    fingerprint: RequestFingerprint,
    context_management: bool,
    cache_ttl_secs: u16,
    snapshot: Option<PathBuf>,
) {
    let mut last = LAST_BY_SESSION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous_snapshot = last.get(session).and_then(|digest| digest.snapshot.clone());
    last.insert(
        session.to_string(),
        RequestDigest {
            fingerprint,
            context_management,
            cache_ttl_secs,
            snapshot,
            previous_snapshot,
        },
    );
}

fn system_without_billing_header(system_json: &str) -> String {
    let Ok(serde_json::Value::Array(blocks)) =
        serde_json::from_str::<serde_json::Value>(system_json)
    else {
        return system_json.to_string();
    };
    let kept: Vec<&serde_json::Value> = blocks
        .iter()
        .filter(|block| {
            !block
                .get("text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| text.starts_with("x-anthropic-billing-header"))
        })
        .collect();
    serde_json::to_string(&kept).unwrap_or_else(|_| system_json.to_string())
}

impl RequestFingerprint {
    #[must_use]
    pub fn from_json_sections(system_json: &str, tools_json: &str, messages_json: &str) -> Self {
        let (prefix_json, last_user_json, tail_json) = split_last_user(messages_json);
        Self {
            system: hash_str(&system_without_billing_header(system_json)),
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
        Some(idx) => (
            serde_json::to_string(&arr[..idx]).unwrap_or_default(),
            serde_json::to_string(&arr[idx]).unwrap_or_default(),
            serde_json::to_string(&arr[idx + 1..]).unwrap_or_default(),
        ),
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
    fn folded(text: &str) -> impl Iterator<Item = u8> + '_ {
        text.bytes()
            .filter(|byte| !matches!(byte, b'-' | b'_'))
            .map(|byte| byte.to_ascii_lowercase())
    }
    SENSITIVE_JSON_FIELDS
        .iter()
        .any(|field| folded(field).eq(folded(name)))
}

pub(crate) fn redacted_headers(headers: &[(&str, &str)]) -> serde_json::Value {
    headers
        .iter()
        .map(|&(name, value)| {
            let safe = if is_sensitive_header(name) {
                REDACTED_HEADER_VALUE
            } else {
                value
            };
            (name.to_string(), serde_json::Value::from(safe))
        })
        .collect::<serde_json::Map<_, _>>()
        .into()
}

pub(crate) fn redact_body(body_json: &str, redact_text: fn(&str) -> String) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(body_json) {
        Ok(mut value) => {
            redact_json_value(&mut value, None, redact_text);
            value
        }
        Err(_) => serde_json::Value::String(redact_text(body_json)),
    }
}

pub(crate) fn redact_json_value(
    value: &mut serde_json::Value,
    field_name: Option<&str>,
    redact_text: fn(&str) -> String,
) {
    if field_name.is_some_and(is_sensitive_json_field) {
        *value = serde_json::Value::String(REDACTED_HEADER_VALUE.to_string());
        return;
    }

    match value {
        serde_json::Value::String(text) => {
            *text = redact_text(text);
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value(value, None, redact_text);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                redact_json_value(value, Some(key), redact_text);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

pub(crate) fn redact_secret_text(text: &str) -> String {
    let mut redacted = String::with_capacity(text.len());
    for (index, word) in text.split_whitespace().enumerate() {
        if index > 0 {
            redacted.push(' ');
        }
        redacted.push_str(&redact_secret_word(word));
    }
    redacted
}

fn redact_secret_word(word: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = word.trim_matches(|ch: char| ch.is_ascii_punctuation());
    if looks_like_secret(trimmed) {
        word.replacen(trimmed, REDACTED_HEADER_VALUE, 1).into()
    } else {
        word.into()
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
    session: Option<&str>,
    fingerprint: &RequestFingerprint,
    body_json: &str,
    headers: &[(&str, &str)],
) -> Option<PathBuf> {
    let dir = snapshot_dir();
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return dump_in(&dir, provider, session, fingerprint, body_json, headers);
    };
    let snapshot = Snapshot::new(&dir, provider, session);
    let path = snapshot.path.clone();
    let provider = provider.to_string();
    let fingerprint = *fingerprint;
    let body_json = body_json.to_string();
    let headers = redacted_headers(headers);
    // a multi-megabyte body takes long enough to redact and sync that it would delay the request
    runtime.spawn_blocking(move || {
        snapshot.write(&dir, &provider, &fingerprint, &body_json, headers);
    });
    Some(path)
}

fn session_tag(session: Option<&str>) -> String {
    match session {
        Some(id) if !id.is_empty() => id.chars().take(8).collect(),
        _ => "nosession".to_string(),
    }
}

struct Snapshot {
    scope: String,
    path: PathBuf,
    ts_ms: u128,
}

impl Snapshot {
    fn new(dir: &Path, provider: &str, session: Option<&str>) -> Self {
        let seq = SNAPSHOT_SEQ.fetch_add(1, Ordering::Relaxed);
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let scope = format!("{provider}-{}", session_tag(session));
        let path = dir.join(format!("{scope}-{ts_ms:013}-{seq:04}.json"));
        Self { scope, path, ts_ms }
    }

    fn write(
        &self,
        dir: &Path,
        provider: &str,
        fingerprint: &RequestFingerprint,
        body_json: &str,
        headers: serde_json::Value,
    ) -> Option<()> {
        std::fs::create_dir_all(dir).ok()?;

        let envelope = serde_json::json!({
            "_snapshot": {
                "provider": provider,
                "timestamp_ms": self.ts_ms,
                "fingerprint": fingerprint,
                "headers": headers,
            },
            "body": redact_body(body_json, redact_secret_text)
        });

        let json = serde_json::to_string_pretty(&envelope).ok()?;
        crate::private_io::write_private(&self.path, json).ok()?;
        prune_old_snapshots(dir, &self.scope);
        Some(())
    }
}

pub fn dump_in(
    dir: &Path,
    provider: &str,
    session: Option<&str>,
    fingerprint: &RequestFingerprint,
    body_json: &str,
    headers: &[(&str, &str)],
) -> Option<PathBuf> {
    let snapshot = Snapshot::new(dir, provider, session);
    snapshot.write(
        dir,
        provider,
        fingerprint,
        body_json,
        redacted_headers(headers),
    )?;
    Some(snapshot.path)
}

fn prune_old_snapshots(dir: &Path, scope: &str) {
    for old in snapshots_newest_first(dir, scope)
        .into_iter()
        .skip(SNAPSHOT_RETENTION)
    {
        let _ = std::fs::remove_file(old);
    }
}

fn snapshots_newest_first(dir: &Path, scope: &str) -> Vec<PathBuf> {
    let prefix = format!("{scope}-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?;
            if !name.starts_with(&prefix) || !name.ends_with(".json") {
                return None;
            }
            let mtime = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((path, mtime))
        })
        .collect();
    files.sort_by_key(|b| std::cmp::Reverse(b.1));
    files.into_iter().map(|(path, _)| path).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recent_snapshots_in(dir: &Path, provider: &str, limit: usize) -> Vec<PathBuf> {
        snapshots_newest_first(dir, provider)
            .into_iter()
            .take(limit)
            .collect()
    }

    #[test]
    fn sensitive_fields_match_ignoring_case_dashes_and_underscores() {
        for name in [
            "token",
            "Refresh-Token",
            "API_KEY",
            "apiKey",
            "set_cookie",
            "SetCookie",
        ] {
            assert!(is_sensitive_json_field(name), "{name} should be sensitive");
        }
        for name in ["tokens", "x_token", "key", "t\u{f6}ken", ""] {
            assert!(
                !is_sensitive_json_field(name),
                "{name} should not be sensitive"
            );
        }
    }

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
        let path = dump_in(dir.path(), "p", None, &fp, "{}", &headers).expect("dump succeeds");
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

        let path =
            dump_in(dir.path(), "p", None, &fp, &body.to_string(), &[]).expect("dump succeeds");
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
    fn a_recorded_request_is_read_back_per_session_with_its_previous_snapshot() {
        let fp = RequestFingerprint::from_json_sections("[]", "[]", "[]");
        record_request(
            "sess-a",
            fp,
            true,
            3600,
            Some(PathBuf::from("/tmp/a-1.json")),
        );
        record_request(
            "sess-b",
            fp,
            false,
            300,
            Some(PathBuf::from("/tmp/b-1.json")),
        );
        record_request(
            "sess-a",
            fp,
            true,
            3600,
            Some(PathBuf::from("/tmp/a-2.json")),
        );

        let a = last_request("sess-a").unwrap();
        assert_eq!(a.snapshot, Some(PathBuf::from("/tmp/a-2.json")));
        assert_eq!(a.previous_snapshot, Some(PathBuf::from("/tmp/a-1.json")));
        assert!(a.context_management);
        assert_eq!(a.cache_ttl_secs, 3600);
        let b = last_request("sess-b").unwrap();
        assert_eq!(b.snapshot, Some(PathBuf::from("/tmp/b-1.json")));
        assert_eq!(b.previous_snapshot, None);
        assert!(last_request("sess-c").is_none());
    }

    #[test]
    fn the_billing_header_block_does_not_move_the_system_fingerprint() {
        let with_a = r#"[{"type":"text","text":"x-anthropic-billing-header: cc_prev_req=req_1"},{"type":"text","text":"You are mush"}]"#;
        let with_b = r#"[{"type":"text","text":"x-anthropic-billing-header: cc_prev_req=req_2"},{"type":"text","text":"You are mush"}]"#;
        let other = r#"[{"type":"text","text":"x-anthropic-billing-header: cc_prev_req=req_2"},{"type":"text","text":"You are prime"}]"#;

        let a = RequestFingerprint::from_json_sections(with_a, "[]", "[]");
        let b = RequestFingerprint::from_json_sections(with_b, "[]", "[]");
        let c = RequestFingerprint::from_json_sections(other, "[]", "[]");

        assert_eq!(a.system, b.system);
        assert_ne!(a.system, c.system);
    }

    #[test]
    fn snapshots_are_pruned_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let fp = RequestFingerprint::from_json_sections("[]", "[]", "[]");
        for i in 0..(SNAPSHOT_RETENTION + 2) {
            dump_in(
                dir.path(),
                "anthropic",
                Some("aaaaaaaa11"),
                &fp,
                &format!("{{\"a\":{i}}}"),
                &[],
            )
            .unwrap();
        }
        for i in 0..2 {
            dump_in(
                dir.path(),
                "anthropic",
                Some("bbbbbbbb22"),
                &fp,
                &format!("{{\"b\":{i}}}"),
                &[],
            )
            .unwrap();
        }
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names
                .iter()
                .filter(|n| n.starts_with("anthropic-aaaaaaaa-"))
                .count(),
            SNAPSHOT_RETENTION
        );
        assert_eq!(
            names
                .iter()
                .filter(|n| n.starts_with("anthropic-bbbbbbbb-"))
                .count(),
            2
        );
    }

    #[test]
    fn dump_writes_file_with_fingerprint_header() {
        let dir = tempfile::tempdir().unwrap();
        let fp = RequestFingerprint::from_json_sections("[]", "[]", "[]");
        let body = r#"{"model":"test","messages":[]}"#;
        let path =
            dump_in(dir.path(), "testprovider", None, &fp, body, &[]).expect("dump succeeds");
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
            dump_in(
                dir.path(),
                "p",
                None,
                &fp,
                &format!(r#"{{"call":{i}}}"#),
                &[],
            )
            .unwrap();
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
            dump_in(
                dir.path(),
                "pr",
                None,
                &fp,
                &format!(r#"{{"call":{i}}}"#),
                &[],
            )
            .unwrap();
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
