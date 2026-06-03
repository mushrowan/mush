//! login provider catalogue and source detection
//!
//! the `/login` picker (and the `mush login` CLI) need a single place
//! to enumerate every provider mush can authenticate against and where
//! a given provider's credentials come from at runtime. the picker
//! uses this to render rows + badges, and resolution code uses it to
//! find the active key.
//!
//! the catalogue is intentionally hand-curated. discovery / dynamic
//! providers are out of scope here

use std::collections::HashMap;

use crate::credentials::CredentialStore;
use crate::types::ApiKey;

/// how a provider authenticates
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginMethod {
    /// browser-based oauth flow, completed via `/login-complete`. the
    /// `oauth_provider_id` matches an entry in [`crate::oauth::list_providers`]
    OAuth { oauth_provider_id: String },
    /// user pastes an api key. the key is stored in the credential
    /// store under `storage_key`. `env_var` and `config_key` describe
    /// where the same key may be picked up from the env / config.toml
    /// instead of the credential store, so source detection can label
    /// the row correctly
    ApiKey {
        storage_key: String,
        env_var: String,
        config_key: String,
    },
}

/// one row in the login catalogue
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginProvider {
    /// stable identifier, unique across rows. used as the picker's row
    /// id and as the argument to `/login <id>`
    pub id: String,
    /// human-readable name shown in the picker (e.g. "OpenRouter")
    pub name: String,
    pub method: LoginMethod,
}

/// where a logged-in provider's credential is currently coming from.
/// drives the picker's `[oauth] / [env] / [config] / [stored]` badge
/// and the logout policy (only `OAuth` and `Stored` rows can be logged
/// out via the picker; env / config sources are left to the user)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginSource {
    /// saved oauth credentials (`oauth.json`)
    OAuth,
    /// saved api key in the credential store. the `&'static str` is
    /// the backend label (e.g. `keyring`, `file`) so the picker can
    /// surface where the secret landed
    Stored(&'static str),
    /// api key resolved from a process environment variable. the
    /// `String` carries the var name so the toast can say which one
    Env(String),
    /// api key declared in `config.toml`'s `[api_keys]` section
    Config,
}

impl LoginSource {
    /// short label for the picker badge
    #[must_use]
    pub fn badge(&self) -> String {
        match self {
            Self::OAuth => "[oauth]".into(),
            Self::Stored(backend) => format!("[stored: {backend}]"),
            Self::Env(_) => "[env]".into(),
            Self::Config => "[config]".into(),
        }
    }

    /// whether the picker should let the user log out of this row.
    /// env and config sources are user-managed, so the picker explains
    /// where to look instead of silently editing those files
    #[must_use]
    pub fn is_picker_managed(&self) -> bool {
        matches!(self, Self::OAuth | Self::Stored(_))
    }
}

/// canonical login catalogue. keep this list in sync with the auth
/// resolution path in the cli + tui (env > config > stored > oauth).
/// adding a row here surfaces it in the picker and the `mush login`
/// command without further plumbing
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
    ]
}

/// look up a single provider by id, returning `None` for unknown ids
#[must_use]
pub fn get_provider(id: &str) -> Option<LoginProvider> {
    list_providers().into_iter().find(|p| p.id == id)
}

/// inputs needed to figure out where a credential currently lives.
/// callers pass references rather than owning copies because the
/// config map and credential store are owned by the caller
pub struct SourceInputs<'a> {
    /// `(provider_id, account_id)` pairs from `oauth::load_credentials`
    pub oauth_present: &'a [(String, Option<String>)],
    /// `[api_keys]` from the user's config.toml, keyed by config_key
    pub config_keys: &'a HashMap<String, ApiKey>,
    /// active credential store (file or keyring)
    pub credential_store: &'a dyn CredentialStore,
}

/// determine the active source for `provider`, mirroring the auth
/// resolution order used by the agent at request time. precedence:
/// env > config > stored > oauth
///
/// returns `None` when no credential is available from any source
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

/// optional account id captured at oauth login time for a given
/// provider. mirrors the lookup in `oauth_account_id` callers and is
/// here so the picker shows the same badge as elsewhere
#[must_use]
pub fn oauth_account_id(oauth_provider_id: &str) -> Option<String> {
    crate::oauth::load_credentials().ok().and_then(|store| {
        store
            .providers
            .get(oauth_provider_id)
            .and_then(|c| c.account_id.clone())
    })
}

/// metadata learned by validating an api key against its provider.
/// most fields are populated only by openrouter (which has a
/// purpose-built `/auth/key` endpoint); anthropic / openai don't
/// expose anything similar so a successful validation just yields
/// the default empty record
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeyInfo {
    /// human-readable key label (openrouter's `data.label`)
    pub label: Option<String>,
    /// dollars already spent on this key
    pub usage: Option<f64>,
    /// dollar spend limit, if one was set
    pub limit: Option<f64>,
    /// whether the key is on openrouter's free tier
    pub free_tier: Option<bool>,
}

/// outcome of an api-key validation request. distinct from a generic
/// http error so the picker can route a clear "key was rejected"
/// message vs a transient network failure
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    /// the upstream returned 401/403 - the key itself is the problem
    #[error("{provider} rejected the api key as invalid")]
    Unauthorized { provider: String },
    /// the request never completed (dns, tls, connection refused, ...)
    #[error("could not reach {provider} to validate: {reason}")]
    Network { provider: String, reason: String },
    /// upstream returned a non-success status that wasn't 401/403.
    /// includes a short body excerpt for diagnostics
    #[error("unexpected response from {provider} ({status}): {body}")]
    UnexpectedStatus {
        provider: String,
        status: u16,
        body: String,
    },
    /// the response body wasn't shaped the way we expected
    #[error("could not parse {0} response")]
    Parse(String),
    /// no validation strategy is registered for this provider
    #[error("validation is not implemented for provider {0}")]
    Unsupported(String),
}

/// parse openrouter's `GET /api/v1/auth/key` response into [`KeyInfo`].
///
/// the upstream wraps everything in a `data` object. fields are
/// optional in practice (free-tier keys often omit `limit`), so we
/// pass them through as `Option`s and treat empty strings as absent
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

/// validate an api key against its upstream and return whatever
/// metadata the provider exposes. routes by `provider.id`:
///
/// - `openrouter`  → `GET /api/v1/auth/key` (returns label / usage / limit)
/// - `openai-api`  → `GET /v1/models` (200 = ok)
/// - `anthropic-api` → `GET /v1/models` with `x-api-key` (200 = ok)
///
/// any other id returns [`ValidationError::Unsupported`]. callers
/// generally want to gate `store.set` on a successful validation
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
        .map_err(|e| ValidationError::Network {
            provider: "openrouter".into(),
            reason: e.to_string(),
        })?;
    let status = resp.status();
    if status.is_success() {
        let body = resp.text().await.map_err(|e| ValidationError::Network {
            provider: "openrouter".into(),
            reason: e.to_string(),
        })?;
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
        .map_err(|e| ValidationError::Network {
            provider: "openai".into(),
            reason: e.to_string(),
        })?;
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
        .map_err(|e| ValidationError::Network {
            provider: "anthropic".into(),
            reason: e.to_string(),
        })?;
    if resp.status().is_success() {
        return Ok(KeyInfo::default());
    }
    Err(map_status("anthropic", resp).await)
}

/// turn a non-success response into a [`ValidationError`]. 401/403 are
/// treated as "key rejected"; any other status carries the body
/// excerpt back to the caller for display
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
        // the picker needs this catalogue to be deterministic and
        // exhaustive. drop or rename here means a deliberate version
        // bump in the picker UI, so the test guards against accidents
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
        // wording matters here: "Pro/Max" reflects what the upstream
        // plan is actually called and is what the oauth catalogue
        // already shows in its toast text
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
        // env vars must not be set for the test; isolate by using a
        // unique variable name in the catalogue or temporarily ensure
        // the var is missing. OPENROUTER_API_KEY may be set on the
        // host so we skip the assertion when that's the case
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
        // env may be set on the host; skip assertion in that case
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
        // env / config rows must not be loggable-out via the picker
        // because mush doesn't own those files
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
        // openrouter's /auth/key response carries useful surface that
        // we want to show in the picker toast (label, remaining credit).
        // exercise the happy path so future api shape changes break
        // loudly here instead of silently dropping fields
        let info = parse_openrouter_auth_key(OPENROUTER_FULL_RESPONSE).unwrap();
        assert_eq!(info.label.as_deref(), Some("mush-dev"));
        assert!((info.usage.unwrap() - 1.234).abs() < 1e-9);
        assert!((info.limit.unwrap() - 10.0).abs() < 1e-9);
        assert_eq!(info.free_tier, Some(false));
    }

    #[test]
    fn parse_openrouter_auth_key_treats_empty_label_as_absent() {
        // a key without a label comes back as an empty string. that's
        // not informative for the picker, so drop it to None so the
        // toast doesn't show "key '' validated"
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
        // the picker shows this string verbatim, so it has to be
        // self-explanatory when it lands in the toast
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
