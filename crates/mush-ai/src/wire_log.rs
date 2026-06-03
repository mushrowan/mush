//! full-fidelity conversation journal for the perf build.
//!
//! when `MUSH_WIRE_LOG` points at a writable file path, every llm http
//! call is appended as newline-delimited json: the outgoing request
//! (full body + redacted headers), the response status + headers, every
//! raw sse event untruncated, and the final assembled assistant message
//! (tool calls with full arguments). this is a complete, parseable
//! record of exactly what crossed the wire, kept separate from the
//! noisy `mush.log` so it stays small and greppable.
//!
//! a single monotonic `call_id` threads the request, response, sse
//! events, and assembled message of one logical call together so
//! concurrent streams (e.g. a background title generation racing the
//! main turn) stay disentanglable.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

use crate::request_snapshot::{
    REDACTED_HEADER_VALUE, is_sensitive_header, is_sensitive_json_field, looks_like_secret,
};

static CALL_SEQ: AtomicU64 = AtomicU64::new(0);

/// allocate a fresh correlation id for one llm http call. cheap and
/// always available, even when journalling is disabled, so callers can
/// thread it unconditionally
#[must_use]
pub fn next_call_id() -> u64 {
    CALL_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// the process-wide journal file handle, opened lazily from
/// `MUSH_WIRE_LOG` on first use. `None` when the env var is unset or the
/// file can't be opened, in which case all log calls are no-ops
fn writer() -> Option<&'static Mutex<std::fs::File>> {
    static WRITER: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    WRITER
        .get_or_init(|| {
            let path = std::env::var_os("MUSH_WIRE_LOG")?;
            if let Some(parent) = std::path::Path::new(&path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
                .map(Mutex::new)
        })
        .as_ref()
}

/// whether the conversation journal is active for this process
#[must_use]
pub fn enabled() -> bool {
    writer().is_some()
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// append one record as a json line. no-op when journalling is disabled
fn write_record(record: &Value) {
    let Some(handle) = writer() else { return };
    let Ok(mut line) = serde_json::to_string(record) else {
        return;
    };
    line.push('\n');
    if let Ok(mut file) = handle.lock() {
        let _ = file.write_all(line.as_bytes());
    }
}

/// redact secret-shaped whitespace-delimited tokens while preserving the
/// original whitespace exactly, so the journal stays a faithful copy of
/// model output (the reflowing redactor used for previews would collapse
/// runs of spaces and newlines)
#[must_use]
pub fn redact_secrets_preserving(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            flush_word(&mut word, &mut out);
            out.push(ch);
        } else {
            word.push(ch);
        }
    }
    flush_word(&mut word, &mut out);
    out
}

fn flush_word(word: &mut String, out: &mut String) {
    if word.is_empty() {
        return;
    }
    let trimmed = word.trim_matches(|ch: char| ch.is_ascii_punctuation());
    if !trimmed.is_empty() && looks_like_secret(trimmed) {
        out.push_str(&word.replacen(trimmed, REDACTED_HEADER_VALUE, 1));
    } else {
        out.push_str(word);
    }
    word.clear();
}

/// recursively redact a json value in place: sensitive field values
/// (api keys, tokens) become the redaction marker, and every string is
/// scrubbed of secret-shaped tokens without disturbing its whitespace
fn redact_value_preserving(value: &mut Value, field: Option<&str>) {
    if field.is_some_and(is_sensitive_json_field) {
        *value = Value::String(REDACTED_HEADER_VALUE.to_string());
        return;
    }
    match value {
        Value::String(text) => *text = redact_secrets_preserving(text),
        Value::Array(items) => {
            for item in items {
                redact_value_preserving(item, None);
            }
        }
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                let key = key.clone();
                redact_value_preserving(val, Some(&key));
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// parse a request/response body into json (redacted), falling back to a
/// redacted string when it isn't valid json
fn redact_body(body_json: &str) -> Value {
    match serde_json::from_str::<Value>(body_json) {
        Ok(mut value) => {
            redact_value_preserving(&mut value, None);
            value
        }
        Err(_) => Value::String(redact_secrets_preserving(body_json)),
    }
}

fn redact_headers(headers: &[(&str, &str)]) -> Value {
    let map: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(name, value)| {
            let safe = if is_sensitive_header(name) {
                REDACTED_HEADER_VALUE.to_string()
            } else {
                (*value).to_string()
            };
            ((*name).to_string(), Value::String(safe))
        })
        .collect();
    Value::Object(map)
}

fn build_request(
    call_id: u64,
    provider: &str,
    model: &str,
    url: &str,
    headers: &[(&str, &str)],
    body_json: &str,
) -> Value {
    json!({
        "ts_ms": now_ms(),
        "call_id": call_id,
        "kind": "request",
        "provider": provider,
        "model": model,
        "url": url,
        "headers": redact_headers(headers),
        "body": redact_body(body_json),
    })
}

fn build_response(
    call_id: u64,
    provider: &str,
    model: &str,
    status: u16,
    headers: &[(&str, &str)],
) -> Value {
    json!({
        "ts_ms": now_ms(),
        "call_id": call_id,
        "kind": "response",
        "provider": provider,
        "model": model,
        "status": status,
        "headers": redact_headers(headers),
    })
}

fn build_sse(call_id: u64, seq: u64, event: &str, data: &str) -> Value {
    json!({
        "ts_ms": now_ms(),
        "call_id": call_id,
        "kind": "sse",
        "seq": seq,
        "event": event,
        "data": redact_secrets_preserving(data),
    })
}

fn build_assembled(call_id: u64, message: &crate::types::AssistantMessage) -> Value {
    let mut body = serde_json::to_value(message).unwrap_or(Value::Null);
    redact_value_preserving(&mut body, None);
    json!({
        "ts_ms": now_ms(),
        "call_id": call_id,
        "kind": "assembled",
        "message": body,
    })
}

/// record an outgoing llm request with its full (redacted) body
pub fn log_request(
    call_id: u64,
    provider: &str,
    model: &str,
    url: &str,
    headers: &[(&str, &str)],
    body_json: &str,
) {
    if !enabled() {
        return;
    }
    write_record(&build_request(
        call_id, provider, model, url, headers, body_json,
    ));
}

/// record the response status line and headers for a call
pub fn log_response(
    call_id: u64,
    provider: &str,
    model: &str,
    status: u16,
    headers: &[(&str, &str)],
) {
    if !enabled() {
        return;
    }
    write_record(&build_response(call_id, provider, model, status, headers));
}

/// record one raw sse event, untruncated
pub fn log_sse(call_id: u64, seq: u64, event: &str, data: &str) {
    if !enabled() {
        return;
    }
    write_record(&build_sse(call_id, seq, event, data));
}

/// record the final assembled assistant message, including tool calls
/// with their full arguments
pub fn log_assembled(call_id: u64, message: &crate::types::AssistantMessage) {
    if !enabled() {
        return;
    }
    write_record(&build_assembled(call_id, message));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_ids_are_monotonic() {
        let a = next_call_id();
        let b = next_call_id();
        assert!(b > a, "call ids must increase: {a} then {b}");
    }

    #[test]
    fn headers_redact_sensitive_keep_rest() {
        let headers = build_request(
            0,
            "anthropic",
            "claude",
            "https://x/v1/messages",
            &[
                ("authorization", "Bearer sk-ant-secret-token-value"),
                ("x-api-key", "sk-secret"),
                ("anthropic-version", "2023-06-01"),
            ],
            "{}",
        );
        let h = &headers["headers"];
        assert_eq!(h["authorization"], REDACTED_HEADER_VALUE);
        assert_eq!(h["x-api-key"], REDACTED_HEADER_VALUE);
        assert_eq!(h["anthropic-version"], "2023-06-01");
    }

    #[test]
    fn preserving_redactor_keeps_whitespace() {
        let input = "foo  bar\n\tbaz sk-1234567890abcdef end";
        let out = redact_secrets_preserving(input);
        assert!(
            out.contains("foo  bar\n\tbaz "),
            "whitespace mangled: {out:?}"
        );
        assert!(!out.contains("sk-1234567890abcdef"), "secret leaked: {out}");
        assert!(out.contains(REDACTED_HEADER_VALUE));
        assert!(out.ends_with(" end"));
    }

    #[test]
    fn request_body_parsed_and_redacted() {
        let body = r#"{"model":"x","api_key":"sk-1234567890ab","messages":[{"role":"user"}]}"#;
        let rec = build_request(7, "openai", "gpt", "https://x", &[], body);
        assert_eq!(rec["kind"], "request");
        assert_eq!(rec["call_id"], 7);
        assert_eq!(rec["body"]["model"], "x");
        assert_eq!(rec["body"]["api_key"], REDACTED_HEADER_VALUE);
        assert_eq!(rec["body"]["messages"][0]["role"], "user");
    }

    #[test]
    fn sse_data_is_not_truncated() {
        let big = "x".repeat(4096);
        let data = format!("{{\"type\":\"input_json_delta\",\"partial_json\":\"{big}\"}}");
        let rec = build_sse(3, 12, "content_block_delta", &data);
        assert_eq!(rec["kind"], "sse");
        assert_eq!(rec["seq"], 12);
        assert_eq!(rec["event"], "content_block_delta");
        let logged = rec["data"].as_str().unwrap();
        assert!(
            logged.contains(&big),
            "sse data was truncated: {} chars",
            logged.len()
        );
    }
}
