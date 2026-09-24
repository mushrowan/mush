use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::OAuthError;
use crate::types::AccountLabel;

const REFRESH_BUFFER_MS: u64 = 5 * 60_000;

#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    pub access_token: String,
    pub refresh_token: String,
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

fn redacted_token(token: &str) -> String {
    if token.chars().count() > 8 {
        let prefix: String = token.chars().take(4).collect();
        format!("{prefix}…")
    } else {
        "***".into()
    }
}

impl OAuthCredentials {
    pub fn is_expired(&self) -> bool {
        timestamp_ms().saturating_add(REFRESH_BUFFER_MS) >= self.expires_at
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProviderAccounts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default: Option<AccountLabel>,
    accounts: BTreeMap<AccountLabel, OAuthCredentials>,
}

impl<'de> Deserialize<'de> for ProviderAccounts {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stored {
            Accounts {
                #[serde(default)]
                default: Option<AccountLabel>,
                accounts: BTreeMap<AccountLabel, OAuthCredentials>,
            },
            Single(OAuthCredentials),
        }
        Ok(match Stored::deserialize(deserializer)? {
            Stored::Accounts { default, accounts } => Self { default, accounts },
            Stored::Single(creds) => Self {
                default: None,
                accounts: BTreeMap::from([(AccountLabel::default_label(), creds)]),
            },
        })
    }
}

impl ProviderAccounts {
    #[must_use]
    pub fn default_label(&self) -> Option<&AccountLabel> {
        self.default
            .as_ref()
            .filter(|label| self.accounts.contains_key(*label))
            .or_else(|| self.accounts.keys().next())
    }

    #[must_use]
    pub fn get(&self, label: Option<&AccountLabel>) -> Option<(&AccountLabel, &OAuthCredentials)> {
        let label = label.or_else(|| self.default_label())?;
        self.accounts.get_key_value(label)
    }

    pub fn labels(&self) -> impl Iterator<Item = &AccountLabel> {
        self.accounts.keys()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CredentialStore {
    #[serde(flatten)]
    pub providers: HashMap<String, ProviderAccounts>,
}

impl CredentialStore {
    #[must_use]
    pub fn has_provider(&self, provider: &str) -> bool {
        self.providers
            .get(provider)
            .is_some_and(|accounts| !accounts.is_empty())
    }

    #[must_use]
    pub fn accounts(&self, provider: &str) -> Option<&ProviderAccounts> {
        self.providers
            .get(provider)
            .filter(|accounts| !accounts.is_empty())
    }

    #[must_use]
    pub fn get(
        &self,
        provider: &str,
        label: Option<&AccountLabel>,
    ) -> Option<(&AccountLabel, &OAuthCredentials)> {
        self.providers.get(provider)?.get(label)
    }

    pub fn insert(&mut self, provider: &str, label: AccountLabel, creds: OAuthCredentials) {
        let accounts = self.providers.entry(provider.to_string()).or_default();
        if accounts.default_label().is_none() {
            accounts.default = Some(label.clone());
        }
        accounts.accounts.insert(label, creds);
    }

    pub fn remove(&mut self, provider: &str, label: Option<&AccountLabel>) -> Option<AccountLabel> {
        let accounts = self.providers.get_mut(provider)?;
        let label = label.or_else(|| accounts.default_label())?.clone();
        accounts.accounts.remove(&label)?;
        if accounts.default.as_ref() == Some(&label) {
            accounts.default = accounts.accounts.keys().next().cloned();
        }
        if accounts.accounts.is_empty() {
            self.providers.remove(provider);
        }
        Some(label)
    }

    pub fn set_default(&mut self, provider: &str, label: &AccountLabel) -> bool {
        let Some(accounts) = self.providers.get_mut(provider) else {
            return false;
        };
        if !accounts.accounts.contains_key(label) {
            return false;
        }
        accounts.default = Some(label.clone());
        true
    }
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

pub fn load_credentials() -> Result<CredentialStore, OAuthError> {
    load_credentials_from(&credentials_path())
}

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

pub fn save_credentials(store: &CredentialStore) -> Result<(), OAuthError> {
    save_credentials_to(&credentials_path(), store)
}

fn save_credentials_to(path: &std::path::Path, store: &CredentialStore) -> Result<(), OAuthError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(store)?;
    crate::private_io::write_private(path, json)?;
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

        let creds = OAuthCredentials {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: timestamp_ms() + 60 * 60_000,
            account_id: None,
        };
        assert!(!creds.is_expired());
    }

    #[test]
    fn credentials_expired_within_refresh_buffer() {
        let creds = OAuthCredentials {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: timestamp_ms() + 60_000,
            account_id: None,
        };
        assert!(
            creds.is_expired(),
            "token within refresh buffer should be treated as expired"
        );
    }

    #[test]
    fn credentials_not_expired_beyond_refresh_buffer() {
        let creds = OAuthCredentials {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: timestamp_ms() + 10 * 60_000,
            account_id: None,
        };
        assert!(
            !creds.is_expired(),
            "token with >5min left should not be treated as expired"
        );
    }

    fn creds(access: &str) -> OAuthCredentials {
        OAuthCredentials {
            access_token: access.into(),
            refresh_token: "ref".into(),
            expires_at: 12345,
            account_id: None,
        }
    }

    fn label(name: &str) -> AccountLabel {
        AccountLabel::parse(name).unwrap()
    }

    #[test]
    fn store_roundtrip() {
        let mut store = CredentialStore::default();
        store.insert("anthropic", label("work"), creds("acc"));

        let json = serde_json::to_string(&store).unwrap();
        let restored: CredentialStore = serde_json::from_str(&json).unwrap();
        let (found, creds) = restored.get("anthropic", None).unwrap();
        assert_eq!(found.as_str(), "work");
        assert_eq!(creds.access_token, "acc");
    }

    #[test]
    fn a_single_login_file_reads_as_the_default_account() {
        let json = r#"{"anthropic":{"access_token":"acc","refresh_token":"ref","expires_at":1}}"#;
        let store: CredentialStore = serde_json::from_str(json).unwrap();
        let (found, creds) = store.get("anthropic", None).unwrap();
        assert_eq!(found.as_str(), AccountLabel::DEFAULT);
        assert_eq!(creds.access_token, "acc");
        assert!(store.get("anthropic", Some(&label("work"))).is_none());
    }

    #[test]
    fn the_first_login_stays_the_default_until_changed() {
        let mut store = CredentialStore::default();
        store.insert("anthropic", label("work"), creds("w"));
        store.insert("anthropic", label("alpha"), creds("a"));
        assert_eq!(store.get("anthropic", None).unwrap().0.as_str(), "work");
        assert_eq!(
            store
                .get("anthropic", Some(&label("alpha")))
                .unwrap()
                .1
                .access_token,
            "a"
        );

        assert!(store.set_default("anthropic", &label("alpha")));
        assert!(!store.set_default("anthropic", &label("missing")));
        assert_eq!(store.get("anthropic", None).unwrap().0.as_str(), "alpha");
    }

    #[test]
    fn removing_the_default_promotes_another_and_the_last_drops_the_provider() {
        let mut store = CredentialStore::default();
        store.insert("anthropic", label("work"), creds("w"));
        store.insert("anthropic", label("home"), creds("h"));

        assert_eq!(store.remove("anthropic", None).unwrap().as_str(), "work");
        assert_eq!(store.get("anthropic", None).unwrap().0.as_str(), "home");
        assert!(store.remove("anthropic", Some(&label("work"))).is_none());
        assert_eq!(
            store
                .remove("anthropic", Some(&label("home")))
                .unwrap()
                .as_str(),
            "home"
        );
        assert!(!store.has_provider("anthropic"));
        assert!(store.providers.is_empty());
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
        store.insert("anthropic", label("work"), creds("token"));
        store.insert("anthropic", label("home"), creds("other"));

        save_credentials_to(&path, &store).unwrap();

        let loaded = load_credentials_from(&path).unwrap();
        let labels: Vec<&str> = loaded
            .accounts("anthropic")
            .unwrap()
            .labels()
            .map(AccountLabel::as_str)
            .collect();
        assert_eq!(labels, ["home", "work"]);
        assert_eq!(loaded.get("anthropic", None).unwrap().0.as_str(), "work");
    }
}
