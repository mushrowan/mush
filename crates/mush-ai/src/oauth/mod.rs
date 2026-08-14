pub mod anthropic;
pub mod callback_server;
pub mod openai_codex;
pub mod openrouter;
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

pub fn remove_credentials(provider_id: &str) -> Result<bool, OAuthError> {
    let mut store = load_credentials()?;
    let removed = store.providers.remove(provider_id).is_some();
    if removed {
        save_credentials(&store)?;
    }
    Ok(removed)
}

pub async fn get_anthropic_oauth_token() -> Result<Option<String>, OAuthError> {
    get_oauth_token("anthropic").await
}

pub async fn force_refresh_anthropic_oauth_token() -> Result<Option<String>, OAuthError> {
    force_refresh_oauth_token("anthropic").await
}

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
