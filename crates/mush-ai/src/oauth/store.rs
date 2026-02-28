//! credential persistence to disk

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::OAuthError;

/// stored oauth credentials
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    pub access_token: String,
    pub refresh_token: String,
    /// epoch ms when the access token expires
    pub expires_at: u64,
}

impl OAuthCredentials {
    pub fn is_expired(&self) -> bool {
        timestamp_ms() >= self.expires_at
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
    let path = credentials_path();
    if !path.exists() {
        return Ok(CredentialStore::default());
    }
    let content = std::fs::read_to_string(path)?;
    let store: CredentialStore = serde_json::from_str(&content)?;
    Ok(store)
}

/// save credentials to disk
pub fn save_credentials(store: &CredentialStore) -> Result<(), OAuthError> {
    let path = credentials_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(store)?;
    std::fs::write(path, json)?;
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
    fn credentials_expired_check() {
        let creds = OAuthCredentials {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: 0,
        };
        assert!(creds.is_expired());

        let creds = OAuthCredentials {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: timestamp_ms() + 60_000,
        };
        assert!(!creds.is_expired());
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
            },
        );

        let json = serde_json::to_string(&store).unwrap();
        let restored: CredentialStore = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.providers["anthropic"].access_token, "acc");
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
            },
        );

        let json = serde_json::to_string_pretty(&store).unwrap();
        std::fs::write(&path, &json).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let loaded: CredentialStore = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.providers.len(), 1);
    }
}
