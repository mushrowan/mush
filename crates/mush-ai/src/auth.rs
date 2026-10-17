use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::credentials::{CredentialStore, default_store};
use crate::login;
use crate::types::{AccountLabel, ApiKey, Provider};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedAuth {
    pub api_key: Option<ApiKey>,
    pub account_id: Option<String>,
}

pub async fn resolve_auth(
    provider: &Provider,
    config_api_keys: &HashMap<String, ApiKey>,
    credential_store: &dyn CredentialStore,
    account: Option<&AccountLabel>,
) -> ResolvedAuth {
    let oauth_id = oauth_provider_id(provider);
    if let Some(oauth_id) = oauth_id
        && account.is_some()
    {
        return oauth_auth(oauth_id, account).await;
    }

    let key = env_api_key(provider)
        .or_else(|| config_api_keys.get(config_key(provider)).cloned())
        .or_else(|| credential_store.get(config_key(provider)).ok().flatten());
    if key.is_some() {
        return ResolvedAuth {
            api_key: key,
            account_id: None,
        };
    }

    match oauth_id {
        Some(oauth_id) => oauth_auth(oauth_id, None).await,
        None => ResolvedAuth::default(),
    }
}

async fn oauth_auth(oauth_id: &str, account: Option<&AccountLabel>) -> ResolvedAuth {
    match crate::oauth::get_oauth_token(oauth_id, account).await {
        Ok(Some(token)) => ResolvedAuth {
            api_key: ApiKey::new(token),
            account_id: login::oauth_account_id(oauth_id, account),
        },
        Ok(None) => ResolvedAuth::default(),
        Err(error) => {
            tracing::warn!(%error, provider = oauth_id, ?account, "oauth token lookup failed");
            ResolvedAuth::default()
        }
    }
}

#[must_use]
pub fn has_credentials(
    provider: &Provider,
    config_api_keys: &HashMap<String, ApiKey>,
    credential_store: &dyn CredentialStore,
) -> bool {
    env_api_key(provider).is_some()
        || config_api_keys.contains_key(config_key(provider))
        || credential_store
            .get(config_key(provider))
            .ok()
            .flatten()
            .is_some()
        || oauth_provider_id(provider).is_some_and(|oauth_id| {
            crate::oauth::load_credentials().is_ok_and(|store| store.has_provider(oauth_id))
        })
}

pub async fn resolve_auth_from_default_sources(provider: &Provider) -> ResolvedAuth {
    let config_api_keys = load_config_api_keys();
    let credential_store = default_store();
    resolve_auth(provider, &config_api_keys, credential_store.as_ref(), None).await
}

#[must_use]
pub fn load_config_api_keys() -> HashMap<String, ApiKey> {
    let path = config_path();
    let Ok(content) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };

    toml::from_str::<ConfigFile>(&content)
        .map(|cfg| cfg.api_keys)
        .unwrap_or_default()
}

fn env_api_key(provider: &Provider) -> Option<ApiKey> {
    match provider {
        Provider::Anthropic => crate::env::anthropic_api_key(),
        _ => crate::env::env_api_key(provider),
    }
}

#[must_use]
pub fn missing_key_error(
    provider: &Provider,
    account: Option<&AccountLabel>,
) -> crate::registry::ProviderError {
    let failure = oauth_provider_id(provider).and_then(|oauth_id| {
        Some((
            oauth_id,
            crate::oauth::last_refresh_failure(oauth_id, account)?,
        ))
    });
    match failure {
        Some((oauth_id, failure)) => crate::registry::ProviderError::LoginExpired {
            provider: oauth_id.to_string(),
            account: failure.account,
            reason: failure.reason,
        },
        None => crate::registry::ProviderError::MissingApiKey(provider.clone()),
    }
}

#[must_use]
pub fn oauth_provider_id(provider: &Provider) -> Option<&'static str> {
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

fn config_path() -> PathBuf {
    crate::dirs::config_dir().join("config.toml")
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    api_keys: HashMap<String, ApiKey>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::credentials::FileCredentialStore;

    #[test]
    fn credentials_count_from_config_or_the_store_but_not_from_another_provider() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::at(dir.path().join("api-keys.json"));
        let provider = Provider::Custom("mush-credential-probe".into());
        let other = Provider::Custom("mush-credential-other".into());
        let mut config_api_keys = HashMap::new();

        assert!(!has_credentials(&provider, &config_api_keys, &store));

        config_api_keys.insert(
            "mush-credential-probe".into(),
            ApiKey::new("sk-probe-config").unwrap(),
        );
        assert!(has_credentials(&provider, &config_api_keys, &store));
        assert!(!has_credentials(&other, &config_api_keys, &store));

        store
            .set("mush-credential-other", "sk-other-stored")
            .unwrap();
        assert!(has_credentials(&other, &HashMap::new(), &store));
    }

    #[tokio::test]
    async fn resolve_auth_uses_config_key_for_openai() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::at(dir.path().join("api-keys.json"));
        let key = ApiKey::new("sk-openai-config-test").unwrap();
        let mut config_api_keys = HashMap::new();
        config_api_keys.insert("openai".into(), key.clone());

        let resolved = resolve_auth(
            &Provider::Custom("openai".into()),
            &config_api_keys,
            &store,
            None,
        )
        .await;

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

        let resolved = resolve_auth(
            &Provider::Custom("openai".into()),
            &HashMap::new(),
            &store,
            None,
        )
        .await;

        assert_eq!(
            resolved,
            ResolvedAuth {
                api_key: Some(ApiKey::new("sk-openai-stored-test").unwrap()),
                account_id: None,
            }
        );
    }

    #[tokio::test]
    async fn a_named_account_skips_api_keys_and_misses_without_a_login() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::at(dir.path().join("api-keys.json"));
        let mut config_api_keys = HashMap::new();
        config_api_keys.insert("anthropic".into(), ApiKey::new("sk-ant-config").unwrap());

        let with_key = resolve_auth(&Provider::Anthropic, &config_api_keys, &store, None).await;
        assert_eq!(with_key.api_key, ApiKey::new("sk-ant-config"));

        let named = AccountLabel::parse("nobody-logged-in-as-this").unwrap();
        let resolved =
            resolve_auth(&Provider::Anthropic, &config_api_keys, &store, Some(&named)).await;
        assert_eq!(resolved, ResolvedAuth::default());
    }
}
