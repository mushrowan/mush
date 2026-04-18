//! oauth credential management
//!
//! trait-based oauth provider registry. each provider implements the login
//! flow (PKCE, device code, etc) and token refresh. credentials are
//! persisted to ~/.config/mush/oauth.json.

pub mod anthropic;
pub mod openai_codex;
mod pkce;
mod store;
pub mod usage;

pub use pkce::{PkceChallenge, generate_pkce};
pub use store::{CredentialStore, OAuthCredentials, load_credentials, save_credentials};

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum OAuthError {
    #[error("oauth token exchange failed: {0}")]
    TokenExchange(String),

    #[error("oauth token refresh failed: {0}")]
    TokenRefresh(String),

    #[error("unknown oauth provider: {0}")]
    #[diagnostic(help("available providers: {}", available_provider_names().join(", ")))]
    UnknownProvider(String),

    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("failed to read/write credentials: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse credentials: {0}")]
    Json(#[from] serde_json::Error),
}

/// info needed to complete the oauth login
pub struct AuthPrompt {
    /// URL the user should open in their browser
    pub url: String,
    /// instructions to show the user
    pub instructions: String,
}

/// trait for oauth providers
pub trait OAuthProvider: Send + Sync {
    /// unique id (e.g. "anthropic")
    fn id(&self) -> &str;

    /// human-readable name
    fn name(&self) -> &str;

    /// start the login flow, returning the URL and instructions
    fn begin_login(&self) -> Result<(AuthPrompt, PkceChallenge), OAuthError>;

    /// exchange the user-provided code for credentials
    fn exchange_code(
        &self,
        code: &str,
        pkce: &PkceChallenge,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OAuthCredentials, OAuthError>> + Send + '_>,
    >;

    /// refresh expired credentials
    fn refresh_token(
        &self,
        refresh_token: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OAuthCredentials, OAuthError>> + Send + '_>,
    >;

    /// extract the api key from credentials
    fn api_key(&self, creds: &OAuthCredentials) -> String {
        creds.access_token.clone()
    }
}

// -- built-in provider registry --

fn builtin_providers() -> Vec<Box<dyn OAuthProvider>> {
    vec![
        Box::new(anthropic::AnthropicOAuth),
        Box::new(openai_codex::OpenaiCodexOAuth),
    ]
}

/// get a provider by id
pub fn get_provider(id: &str) -> Option<Box<dyn OAuthProvider>> {
    builtin_providers().into_iter().find(|p| p.id() == id)
}

/// list all available provider ids and names
pub fn list_providers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("anthropic", "Anthropic (Claude Pro/Max)"),
        ("openai-codex", "ChatGPT Plus/Pro (Codex subscription)"),
    ]
}

fn available_provider_names() -> Vec<String> {
    list_providers()
        .iter()
        .map(|(id, _)| id.to_string())
        .collect()
}

/// get a valid oauth token for a provider, refreshing and saving if needed
pub async fn get_oauth_token(provider_id: &str) -> Result<Option<String>, OAuthError> {
    let provider = match get_provider(provider_id) {
        Some(p) => p,
        None => return Ok(None),
    };

    let mut store = load_credentials()?;
    let Some(creds) = store.providers.get(provider_id) else {
        return Ok(None);
    };

    let updated = if creds.is_expired() {
        let mut refreshed = provider.refresh_token(&creds.refresh_token).await?;
        if refreshed.account_id.is_none() {
            refreshed.account_id = creds.account_id.clone();
        }
        refreshed
    } else {
        creds.clone()
    };

    let token = provider.api_key(&updated);
    store.providers.insert(provider_id.into(), updated);
    save_credentials(&store)?;

    Ok(Some(token))
}

/// unconditionally refresh a provider's oauth token, bypassing the
/// `is_expired` check. used as a safety net when a request comes back
/// with 401 Unauthorized despite the cached expiry claiming the token
/// is still valid (clock skew, server-side revocation, stale timestamp)
pub async fn force_refresh_oauth_token(provider_id: &str) -> Result<Option<String>, OAuthError> {
    let provider = match get_provider(provider_id) {
        Some(p) => p,
        None => return Ok(None),
    };

    let mut store = load_credentials()?;
    let Some(creds) = store.providers.get(provider_id) else {
        return Ok(None);
    };

    let mut refreshed = provider.refresh_token(&creds.refresh_token).await?;
    if refreshed.account_id.is_none() {
        refreshed.account_id = creds.account_id.clone();
    }

    let token = provider.api_key(&refreshed);
    store.providers.insert(provider_id.into(), refreshed);
    save_credentials(&store)?;

    Ok(Some(token))
}

/// convenience: get anthropic oauth token
pub async fn get_anthropic_oauth_token() -> Result<Option<String>, OAuthError> {
    get_oauth_token("anthropic").await
}

/// convenience: force-refresh anthropic oauth token (see [`force_refresh_oauth_token`])
pub async fn force_refresh_anthropic_oauth_token() -> Result<Option<String>, OAuthError> {
    force_refresh_oauth_token("anthropic").await
}

/// convenience: get openai codex oauth token
pub async fn get_openai_codex_oauth_token() -> Result<Option<String>, OAuthError> {
    get_oauth_token("openai-codex").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_providers_includes_openai_codex() {
        let providers = list_providers();
        assert!(providers.iter().any(|(id, _)| *id == "openai-codex"));
    }

    #[test]
    fn get_provider_resolves_openai_codex() {
        let provider = get_provider("openai-codex");
        assert!(provider.is_some());
    }
}
