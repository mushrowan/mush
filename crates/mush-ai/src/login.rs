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
}
