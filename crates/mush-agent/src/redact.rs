use std::borrow::Cow;
use std::sync::{LazyLock, PoisonError, RwLock};

use mush_ai::types::ToolResultContentPart;
use regex::{Captures, Regex};

use crate::tool::ToolResult;

pub const MARKER: &str = "[REDACTED]";
const MIN_SECRET_LEN: usize = 8;

#[derive(Debug, Clone)]
struct KnownSecret {
    value: String,
    marker: String,
}

static KNOWN: LazyLock<RwLock<Vec<KnownSecret>>> = LazyLock::new(|| {
    let mut known = Vec::new();
    for (name, value) in std::env::vars_os() {
        if let (Some(name), Some(value)) = (name.to_str(), value.to_str()) {
            push_env_secret(&mut known, name, value);
        }
    }
    sort_known(&mut known);
    RwLock::new(known)
});

#[expect(clippy::expect_used, reason = "patterns are literals covered by tests")]
fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("redaction pattern compiles")
}

static SECRET_NAME: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?i)(passw(or)?d|passphrase|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|credential|[_.-]key$)",
    )
});

static SECRET_NAME_HINT: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?i)passw|passphrase|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|credential|[_.-]key",
    )
});

static NON_SECRET_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?i)(file|files|path|dir|cmd|command|url|uri|endpoint|sock|socket|id|ids|name|names|user|username|type|kind|count|size|len|length|timeout|ttl|expiry|expires|expires_at|env|var|header|field|prefix|limit|max|min|mode|policy|store)$",
    )
});

static PEM_PRIVATE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?s)-----BEGIN ([A-Z0-9 ]*)PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
    )
});

static TOKEN_SHAPES: LazyLock<Regex> = LazyLock::new(|| {
    compile(concat!(
        r"AGE-SECRET-KEY-1[0-9A-Z]{20,}",
        r"|\bsk-[A-Za-z0-9_-]{20,}",
        r"|\bgh[pousr]_[A-Za-z0-9]{30,}",
        r"|\bgithub_pat_[A-Za-z0-9_]{30,}",
        r"|\bglpat-[A-Za-z0-9_-]{20,}",
        r"|\bxox[abeprs]-[A-Za-z0-9-]{10,}",
        r"|\bAIza[0-9A-Za-z_-]{35}",
        r"|\bhf_[A-Za-z0-9]{30,}",
        r"|\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
    ))
});

static URL_USERINFO: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"(?P<head>\b[a-zA-Z][a-zA-Z0-9+.-]*://[^\s:/@]*):(?P<password>[^\s@/]+)@")
});

static AUTH_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?i)(?P<head>\bauthorization["']?\s*[:=]\s*["']?(?:bearer|basic|token)\s+)(?P<value>[A-Za-z0-9._~+/=-]{8,})"#,
    )
});

static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?P<name>[A-Za-z_][A-Za-z0-9_.-]*)(?P<quote>["']?)(?P<sep>[ \t]*[=:][ \t]*)(?P<value>"(?:[^"\\\n]|\\.)*"|'[^'\n]*'|[^\s"',;]+)"#,
    )
});

static NON_SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?i)(public|sort|cache|primary|foreign|partition|lookup|map|hash|group|index|session|idempotency|dedupe?|cursor|shortcut|hot)[_.-]?key$",
    )
});

pub fn is_secret_name(name: &str) -> bool {
    SECRET_NAME.is_match(name)
        && !NON_SECRET_SUFFIX.is_match(name)
        && !NON_SECRET_KEY.is_match(name)
}

fn plausible_secret_value(value: &str) -> bool {
    value.len() >= MIN_SECRET_LEN && !value.starts_with('/') && !value.starts_with('~')
}

fn push_env_secret(known: &mut Vec<KnownSecret>, name: &str, value: &str) -> bool {
    if !is_secret_name(name) || !plausible_secret_value(value) {
        return false;
    }
    if known.iter().any(|k| k.value == value) {
        return false;
    }
    known.push(KnownSecret {
        value: value.to_owned(),
        marker: format!("[REDACTED:{name}]"),
    });
    true
}

fn sort_known(known: &mut [KnownSecret]) {
    known.sort_by_key(|k| std::cmp::Reverse(k.value.len()));
}

pub fn register_env_var(name: &str, value: &str) -> bool {
    let mut known = KNOWN.write().unwrap_or_else(PoisonError::into_inner);
    let added = push_env_secret(&mut known, name, value);
    if added {
        sort_known(&mut known);
    }
    added
}

fn redact_known(text: &str) -> Cow<'_, str> {
    let known = KNOWN.read().unwrap_or_else(PoisonError::into_inner);
    let mut out = Cow::Borrowed(text);
    for secret in known.iter() {
        if out.contains(secret.value.as_str()) {
            out = Cow::Owned(out.replace(secret.value.as_str(), &secret.marker));
        }
    }
    out
}

fn looks_like_code(value: &str) -> bool {
    const CODE_CHARS: &[char] = &[
        '$', '(', ')', '{', '}', '[', ']', '<', '>', '&', '|', '=', '`',
    ];
    if value.contains(CODE_CHARS) {
        return true;
    }
    let dotted_path = value.contains('.')
        && value.split('.').all(|part| {
            part.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    dotted_path || value.ends_with(':')
}

fn redact_assignment(caps: &Captures<'_>) -> Option<String> {
    let name = &caps["name"];
    if !is_secret_name(name) {
        return None;
    }
    let value = &caps["value"];
    let (open, inner, close) = match value.as_bytes().first() {
        Some(b'"' | b'\'') if value.len() >= 2 => (
            &value[..1],
            &value[1..value.len() - 1],
            &value[value.len() - 1..],
        ),
        _ => {
            if looks_like_code(value) {
                return None;
            }
            ("", value, "")
        }
    };
    if inner.len() < MIN_SECRET_LEN
        || inner.starts_with("ENC[")
        || inner.starts_with("[REDACTED")
        || inner.contains("${")
        || inner.contains("$(")
        || inner.starts_with('$')
        || (inner.contains('{') && inner.contains('}'))
    {
        return None;
    }
    Some(format!(
        "{name}{quote}{sep}{open}{MARKER}{close}",
        quote = &caps["quote"],
        sep = &caps["sep"],
    ))
}

fn replace_all_cow<'a>(
    text: Cow<'a, str>,
    re: &Regex,
    mut f: impl FnMut(&Captures<'_>) -> Option<String>,
) -> Cow<'a, str> {
    if !re.is_match(&text) {
        return text;
    }
    let mut changed = false;
    let replaced = re
        .replace_all(&text, |caps: &Captures<'_>| match f(caps) {
            Some(replacement) => {
                changed = true;
                replacement
            }
            None => caps[0].to_owned(),
        })
        .into_owned();
    if changed { Cow::Owned(replaced) } else { text }
}

#[must_use]
pub fn redact(text: &str) -> Cow<'_, str> {
    let out = redact_known(text);
    let out = replace_all_cow(out, &PEM_PRIVATE_KEY, |caps| {
        Some(format!("-----BEGIN {}PRIVATE KEY-----{MARKER}", &caps[1]))
    });
    let out = replace_all_cow(out, &TOKEN_SHAPES, |_| Some(MARKER.to_owned()));
    let out = replace_all_cow(out, &URL_USERINFO, |caps| {
        let password = &caps["password"];
        if password.starts_with("[REDACTED") || password.starts_with('$') {
            return None;
        }
        Some(format!("{}:{MARKER}@", &caps["head"]))
    });
    let out = replace_all_cow(out, &AUTH_HEADER, |caps| {
        Some(format!("{}{MARKER}", &caps["head"]))
    });
    if !SECRET_NAME_HINT.is_match(&out) {
        return out;
    }
    replace_all_cow(out, &ASSIGNMENT, redact_assignment)
}

fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            if let Cow::Owned(redacted) = redact(s) {
                *s = redacted;
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json),
        serde_json::Value::Object(map) => map.values_mut().for_each(redact_json),
        _ => {}
    }
}

pub fn redact_result(result: &mut ToolResult) {
    for part in &mut result.content {
        if let ToolResultContentPart::Text(text) = part
            && let Cow::Owned(redacted) = redact(&text.text)
        {
            text.text = redacted;
        }
    }
    if let Some(details) = result.details.as_mut() {
        redact_json(details);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(parts: &[&str]) -> String {
        parts.concat()
    }

    #[test]
    fn clean_text_is_borrowed() {
        let text = "compiled in 4.2s\nTOKENIZERS_PARALLELISM=false\n";
        assert!(matches!(redact(text), Cow::Borrowed(_)));
    }

    #[test]
    fn age_secret_key() {
        let key = secret(&["AGE-SECRET-KEY-1", &"QZ".repeat(29)]);
        let out = redact(&format!("key: {key}\n")).into_owned();
        assert!(!out.contains(&key), "{out}");
    }

    #[test]
    fn pem_private_key_block() {
        let body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC\nBKcwggSjAgEAAoIBAQC7\n";
        let text = secret(&[
            "before\n-----BEGIN OPENSSH ",
            "PRIVATE KEY-----\n",
            body,
            "-----END OPENSSH PRIVATE KEY-----\nafter",
        ]);
        let out = redact(&text);
        assert!(!out.contains("BKcwggSjAgEAAoIBAQC7"), "{out}");
        assert!(
            out.starts_with("before\n") && out.ends_with("after"),
            "{out}"
        );
    }

    #[test]
    fn truncated_pem_block_is_redacted_to_end() {
        let text = secret(&[
            "-----BEGIN ",
            "PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC",
        ]);
        let out = redact(&text);
        assert!(!out.contains("MIIEvQ"), "{out}");
    }

    #[test]
    fn provider_token_shapes() {
        for token in [
            secret(&["sk-ant-api03-", &"a1".repeat(20)]),
            secret(&["ghp_", &"Ab3".repeat(12)]),
            secret(&["github_pat_", &"Ab3_".repeat(10)]),
        ] {
            let out = redact(&format!("value {token} end")).into_owned();
            assert!(!out.contains(&token), "{out}");
        }
    }

    #[test]
    fn restic_env_file_lines() {
        let password = secret(&[
            "space quote \" dollar $HOME ",
            "backtick `id` semicolon ; ampersand &",
        ]);
        let aws = secret(&["wJalrXUtnFEMI", "K7MDENGbPxRfiCY"]);
        let text = format!(
            "RESTIC_PASSWORD='{password}'\nexport AWS_SECRET_ACCESS_KEY={aws}\nAWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE\nRESTIC_PASSWORD_FILE=/run/secrets/restic\n"
        );
        let out = redact(&text);
        assert!(!out.contains(&password) && !out.contains(&aws), "{out}");
        assert!(out.contains("RESTIC_PASSWORD='[REDACTED]'"), "{out}");
        assert!(
            out.contains("AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE"),
            "{out}"
        );
        assert!(
            out.contains("RESTIC_PASSWORD_FILE=/run/secrets/restic"),
            "{out}"
        );
    }

    #[test]
    fn decrypted_sops_yaml() {
        let value = secret(&["K005fQ3x", "Rq9LmZt2"]);
        let text = format!(
            "restic-b2-password: {value}\nrestic-b2-aws-access-key-id: 0051234567890\nrestic-b2-aws-secret-access-key: ENC[AES256_GCM,data:OQ7K]\n"
        );
        let out = redact(&text);
        assert!(!out.contains(&value), "{out}");
        assert!(
            out.contains("restic-b2-aws-access-key-id: 0051234567890"),
            "{out}"
        );
        assert!(out.contains("ENC[AES256_GCM,data:OQ7K]"), "{out}");
    }

    #[test]
    fn json_and_headers_and_urls() {
        let value = secret(&["abcd", "efgh1234"]);
        let text = format!(
            "{{\"api_key\": \"{value}\"}}\nAuthorization: Bearer {value}x\nrest:https://rain:{value}y@backup.example/repo\n"
        );
        let out = redact(&text);
        assert!(!out.contains(&value), "{out}");
        assert!(
            out.contains("rest:https://rain:[REDACTED]@backup.example/repo"),
            "{out}"
        );
    }

    #[test]
    fn code_is_left_alone() {
        let text = [
            "api_key = os.environ[\"API_KEY\"]",
            "    apiKey: options.apiKey,",
            "let token: String = get();",
            "password = get_password()",
            "passwordFile = \"${pkgs.writeText \"x\" \"y\"}\";",
            "SOPS_AGE_KEY_CMD=\"ssh-to-age -i $HOME/.ssh/roarch.pem -private-key\"",
            "secret = f\"{prefix}{suffix}\"",
            "if token == expected_token {",
            "KITTY_PUBLIC_KEY=1:abcdefghijklmnop",
            "\"cache_key\": \"build-artifacts-v2\"",
            "key: some-long-identifier",
        ]
        .join("\n");
        let out = redact(&text);
        assert_eq!(out, text);
    }

    #[test]
    fn registered_env_values_are_replaced_anywhere() {
        let value = secret(&["correct horse ", "battery staple"]);
        assert!(register_env_var("MUSH_TEST_RESTIC_PASSWORD", &value));
        assert!(!register_env_var("MUSH_TEST_HOME", "/home/somebody/x"));
        assert!(!register_env_var(
            "MUSH_TEST_TOKEN_FILE",
            "not-a-path-but-a-file-var"
        ));
        let out = redact(&format!("printed {value} plainly")).into_owned();
        assert_eq!(out, "printed [REDACTED:MUSH_TEST_RESTIC_PASSWORD] plainly");
    }

    #[test]
    fn tool_result_text_and_details() {
        let value = secret(&["ghp_", &"Zx9".repeat(12)]);
        let mut result = ToolResult::text(format!("out {value}"))
            .with_details(serde_json::json!({"stdout": [format!("GITHUB_TOKEN={value}")]}));
        redact_result(&mut result);
        let rendered = serde_json::to_string(&result).unwrap();
        assert!(!rendered.contains(&value), "{rendered}");
    }
}
