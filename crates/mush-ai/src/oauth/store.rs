//! credential persistence to disk

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::OAuthError;

/// eager refresh buffer: credentials are treated as expired once their
/// remaining lifetime drops below this threshold. prevents a request
/// from landing with a token that expired in-flight, and gives us room
/// to refresh before the actual deadline (mirrors opencode's approach)
const REFRESH_BUFFER_MS: u64 = 5 * 60_000;

/// stored oauth credentials. manual `Debug` impl redacts the tokens so
/// they can't leak via tracing / dbg / assertion messages. explicit
/// field access (`creds.access_token`) is still unrestricted because
/// the HTTP code needs the bearer value - this is leak-prevention for
/// logging, not a full `secrecy` crate style sealed wrapper
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    pub access_token: String,
    pub refresh_token: String,
    /// epoch ms when the access token expires
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl std::fmt::Debug for OAuthCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthCredentials")
            .field("access_token", &redacted_token(&self.access_token))
            .field("refresh_token", &redacted_token(&self.refresh_token))
            .field("expires_at", &self.expires_at)
            .field("account_id", &self.account_id)
            .finish()
    }
}

/// format an oauth token for debug output: first 4 chars + `…` when long
/// enough to recognise without leaking, else `***`. mirrors `ApiKey`
fn redacted_token(token: &str) -> String {
    if token.chars().count() > 8 {
        let prefix: String = token.chars().take(4).collect();
        format!("{prefix}…")
    } else {
        "***".into()
    }
}

impl OAuthCredentials {
    /// whether the token is expired or close enough to expiry that we
    /// should refresh proactively. see [`REFRESH_BUFFER_MS`]
    pub fn is_expired(&self) -> bool {
        timestamp_ms().saturating_add(REFRESH_BUFFER_MS) >= self.expires_at
    }
}

/// all stored credentials keyed by provider name
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CredentialStore {
    #[serde(flatten)]
    pub providers: HashMap<String, OAuthCredentials>,
}

fn credentials_path() -> PathBuf {
    if let Ok(dir) = std::env::var("MUSH_CONFIG_DIR") {
        PathBuf::from(dir).join("oauth.json")
    } else if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(config).join("mush/oauth.json")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/mush/oauth.json")
    } else {
        PathBuf::from(".mush/oauth.json")
    }
}

/// load credentials from disk
pub fn load_credentials() -> Result<CredentialStore, OAuthError> {
    load_credentials_from(&credentials_path())
}

/// load credentials from a specific path
pub fn load_credentials_from(path: &PathBuf) -> Result<CredentialStore, OAuthError> {
    if !path.exists() {
        return Ok(CredentialStore::default());
    }
    let content = std::fs::read_to_string(path)?;
    if content.trim().is_empty() {
        return Ok(CredentialStore::default());
    }
    let store: CredentialStore = serde_json::from_str(&content)?;
    Ok(store)
}

/// save credentials to disk with private permissions (0o600 on unix)
pub fn save_credentials(store: &CredentialStore) -> Result<(), OAuthError> {
    let path = credentials_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(store)?;
    crate::private_io::write_private(&path, json)?;
    Ok(())
}

fn timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_debug_redacts_tokens() {
        let creds = OAuthCredentials {
            access_token: "sk-ant-oat-this-is-a-real-looking-token".into(),
            refresh_token: "sk-ant-ort-this-is-the-refresh-secret".into(),
            expires_at: 12345,
            account_id: Some("account-abc".into()),
        };
        let dbg = format!("{creds:?}");
        assert!(
            !dbg.contains("this-is-a-real-looking-token"),
            "access_token leaked in Debug: {dbg}"
        );
        assert!(
            !dbg.contains("this-is-the-refresh-secret"),
            "refresh_token leaked in Debug: {dbg}"
        );
        // non-secret fields should still surface
        assert!(
            dbg.contains("12345"),
            "expires_at missing from debug: {dbg}"
        );
        assert!(
            dbg.contains("account-abc"),
            "account_id missing from debug: {dbg}"
        );
    }

    #[test]
    fn credentials_expired_check() {
        let creds = OAuthCredentials {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: 0,
            account_id: None,
        };
        assert!(creds.is_expired());

        // well beyond the eager refresh buffer
        let creds = OAuthCredentials {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: timestamp_ms() + 60 * 60_000, // 1 hour
            account_id: None,
        };
        assert!(!creds.is_expired());
    }

    #[test]
    fn credentials_expired_within_refresh_buffer() {
        // token that expires in 1 minute counts as "expired" for the
        // purposes of the eager refresh threshold (5 min by default)
        let creds = OAuthCredentials {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: timestamp_ms() + 60_000, // 1 minute
            account_id: None,
        };
        assert!(
            creds.is_expired(),
            "token within refresh buffer should be treated as expired"
        );
    }

    #[test]
    fn credentials_not_expired_beyond_refresh_buffer() {
        // token with > 5 minutes left should NOT be treated as expired
        let creds = OAuthCredentials {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: timestamp_ms() + 10 * 60_000, // 10 minutes
            account_id: None,
        };
        assert!(
            !creds.is_expired(),
            "token with >5min left should not be treated as expired"
        );
    }

    #[test]
    fn store_roundtrip() {
        let mut store = CredentialStore::default();
        store.providers.insert(
            "anthropic".into(),
            OAuthCredentials {
                access_token: "acc".into(),
                refresh_token: "ref".into(),
                expires_at: 12345,
                account_id: None,
            },
        );

        let json = serde_json::to_string(&store).unwrap();
        let restored: CredentialStore = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.providers["anthropic"].access_token, "acc");
    }

    #[test]
    fn empty_json_is_parse_error() {
        let result: Result<CredentialStore, _> = serde_json::from_str("");
        assert!(result.is_err(), "empty string should fail to parse");
    }

    #[test]
    fn load_credentials_handles_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth.json");
        std::fs::write(&path, "").unwrap();

        let store = load_credentials_from(&path).unwrap();
        assert!(store.providers.is_empty());
    }

    #[test]
    fn load_credentials_handles_whitespace_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth.json");
        std::fs::write(&path, "  \n  ").unwrap();

        let store = load_credentials_from(&path).unwrap();
        assert!(store.providers.is_empty());
    }

    #[test]
    fn load_credentials_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");

        let store = load_credentials_from(&path).unwrap();
        assert!(store.providers.is_empty());
    }

    #[test]
    fn store_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth.json");

        let mut store = CredentialStore::default();
        store.providers.insert(
            "anthropic".into(),
            OAuthCredentials {
                access_token: "token".into(),
                refresh_token: "refresh".into(),
                expires_at: 99999,
                account_id: None,
            },
        );

        let json = serde_json::to_string_pretty(&store).unwrap();
        std::fs::write(&path, &json).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let loaded: CredentialStore = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.providers.len(), 1);
    }
}
