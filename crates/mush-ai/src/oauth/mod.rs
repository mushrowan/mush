pub mod anthropic;
pub mod callback_server;
pub mod openai_codex;
pub mod openrouter;
mod pkce;
mod store;
pub mod usage;

pub use pkce::{PkceChallenge, generate_pkce};
pub use store::{
    CredentialStore, OAuthCredentials, ProviderAccounts, load_credentials, save_credentials,
};

use crate::types::AccountLabel;

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

    #[error("http middleware failed: {0}")]
    Middleware(String),

    #[error("failed to read/write credentials: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse credentials: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<reqwest_middleware::Error> for OAuthError {
    fn from(err: reqwest_middleware::Error) -> Self {
        match err {
            reqwest_middleware::Error::Reqwest(e) => Self::Http(e),
            reqwest_middleware::Error::Middleware(e) => Self::Middleware(e.to_string()),
        }
    }
}

pub struct AuthPrompt {
    pub url: String,
    pub instructions: String,
}

pub struct BrowserLogin {
    pub auth_url: String,
    completion: std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OAuthCredentials, OAuthError>> + Send>,
    >,
}

impl BrowserLogin {
    pub fn new(
        auth_url: impl Into<String>,
        completion: impl std::future::Future<Output = Result<OAuthCredentials, OAuthError>>
        + Send
        + 'static,
    ) -> Self {
        Self {
            auth_url: auth_url.into(),
            completion: Box::pin(completion),
        }
    }

    pub async fn await_credentials(self) -> Result<OAuthCredentials, OAuthError> {
        self.completion.await
    }
}

pub trait OAuthProvider: Send + Sync {
    fn id(&self) -> &str;

    fn name(&self) -> &str;

    fn begin_login(&self) -> Result<(AuthPrompt, PkceChallenge), OAuthError>;

    fn begin_browser_login(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<BrowserLogin, OAuthError>> + Send + '_>,
    >;

    fn exchange_code(
        &self,
        code: &str,
        pkce: &PkceChallenge,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OAuthCredentials, OAuthError>> + Send + '_>,
    >;

    fn refresh_token(
        &self,
        refresh_token: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OAuthCredentials, OAuthError>> + Send + '_>,
    >;

    fn api_key(&self, creds: &OAuthCredentials) -> String {
        creds.access_token.clone()
    }
}

fn builtin_providers() -> Vec<Box<dyn OAuthProvider>> {
    vec![
        Box::new(anthropic::AnthropicOAuth),
        Box::new(openai_codex::OpenaiCodexOAuth),
        Box::new(openrouter::OpenRouterOAuth),
    ]
}

pub fn get_provider(id: &str) -> Option<Box<dyn OAuthProvider>> {
    builtin_providers().into_iter().find(|p| p.id() == id)
}

pub fn list_providers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("anthropic", "Anthropic (Claude Pro/Max)"),
        ("openai-codex", "ChatGPT Plus/Pro (Codex subscription)"),
        ("openrouter", "OpenRouter (web sign-in)"),
    ]
}

fn available_provider_names() -> Vec<String> {
    list_providers()
        .iter()
        .map(|(id, _)| id.to_string())
        .collect()
}

pub async fn get_oauth_token(
    provider_id: &str,
    account: Option<&AccountLabel>,
) -> Result<Option<String>, OAuthError> {
    let provider = match get_provider(provider_id) {
        Some(p) => p,
        None => return Ok(None),
    };

    let mut store = load_credentials()?;
    let Some((label, creds)) = store.get(provider_id, account) else {
        return Ok(None);
    };
    let label = label.clone();

    if !creds.is_expired() {
        return Ok(Some(provider.api_key(creds)));
    }

    let mut refreshed = provider.refresh_token(&creds.refresh_token).await?;
    if refreshed.account_id.is_none() {
        refreshed.account_id = creds.account_id.clone();
    }
    let token = provider.api_key(&refreshed);
    store.insert(provider_id, label, refreshed);
    save_credentials(&store)?;

    Ok(Some(token))
}

pub async fn force_refresh_oauth_token(
    provider_id: &str,
    account: Option<&AccountLabel>,
) -> Result<Option<String>, OAuthError> {
    let provider = match get_provider(provider_id) {
        Some(p) => p,
        None => return Ok(None),
    };

    let mut store = load_credentials()?;
    let Some((label, creds)) = store.get(provider_id, account) else {
        return Ok(None);
    };
    let label = label.clone();

    let mut refreshed = provider.refresh_token(&creds.refresh_token).await?;
    if refreshed.account_id.is_none() {
        refreshed.account_id = creds.account_id.clone();
    }

    let token = provider.api_key(&refreshed);
    store.insert(provider_id, label, refreshed);
    save_credentials(&store)?;

    Ok(Some(token))
}

pub fn save_login(
    provider_id: &str,
    account: Option<AccountLabel>,
    creds: OAuthCredentials,
) -> Result<AccountLabel, OAuthError> {
    let mut store = load_credentials()?;
    let label = account
        .or_else(|| {
            store
                .accounts(provider_id)
                .and_then(|accounts| accounts.default_label().cloned())
        })
        .unwrap_or_else(AccountLabel::default_label);
    store.insert(provider_id, label.clone(), creds);
    save_credentials(&store)?;
    Ok(label)
}

pub fn remove_credentials(
    provider_id: &str,
    account: Option<&AccountLabel>,
) -> Result<Option<AccountLabel>, OAuthError> {
    let mut store = load_credentials()?;
    let removed = store.remove(provider_id, account);
    if removed.is_some() {
        save_credentials(&store)?;
    }
    Ok(removed)
}

pub fn set_default_account(provider_id: &str, account: &AccountLabel) -> Result<bool, OAuthError> {
    let mut store = load_credentials()?;
    let changed = store.set_default(provider_id, account);
    if changed {
        save_credentials(&store)?;
    }
    Ok(changed)
}

pub async fn get_anthropic_oauth_token(
    account: Option<&AccountLabel>,
) -> Result<Option<String>, OAuthError> {
    get_oauth_token("anthropic", account).await
}

pub async fn force_refresh_anthropic_oauth_token(
    account: Option<&AccountLabel>,
) -> Result<Option<String>, OAuthError> {
    force_refresh_oauth_token("anthropic", account).await
}

pub async fn get_openai_codex_oauth_token(
    account: Option<&AccountLabel>,
) -> Result<Option<String>, OAuthError> {
    get_oauth_token("openai-codex", account).await
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

    #[test]
    fn list_providers_includes_openrouter() {
        let providers = list_providers();
        assert!(providers.iter().any(|(id, _)| *id == "openrouter"));
    }

    #[test]
    fn get_provider_resolves_openrouter() {
        let provider = get_provider("openrouter");
        assert!(provider.is_some(), "openrouter oauth provider missing");
        let provider = provider.unwrap();
        assert_eq!(provider.id(), "openrouter");
        assert!(provider.name().to_lowercase().contains("openrouter"));
    }

    #[tokio::test]
    async fn every_builtin_provider_offers_a_loopback_browser_login() {
        for provider in builtin_providers() {
            let login = provider
                .begin_browser_login()
                .await
                .unwrap_or_else(|e| panic!("{} browser login failed: {e}", provider.id()));
            assert!(
                login.auth_url.contains("localhost%3A"),
                "{} should redirect to a loopback callback, got {}",
                provider.id(),
                login.auth_url
            );
        }
    }
}
