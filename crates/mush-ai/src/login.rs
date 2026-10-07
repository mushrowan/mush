use std::collections::HashMap;

use crate::credentials::CredentialStore;
use crate::types::ApiKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginMethod {
    OAuth {
        oauth_provider_id: String,
    },
    ApiKey {
        storage_key: String,
        env_var: String,
        config_key: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginProvider {
    pub id: String,
    pub name: String,
    pub method: LoginMethod,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginSource {
    OAuth,
    Stored(&'static str),
    Env(String),
    Config,
}

impl LoginSource {
    #[must_use]
    pub fn badge(&self) -> String {
        match self {
            Self::OAuth => "[oauth]".into(),
            Self::Stored(backend) => format!("[stored: {backend}]"),
            Self::Env(_) => "[env]".into(),
            Self::Config => "[config]".into(),
        }
    }

    #[must_use]
    pub fn is_picker_managed(&self) -> bool {
        matches!(self, Self::OAuth | Self::Stored(_))
    }
}

#[must_use]
pub fn list_providers() -> Vec<LoginProvider> {
    vec![
        LoginProvider {
            id: "anthropic-pro-max".into(),
            name: "Anthropic (Claude Pro/Max)".into(),
            method: LoginMethod::OAuth {
                oauth_provider_id: "anthropic".into(),
            },
        },
        LoginProvider {
            id: "anthropic-api".into(),
            name: "Anthropic API".into(),
            method: LoginMethod::ApiKey {
                storage_key: "anthropic".into(),
                env_var: "ANTHROPIC_API_KEY".into(),
                config_key: "anthropic".into(),
            },
        },
        LoginProvider {
            id: "openai-codex".into(),
            name: "ChatGPT Plus/Pro (Codex)".into(),
            method: LoginMethod::OAuth {
                oauth_provider_id: "openai-codex".into(),
            },
        },
        LoginProvider {
            id: "openai-api".into(),
            name: "OpenAI API".into(),
            method: LoginMethod::ApiKey {
                storage_key: "openai".into(),
                env_var: "OPENAI_API_KEY".into(),
                config_key: "openai".into(),
            },
        },
        LoginProvider {
            id: "openrouter-web".into(),
            name: "OpenRouter (web sign-in)".into(),
            method: LoginMethod::OAuth {
                oauth_provider_id: "openrouter".into(),
            },
        },
        LoginProvider {
            id: "openrouter".into(),
            name: "OpenRouter".into(),
            method: LoginMethod::ApiKey {
                storage_key: "openrouter".into(),
                env_var: "OPENROUTER_API_KEY".into(),
                config_key: "openrouter".into(),
            },
        },
        LoginProvider {
            id: "zai".into(),
            name: "Z.AI (GLM Coding Plan)".into(),
            method: LoginMethod::ApiKey {
                storage_key: "zai".into(),
                env_var: "ZAI_API_KEY".into(),
                config_key: "zai".into(),
            },
        },
    ]
}

#[must_use]
pub fn get_provider(id: &str) -> Option<LoginProvider> {
    list_providers().into_iter().find(|p| p.id == id)
}

pub struct SourceInputs<'a> {
    pub oauth_present: &'a [(String, Option<String>)],
    pub config_keys: &'a HashMap<String, ApiKey>,
    pub credential_store: &'a dyn CredentialStore,
}

#[must_use]
pub fn resolve_source(provider: &LoginProvider, inputs: &SourceInputs<'_>) -> Option<LoginSource> {
    match &provider.method {
        LoginMethod::OAuth { oauth_provider_id } => inputs
            .oauth_present
            .iter()
            .any(|(id, _)| id == oauth_provider_id)
            .then_some(LoginSource::OAuth),
        LoginMethod::ApiKey {
            storage_key,
            env_var,
            config_key,
        } => {
            if std::env::var(env_var).is_ok() {
                return Some(LoginSource::Env(env_var.clone()));
            }
            if inputs.config_keys.contains_key(config_key) {
                return Some(LoginSource::Config);
            }
            match inputs.credential_store.get(storage_key) {
                Ok(Some(_)) => Some(LoginSource::Stored(inputs.credential_store.backend())),
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(?e, %storage_key, "credential store lookup failed");
                    None
                }
            }
        }
    }
}

#[must_use]
pub fn oauth_logins() -> Vec<(String, Option<String>)> {
    let store = crate::oauth::load_credentials().unwrap_or_default();
    store
        .providers
        .iter()
        .filter_map(|(id, accounts)| {
            let (_, creds) = accounts.get(None)?;
            Some((id.clone(), creds.account_id.clone()))
        })
        .collect()
}

#[must_use]
pub fn oauth_account_id(
    oauth_provider_id: &str,
    account: Option<&crate::types::AccountLabel>,
) -> Option<String> {
    crate::oauth::load_credentials().ok().and_then(|store| {
        store
            .get(oauth_provider_id, account)
            .and_then(|(_, creds)| creds.account_id.clone())
    })
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeyInfo {
    pub label: Option<String>,
    pub usage: Option<f64>,
    pub limit: Option<f64>,
    pub free_tier: Option<bool>,
}

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("{provider} rejected the api key as invalid")]
    Unauthorized { provider: String },
    #[error("could not reach {provider} to validate: {reason}")]
    Network { provider: String, reason: String },
    #[error("unexpected response from {provider} ({status}): {body}")]
    UnexpectedStatus {
        provider: String,
        status: u16,
        body: String,
    },
    #[error("could not parse {0} response")]
    Parse(String),
    #[error("validation is not implemented for provider {0}")]
    Unsupported(String),
}

pub fn parse_openrouter_auth_key(body: &str) -> Result<KeyInfo, ValidationError> {
    #[derive(serde::Deserialize)]
    struct Wrapper {
        data: Inner,
    }
    #[derive(serde::Deserialize)]
    struct Inner {
        label: Option<String>,
        usage: Option<f64>,
        limit: Option<f64>,
        is_free_tier: Option<bool>,
    }
    let parsed: Wrapper = serde_json::from_str(body)
        .map_err(|e| ValidationError::Parse(format!("openrouter: {e}")))?;
    Ok(KeyInfo {
        label: parsed.data.label.filter(|s| !s.is_empty()),
        usage: parsed.data.usage,
        limit: parsed.data.limit,
        free_tier: parsed.data.is_free_tier,
    })
}

pub async fn validate_api_key(
    client: &crate::http::HttpClient,
    provider: &LoginProvider,
    key: &str,
) -> Result<KeyInfo, ValidationError> {
    match provider.id.as_str() {
        "openrouter" => validate_openrouter(client, key, "https://openrouter.ai").await,
        "openai-api" => validate_openai(client, key, "https://api.openai.com").await,
        "anthropic-api" => validate_anthropic(client, key, "https://api.anthropic.com").await,
        other => Err(ValidationError::Unsupported(other.into())),
    }
}

async fn validate_openrouter(
    client: &crate::http::HttpClient,
    key: &str,
    base_url: &str,
) -> Result<KeyInfo, ValidationError> {
    let url = format!("{base_url}/api/v1/auth/key");
    let resp = client
        .get(&url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(network_error("openrouter"))?;
    let status = resp.status();
    if status.is_success() {
        let body = resp.text().await.map_err(network_error("openrouter"))?;
        return parse_openrouter_auth_key(&body);
    }
    Err(map_status("openrouter", resp).await)
}

async fn validate_openai(
    client: &crate::http::HttpClient,
    key: &str,
    base_url: &str,
) -> Result<KeyInfo, ValidationError> {
    let url = format!("{base_url}/v1/models");
    let resp = client
        .get(&url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(network_error("openai"))?;
    if resp.status().is_success() {
        return Ok(KeyInfo::default());
    }
    Err(map_status("openai", resp).await)
}

async fn validate_anthropic(
    client: &crate::http::HttpClient,
    key: &str,
    base_url: &str,
) -> Result<KeyInfo, ValidationError> {
    let url = format!("{base_url}/v1/models");
    let resp = client
        .get(&url)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .map_err(network_error("anthropic"))?;
    if resp.status().is_success() {
        return Ok(KeyInfo::default());
    }
    Err(map_status("anthropic", resp).await)
}

fn network_error<E: std::fmt::Display>(provider: &str) -> impl FnOnce(E) -> ValidationError {
    move |error| ValidationError::Network {
        provider: provider.into(),
        reason: error.to_string(),
    }
}

async fn map_status(provider: &str, resp: reqwest::Response) -> ValidationError {
    let status = resp.status();
    if matches!(status.as_u16(), 401 | 403) {
        return ValidationError::Unauthorized {
            provider: provider.into(),
        };
    }
    let body = resp.text().await.unwrap_or_default();
    let truncated = body.chars().take(200).collect::<String>();
    ValidationError::UnexpectedStatus {
        provider: provider.into(),
        status: status.as_u16(),
        body: truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::FileCredentialStore;

    fn temp_store() -> (tempfile::TempDir, FileCredentialStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api-keys.json");
        (dir, FileCredentialStore::at(path))
    }

    fn provider(id: &str) -> LoginProvider {
        get_provider(id).unwrap()
    }

    #[test]
    fn catalogue_lists_oauth_and_api_key_providers_by_stable_id() {
        let ids: Vec<String> = list_providers().into_iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            vec![
                "anthropic-pro-max",
                "anthropic-api",
                "openai-codex",
                "openai-api",
                "openrouter-web",
                "openrouter",
                "zai",
            ]
        );
    }

    #[test]
    fn catalogue_marks_oauth_vs_api_key_methods() {
        let pro_max = provider("anthropic-pro-max");
        assert!(matches!(pro_max.method, LoginMethod::OAuth { .. }));
        let openrouter = provider("openrouter");
        assert!(matches!(openrouter.method, LoginMethod::ApiKey { .. }));
        let openrouter_web = provider("openrouter-web");
        assert!(
            matches!(openrouter_web.method, LoginMethod::OAuth { ref oauth_provider_id } if oauth_provider_id == "openrouter")
        );
    }

    #[test]
    fn anthropic_pro_max_uses_pro_max_label() {
        let p = provider("anthropic-pro-max");
        assert!(p.name.contains("Pro/Max"), "got: {}", p.name);
    }

    #[test]
    fn resolve_source_returns_none_when_nothing_present() {
        let (_dir, store) = temp_store();
        let inputs = SourceInputs {
            oauth_present: &[],
            config_keys: &HashMap::new(),
            credential_store: &store,
        };
        let p = provider("openrouter");
        if std::env::var("OPENROUTER_API_KEY").is_err() {
            assert!(resolve_source(&p, &inputs).is_none());
        }
    }

    #[test]
    fn resolve_source_picks_oauth_when_credentials_present() {
        let (_dir, store) = temp_store();
        let inputs = SourceInputs {
            oauth_present: &[("anthropic".into(), None)],
            config_keys: &HashMap::new(),
            credential_store: &store,
        };
        let p = provider("anthropic-pro-max");
        assert_eq!(resolve_source(&p, &inputs), Some(LoginSource::OAuth));
    }

    #[test]
    fn resolve_source_picks_config_when_present_for_api_key_provider() {
        let (_dir, store) = temp_store();
        let mut config = HashMap::new();
        config.insert("openrouter".into(), ApiKey::new("k").unwrap());
        let inputs = SourceInputs {
            oauth_present: &[],
            config_keys: &config,
            credential_store: &store,
        };
        let p = provider("openrouter");
        if std::env::var("OPENROUTER_API_KEY").is_err() {
            assert_eq!(resolve_source(&p, &inputs), Some(LoginSource::Config));
        }
    }

    #[test]
    fn resolve_source_picks_stored_when_only_credential_store_has_key() {
        let (_dir, store) = temp_store();
        store.set("openrouter", "stored-key").unwrap();
        let inputs = SourceInputs {
            oauth_present: &[],
            config_keys: &HashMap::new(),
            credential_store: &store,
        };
        let p = provider("openrouter");
        if std::env::var("OPENROUTER_API_KEY").is_err() {
            assert_eq!(
                resolve_source(&p, &inputs),
                Some(LoginSource::Stored("file"))
            );
        }
    }

    #[test]
    fn login_source_badge_matches_kind() {
        assert_eq!(LoginSource::OAuth.badge(), "[oauth]");
        assert_eq!(LoginSource::Stored("file").badge(), "[stored: file]");
        assert_eq!(LoginSource::Env("X".into()).badge(), "[env]");
        assert_eq!(LoginSource::Config.badge(), "[config]");
    }

    #[test]
    fn picker_only_owns_oauth_and_stored_sources() {
        assert!(LoginSource::OAuth.is_picker_managed());
        assert!(LoginSource::Stored("file").is_picker_managed());
        assert!(!LoginSource::Env("X".into()).is_picker_managed());
        assert!(!LoginSource::Config.is_picker_managed());
    }

    const OPENROUTER_FULL_RESPONSE: &str = r#"{
      "data": {
        "label": "mush-dev",
        "usage": 1.234,
        "limit": 10.0,
        "is_free_tier": false
      }
    }"#;

    #[test]
    fn parse_openrouter_auth_key_extracts_label_usage_and_limit() {
        let info = parse_openrouter_auth_key(OPENROUTER_FULL_RESPONSE).unwrap();
        assert_eq!(info.label.as_deref(), Some("mush-dev"));
        assert!((info.usage.unwrap() - 1.234).abs() < 1e-9);
        assert!((info.limit.unwrap() - 10.0).abs() < 1e-9);
        assert_eq!(info.free_tier, Some(false));
    }

    #[test]
    fn parse_openrouter_auth_key_treats_empty_label_as_absent() {
        let body = r#"{ "data": { "label": "" } }"#;
        let info = parse_openrouter_auth_key(body).unwrap();
        assert!(info.label.is_none());
        assert!(info.usage.is_none());
        assert!(info.limit.is_none());
    }

    #[test]
    fn parse_openrouter_auth_key_rejects_malformed_json() {
        let err = parse_openrouter_auth_key("not json").unwrap_err();
        assert!(matches!(err, ValidationError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn unauthorized_error_message_names_the_provider() {
        let err = ValidationError::Unauthorized {
            provider: "openrouter".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("openrouter"), "got {msg:?}");
        assert!(
            msg.contains("rejected") || msg.contains("invalid"),
            "got {msg:?}"
        );
    }
}
