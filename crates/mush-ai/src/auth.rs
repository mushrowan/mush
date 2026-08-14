use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::credentials::{CredentialStore, default_store};
use crate::login;
use crate::types::{ApiKey, Provider};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedAuth {
    pub api_key: Option<ApiKey>,
    pub account_id: Option<String>,
}

pub async fn resolve_auth(
    provider: &Provider,
    config_api_keys: &HashMap<String, ApiKey>,
    credential_store: &dyn CredentialStore,
) -> ResolvedAuth {
    if let Some(key) = env_api_key(provider) {
        return ResolvedAuth {
            api_key: Some(key),
            account_id: None,
        };
    }

    if let Some(key) = config_api_keys.get(config_key(provider)) {
        return ResolvedAuth {
            api_key: Some(key.clone()),
            account_id: None,
        };
    }

    if let Ok(Some(key)) = credential_store.get(storage_key(provider)) {
        return ResolvedAuth {
            api_key: Some(key),
            account_id: None,
        };
    }

    if let Some(oauth_provider_id) = oauth_provider_id(provider)
        && let Ok(Some(token)) = crate::oauth::get_oauth_token(oauth_provider_id).await
    {
        return ResolvedAuth {
            api_key: ApiKey::new(token),
            account_id: login::oauth_account_id(oauth_provider_id),
        };
    }

    ResolvedAuth::default()
}

pub async fn resolve_auth_from_default_sources(provider: &Provider) -> ResolvedAuth {
    let config_api_keys = load_config_api_keys();
    let credential_store = default_store();
    resolve_auth(provider, &config_api_keys, credential_store.as_ref()).await
}

#[must_use]
pub fn load_config_api_keys() -> HashMap<String, ApiKey> {
    let path = config_path();
    let Ok(content) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };

    toml::from_str::<ConfigFile>(&content)
        .map(|cfg| cfg.api_keys.into_map())
        .unwrap_or_default()
}

fn env_api_key(provider: &Provider) -> Option<ApiKey> {
    match provider {
        Provider::Anthropic => crate::env::anthropic_api_key(),
        _ => crate::env::env_api_key(provider),
    }
}

fn oauth_provider_id(provider: &Provider) -> Option<&'static str> {
    match provider {
        Provider::Anthropic => Some("anthropic"),
        Provider::Custom(name) if name == "openai-codex" => Some("openai-codex"),
        _ => None,
    }
}

fn config_key(provider: &Provider) -> &str {
    match provider {
        Provider::Anthropic => "anthropic",
        Provider::OpenRouter => "openrouter",
        Provider::Custom(name) => name,
    }
}

fn storage_key(provider: &Provider) -> &str {
    config_key(provider)
}

fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MUSH_CONFIG_DIR") {
        PathBuf::from(dir)
    } else if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(config).join("mush")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/mush")
    } else {
        PathBuf::from(".mush")
    }
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    api_keys: ConfigApiKeys,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigApiKeys {
    anthropic: Option<ApiKey>,
    openrouter: Option<ApiKey>,
    openai: Option<ApiKey>,
    #[serde(flatten)]
    other: HashMap<String, ApiKey>,
}

impl ConfigApiKeys {
    fn into_map(self) -> HashMap<String, ApiKey> {
        let mut map = self.other;
        if let Some(key) = self.anthropic {
            map.insert("anthropic".into(), key);
        }
        if let Some(key) = self.openrouter {
            map.insert("openrouter".into(), key);
        }
        if let Some(key) = self.openai {
            map.insert("openai".into(), key);
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::credentials::FileCredentialStore;

    #[tokio::test]
    async fn resolve_auth_uses_config_key_for_openai() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::at(dir.path().join("api-keys.json"));
        let key = ApiKey::new("sk-openai-config-test").unwrap();
        let mut config_api_keys = HashMap::new();
        config_api_keys.insert("openai".into(), key.clone());

        let resolved =
            resolve_auth(&Provider::Custom("openai".into()), &config_api_keys, &store).await;

        assert_eq!(
            resolved,
            ResolvedAuth {
                api_key: Some(key),
                account_id: None,
            }
        );
    }

    #[tokio::test]
    async fn resolve_auth_uses_stored_key_for_openai() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::at(dir.path().join("api-keys.json"));
        store.set("openai", "sk-openai-stored-test").unwrap();

        let resolved =
            resolve_auth(&Provider::Custom("openai".into()), &HashMap::new(), &store).await;

        assert_eq!(
            resolved,
            ResolvedAuth {
                api_key: Some(ApiKey::new("sk-openai-stored-test").unwrap()),
                account_id: None,
            }
        );
    }
}
