//! credential storage for api-key based providers
//!
//! oauth credentials live in `oauth.json` (see [`crate::oauth`]) because
//! they're rich (access + refresh + expiry). this module covers the
//! simpler "single secret per provider" case for plain api-key auth
//! (openrouter, openai, anthropic via key, custom providers).
//!
//! storage is abstracted behind [`CredentialStore`] so we can swap in
//! the os-native keychain via [`keyring-core`] when available, falling
//! back to a file at `~/.config/mush/api-keys.json` (mode 0o600) on
//! headless / containerised hosts. callers shouldn't care which backend
//! is active

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::types::ApiKey;

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum CredentialError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse credential file: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("keyring backend error: {0}")]
    Keyring(String),
}

/// platform-independent credential storage. callers identify each
/// secret by a stable `provider_id` (e.g. `anthropic`, `openrouter`).
/// implementations must be safe to share across threads
pub trait CredentialStore: Send + Sync + std::fmt::Debug {
    /// retrieve a stored secret. returns `Ok(None)` when no entry
    /// exists, distinct from `Err` (which means the lookup itself
    /// failed). secrets are returned as `ApiKey` so the redaction
    /// guarantees flow through the auth pipeline
    fn get(&self, provider_id: &str) -> Result<Option<ApiKey>, CredentialError>;

    /// store a secret, replacing any existing value for `provider_id`
    fn set(&self, provider_id: &str, secret: &str) -> Result<(), CredentialError>;

    /// delete a stored secret. returns whether an entry was actually
    /// removed (`false` when nothing was stored to begin with). a
    /// no-op call is not an error
    fn remove(&self, provider_id: &str) -> Result<bool, CredentialError>;

    /// short label describing the active backend (used in toasts and
    /// debug dumps so users can see where their secrets ended up)
    fn backend(&self) -> &'static str;
}

/// file-based credential store at `~/.config/mush/api-keys.json` (mode
/// 0o600 on unix). this is the universal fallback when the os keychain
/// isn't available, and the only backend used in tests
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    /// create a store backed by an explicit path. tests use this with
    /// a `tempfile::tempdir`; runtime callers usually want
    /// [`Self::default`] (via the `Default` trait)
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    fn load(&self) -> Result<std::collections::HashMap<String, String>, CredentialError> {
        if !self.path.exists() {
            return Ok(std::collections::HashMap::new());
        }
        let content = std::fs::read_to_string(&self.path)?;
        if content.trim().is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        Ok(serde_json::from_str(&content)?)
    }

    fn save(
        &self,
        entries: &std::collections::HashMap<String, String>,
    ) -> Result<(), CredentialError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(entries)?;
        crate::private_io::write_private(&self.path, json)?;
        Ok(())
    }
}

impl Default for FileCredentialStore {
    fn default() -> Self {
        Self::at(default_credentials_path())
    }
}

impl CredentialStore for FileCredentialStore {
    fn get(&self, provider_id: &str) -> Result<Option<ApiKey>, CredentialError> {
        Ok(self.load()?.get(provider_id).cloned().and_then(ApiKey::new))
    }

    fn set(&self, provider_id: &str, secret: &str) -> Result<(), CredentialError> {
        let mut entries = self.load()?;
        entries.insert(provider_id.to_string(), secret.to_string());
        self.save(&entries)
    }

    fn remove(&self, provider_id: &str) -> Result<bool, CredentialError> {
        let mut entries = self.load()?;
        let removed = entries.remove(provider_id).is_some();
        if removed {
            self.save(&entries)?;
        }
        Ok(removed)
    }

    fn backend(&self) -> &'static str {
        "file"
    }
}

/// resolve the on-disk path for the file fallback store. mirrors the
/// logic [`crate::oauth::store`] uses for `oauth.json` so users have
/// one config dir to back up
fn default_credentials_path() -> PathBuf {
    if let Ok(dir) = std::env::var("MUSH_CONFIG_DIR") {
        PathBuf::from(dir).join("api-keys.json")
    } else if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(config).join("mush/api-keys.json")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/mush/api-keys.json")
    } else {
        PathBuf::from(".mush/api-keys.json")
    }
}

/// keyring-backed credential store using the os-native keychain. the
/// backing platform store (Apple Keychain on macOS, Credential Store on
/// Windows, kernel keyutils on Linux) is set as the keyring-core
/// process-wide default by [`try_init_keyring`] before any access
#[derive(Debug, Clone)]
pub struct KeyringCredentialStore {
    service: &'static str,
}

impl KeyringCredentialStore {
    /// service name used for every entry. credentials are keyed by
    /// `(service, user)` where `user == provider_id`
    pub const SERVICE: &'static str = "mush";

    fn entry(&self, provider_id: &str) -> Result<keyring_core::Entry, CredentialError> {
        keyring_core::Entry::new(self.service, provider_id)
            .map_err(|e| CredentialError::Keyring(e.to_string()))
    }
}

impl CredentialStore for KeyringCredentialStore {
    fn get(&self, provider_id: &str) -> Result<Option<ApiKey>, CredentialError> {
        match self.entry(provider_id)?.get_password() {
            Ok(value) => Ok(ApiKey::new(value)),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(CredentialError::Keyring(e.to_string())),
        }
    }

    fn set(&self, provider_id: &str, secret: &str) -> Result<(), CredentialError> {
        self.entry(provider_id)?
            .set_password(secret)
            .map_err(|e| CredentialError::Keyring(e.to_string()))
    }

    fn remove(&self, provider_id: &str) -> Result<bool, CredentialError> {
        match self.entry(provider_id)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(e) => Err(CredentialError::Keyring(e.to_string())),
        }
    }

    fn backend(&self) -> &'static str {
        "keyring"
    }
}

/// install the platform-native keyring-core default store. returns
/// whether installation succeeded - a `false` here means the caller
/// should use the file fallback. failures are common on headless linux
/// (no kernel keyutils session, or restricted containers) and harmless
fn try_init_keyring() -> bool {
    #[cfg(target_os = "linux")]
    {
        match linux_keyutils_keyring_store::Store::new() {
            Ok(store) => {
                keyring_core::set_default_store(store);
                true
            }
            Err(e) => {
                tracing::debug!("linux keyutils unavailable, falling back to file: {e}");
                false
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        match apple_native_keyring_store::Store::new() {
            Ok(store) => {
                keyring_core::set_default_store(store);
                true
            }
            Err(e) => {
                tracing::debug!("macos keychain unavailable, falling back to file: {e}");
                false
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        match windows_native_keyring_store::Store::new() {
            Ok(store) => {
                keyring_core::set_default_store(store);
                true
            }
            Err(e) => {
                tracing::debug!("windows credential store unavailable, falling back to file: {e}");
                false
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

/// the resolved credential store for this process. picked once via
/// [`try_init_keyring`] (keyring if the os backend works, file fallback
/// otherwise) and reused for every subsequent call. tests should
/// construct a [`FileCredentialStore`] explicitly with a temp path
/// instead of going through this global
static DEFAULT_STORE: OnceLock<Arc<dyn CredentialStore>> = OnceLock::new();

/// access the canonical store, initialising it on first use
#[must_use]
pub fn default_store() -> Arc<dyn CredentialStore> {
    DEFAULT_STORE
        .get_or_init(|| {
            if try_init_keyring() {
                Arc::new(KeyringCredentialStore {
                    service: KeyringCredentialStore::SERVICE,
                })
            } else {
                Arc::new(FileCredentialStore::default())
            }
        })
        .clone()
}

/// describe the active backend, e.g. for `/debug` output
#[must_use]
pub fn active_backend() -> &'static str {
    default_store().backend()
}

/// path used by [`FileCredentialStore::default`]. exposed for users so
/// `/login` toasts can mention where the file landed
#[must_use]
pub fn fallback_file_path() -> PathBuf {
    default_credentials_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, FileCredentialStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-keys.json");
        (dir, FileCredentialStore::at(path))
    }

    #[test]
    fn file_store_get_returns_none_when_file_missing() {
        let (_dir, store) = temp_store();
        assert!(store.get("anthropic").unwrap().is_none());
    }

    #[test]
    fn file_store_roundtrip_set_get() {
        let (_dir, store) = temp_store();
        store.set("openrouter", "sk-or-test-1234").unwrap();
        let key = store.get("openrouter").unwrap().unwrap();
        assert_eq!(key.expose(), "sk-or-test-1234");
    }

    #[test]
    fn file_store_set_overwrites_existing() {
        let (_dir, store) = temp_store();
        store.set("openrouter", "first").unwrap();
        store.set("openrouter", "second").unwrap();
        assert_eq!(store.get("openrouter").unwrap().unwrap().expose(), "second");
    }

    #[test]
    fn file_store_remove_returns_true_when_present() {
        let (_dir, store) = temp_store();
        store.set("openrouter", "v").unwrap();
        assert!(store.remove("openrouter").unwrap());
        assert!(store.get("openrouter").unwrap().is_none());
    }

    #[test]
    fn file_store_remove_returns_false_when_missing() {
        let (_dir, store) = temp_store();
        // never wrote anything for this provider id
        assert!(!store.remove("openrouter").unwrap());
    }

    #[test]
    fn file_store_keeps_other_entries_intact_on_remove() {
        // removing one provider must not blow away the rest of the file
        let (_dir, store) = temp_store();
        store.set("openai", "k1").unwrap();
        store.set("openrouter", "k2").unwrap();
        assert!(store.remove("openai").unwrap());
        assert!(store.get("openai").unwrap().is_none());
        assert_eq!(store.get("openrouter").unwrap().unwrap().expose(), "k2");
    }

    #[cfg(unix)]
    #[test]
    fn file_store_writes_with_mode_600() {
        // secrets must land with private permissions even when the
        // parent directory is world-readable
        use std::os::unix::fs::PermissionsExt;
        let (_dir, store) = temp_store();
        store.set("anthropic", "secret").unwrap();
        let mode = std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600, got {mode:o}");
    }

    #[test]
    fn file_store_handles_provider_ids_with_hyphens() {
        // provider ids in the wild contain hyphens (`openai-codex`,
        // `claude-pro-max`). they must round-trip cleanly through json
        let (_dir, store) = temp_store();
        store.set("openai-codex", "k").unwrap();
        store.set("anthropic-api", "k2").unwrap();
        assert_eq!(store.get("openai-codex").unwrap().unwrap().expose(), "k");
        assert_eq!(store.get("anthropic-api").unwrap().unwrap().expose(), "k2");
    }

    #[test]
    fn file_store_handles_empty_file() {
        // a zero-byte file (e.g. a botched write) shouldn't poison the
        // store: load returns empty and subsequent writes succeed
        let (_dir, store) = temp_store();
        if let Some(parent) = store.path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&store.path, "").unwrap();
        assert!(store.get("anthropic").unwrap().is_none());
        store.set("anthropic", "v").unwrap();
        assert_eq!(store.get("anthropic").unwrap().unwrap().expose(), "v");
    }

    #[test]
    fn backend_label_identifies_implementation() {
        let (_dir, store) = temp_store();
        assert_eq!(store.backend(), "file");
    }
}
