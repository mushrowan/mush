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
use store::{lock_store, lock_store_async};

use crate::types::AccountLabel;

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum OAuthError {
    #[error("oauth token exchange failed: {0}")]
    TokenExchange(String),

    #[error("oauth token refresh failed: {0}")]
    TokenRefresh(String),

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

pub type OAuthFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, OAuthError>> + Send + 'a>>;

pub struct AuthPrompt {
    pub url: String,
    pub instructions: String,
}

pub struct BrowserLogin {
    pub auth_url: String,
    completion: OAuthFuture<'static, OAuthCredentials>,
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

    fn begin_browser_login(&self) -> OAuthFuture<'_, BrowserLogin>;

    fn exchange_code(&self, code: &str, pkce: &PkceChallenge) -> OAuthFuture<'_, OAuthCredentials>;

    fn refresh_token(&self, refresh_token: &str) -> OAuthFuture<'_, OAuthCredentials>;

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

pub async fn get_oauth_token(
    provider_id: &str,
    account: Option<&AccountLabel>,
) -> Result<Option<String>, OAuthError> {
    let Some(provider) = get_provider(provider_id) else {
        return Ok(None);
    };
    let store = load_credentials()?;
    let Some((_, creds)) = store.get(provider_id, account) else {
        return Ok(None);
    };
    if !creds.is_expired() {
        return Ok(Some(provider.api_key(creds)));
    }
    refresh_stored(provider.as_ref(), account, Stale::Expired).await
}

pub async fn force_refresh_oauth_token(
    provider_id: &str,
    account: Option<&AccountLabel>,
    rejected_token: &str,
) -> Result<Option<String>, OAuthError> {
    let Some(provider) = get_provider(provider_id) else {
        return Ok(None);
    };
    refresh_stored(provider.as_ref(), account, Stale::Rejected(rejected_token)).await
}

enum Stale<'a> {
    Expired,
    Rejected(&'a str),
}

const REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn refresh_flight(provider_id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static FLIGHTS: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::LazyLock::new(Default::default);
    FLIGHTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(provider_id.to_string())
        .or_default()
        .clone()
}

async fn refresh_stored(
    provider: &dyn OAuthProvider,
    account: Option<&AccountLabel>,
    stale: Stale<'_>,
) -> Result<Option<String>, OAuthError> {
    let provider_id = provider.id();
    let flight = refresh_flight(provider_id);
    let _flight = flight.lock().await;
    let _lock = lock_store_async().await?;

    let mut store = load_credentials()?;
    let Some((label, creds)) = store.get(provider_id, account) else {
        return Ok(None);
    };
    let still_stale = match stale {
        Stale::Expired => creds.is_expired(),
        Stale::Rejected(token) => provider.api_key(creds) == token,
    };
    if !still_stale {
        return Ok(Some(provider.api_key(creds)));
    }
    let label = label.clone();
    let creds = creds.clone();

    let refreshed = tokio::time::timeout(
        REFRESH_TIMEOUT,
        provider.refresh_token(&creds.refresh_token),
    )
    .await
    .unwrap_or_else(|_| {
        Err(OAuthError::TokenRefresh(
            "timed out waiting for the token endpoint".into(),
        ))
    });
    let mut refreshed = match refreshed {
        Ok(refreshed) => {
            record_refresh_failure(provider_id, &label, None);
            refreshed
        }
        Err(error) => {
            record_refresh_failure(provider_id, &label, Some(refresh_failure_reason(&error)));
            return Err(error);
        }
    };
    if refreshed.account_id.is_none() {
        refreshed.account_id = creds.account_id;
    }
    let token = provider.api_key(&refreshed);
    store.insert(provider_id, label, refreshed);
    save_credentials(&store)?;
    Ok(Some(token))
}

type FailureKey = (String, AccountLabel);

fn refresh_failures() -> &'static std::sync::Mutex<std::collections::HashMap<FailureKey, String>> {
    static FAILURES: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<FailureKey, String>>,
    > = std::sync::LazyLock::new(Default::default);
    &FAILURES
}

fn record_refresh_failure(provider_id: &str, label: &AccountLabel, reason: Option<String>) {
    let mut failures = refresh_failures()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (provider_id.to_string(), label.clone());
    match reason {
        Some(reason) => {
            failures.insert(key, reason);
        }
        None => {
            failures.remove(&key);
        }
    }
}

fn refresh_failure_reason(error: &OAuthError) -> String {
    let OAuthError::TokenRefresh(body) = error else {
        return error.to_string();
    };
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error_description")
                .or_else(|| value.get("error"))
                .and_then(|reason| reason.as_str())
                .map(str::to_lowercase)
        })
        .unwrap_or_else(|| body.clone())
}

pub struct RefreshFailure {
    pub account: AccountLabel,
    pub reason: String,
}

pub fn last_refresh_failure(
    provider_id: &str,
    account: Option<&AccountLabel>,
) -> Option<RefreshFailure> {
    let store = load_credentials().ok()?;
    let (label, creds) = store.get(provider_id, account)?;
    if !creds.is_expired() {
        return None;
    }
    recorded_refresh_failure(provider_id, label)
}

pub fn refresh_failure(
    provider_id: &str,
    account: Option<&AccountLabel>,
) -> Option<RefreshFailure> {
    let store = load_credentials().ok()?;
    let (label, _) = store.get(provider_id, account)?;
    recorded_refresh_failure(provider_id, label)
}

fn recorded_refresh_failure(provider_id: &str, label: &AccountLabel) -> Option<RefreshFailure> {
    let reason = refresh_failures()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(provider_id.to_string(), label.clone()))?
        .clone();
    Some(RefreshFailure {
        account: label.clone(),
        reason,
    })
}

pub fn save_login(
    provider_id: &str,
    account: Option<AccountLabel>,
    creds: OAuthCredentials,
) -> Result<AccountLabel, OAuthError> {
    let _lock = lock_store()?;
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
    let _lock = lock_store()?;
    let mut store = load_credentials()?;
    let removed = store.remove(provider_id, account);
    if removed.is_some() {
        save_credentials(&store)?;
    }
    Ok(removed)
}

pub fn set_default_account(provider_id: &str, account: &AccountLabel) -> Result<bool, OAuthError> {
    let _lock = lock_store()?;
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

    struct CountingProvider {
        refreshes: std::sync::atomic::AtomicUsize,
    }

    impl OAuthProvider for CountingProvider {
        fn id(&self) -> &str {
            "counting"
        }

        fn name(&self) -> &str {
            "counting"
        }

        fn begin_login(&self) -> Result<(AuthPrompt, PkceChallenge), OAuthError> {
            unreachable!()
        }

        fn begin_browser_login(&self) -> OAuthFuture<'_, BrowserLogin> {
            unreachable!()
        }

        fn exchange_code(
            &self,
            _code: &str,
            _pkce: &PkceChallenge,
        ) -> OAuthFuture<'_, OAuthCredentials> {
            unreachable!()
        }

        fn refresh_token(&self, refresh_token: &str) -> OAuthFuture<'_, OAuthCredentials> {
            let generation = self
                .refreshes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            let spent = refresh_token.to_string();
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                Ok(OAuthCredentials {
                    access_token: format!("access-{generation}"),
                    refresh_token: format!("{spent}+"),
                    expires_at: u64::MAX,
                    account_id: None,
                })
            })
        }
    }

    static CONFIG_DIR_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct ConfigDir {
        _dir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved: Option<std::ffi::OsString>,
    }

    impl Drop for ConfigDir {
        fn drop(&mut self) {
            unsafe {
                match self.saved.take() {
                    Some(value) => std::env::set_var("MUSH_CONFIG_DIR", value),
                    None => std::env::remove_var("MUSH_CONFIG_DIR"),
                }
            }
        }
    }

    async fn config_with_expired_login() -> ConfigDir {
        config_with_login(0).await
    }

    async fn config_with_login(expires_at: u64) -> ConfigDir {
        let lock = CONFIG_DIR_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let saved = std::env::var_os("MUSH_CONFIG_DIR");
        unsafe { std::env::set_var("MUSH_CONFIG_DIR", dir.path()) };
        save_login(
            "counting",
            None,
            OAuthCredentials {
                access_token: "access-0".into(),
                refresh_token: "refresh".into(),
                expires_at,
                account_id: Some("acct".into()),
            },
        )
        .unwrap();
        ConfigDir {
            _dir: dir,
            _lock: lock,
            saved,
        }
    }

    fn counting() -> CountingProvider {
        CountingProvider {
            refreshes: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    #[tokio::test]
    async fn concurrent_lookups_spend_the_refresh_token_once() {
        let _config = config_with_expired_login().await;
        let provider = counting();

        let (a, b, c) = tokio::join!(
            refresh_stored(&provider, None, Stale::Expired),
            refresh_stored(&provider, None, Stale::Expired),
            refresh_stored(&provider, None, Stale::Expired),
        );

        assert_eq!(
            provider.refreshes.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        for token in [a, b, c] {
            assert_eq!(token.unwrap().as_deref(), Some("access-1"));
        }
        let store = load_credentials().unwrap();
        let (_, creds) = store.get("counting", None).unwrap();
        assert_eq!(creds.refresh_token, "refresh+");
        assert_eq!(creds.account_id.as_deref(), Some("acct"));
    }

    #[tokio::test]
    async fn a_rejected_token_a_peer_already_replaced_is_not_refreshed_again() {
        let _config = config_with_expired_login().await;
        let provider = counting();

        let first = refresh_stored(&provider, None, Stale::Rejected("access-0"))
            .await
            .unwrap();
        let second = refresh_stored(&provider, None, Stale::Rejected("access-0"))
            .await
            .unwrap();

        assert_eq!(first.as_deref(), Some("access-1"));
        assert_eq!(second.as_deref(), Some("access-1"));
        assert_eq!(
            provider.refreshes.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    struct RejectingProvider;

    impl OAuthProvider for RejectingProvider {
        fn id(&self) -> &str {
            "counting"
        }

        fn name(&self) -> &str {
            "rejecting"
        }

        fn begin_login(&self) -> Result<(AuthPrompt, PkceChallenge), OAuthError> {
            unreachable!()
        }

        fn begin_browser_login(&self) -> OAuthFuture<'_, BrowserLogin> {
            unreachable!()
        }

        fn exchange_code(
            &self,
            _code: &str,
            _pkce: &PkceChallenge,
        ) -> OAuthFuture<'_, OAuthCredentials> {
            unreachable!()
        }

        fn refresh_token(&self, _refresh_token: &str) -> OAuthFuture<'_, OAuthCredentials> {
            Box::pin(async {
                Err(OAuthError::TokenRefresh(
                    r#"{"error": "invalid_grant", "error_description": "Refresh token expired"}"#
                        .into(),
                ))
            })
        }
    }

    #[tokio::test]
    async fn a_failed_refresh_is_remembered_until_one_succeeds() {
        let _config = config_with_expired_login().await;

        assert!(
            refresh_stored(&RejectingProvider, None, Stale::Expired)
                .await
                .is_err()
        );
        let failure = last_refresh_failure("counting", None).expect("failure recorded");
        assert_eq!(failure.account, AccountLabel::default_label());
        assert_eq!(failure.reason, "refresh token expired");

        refresh_stored(&counting(), None, Stale::Expired)
            .await
            .unwrap();
        assert!(last_refresh_failure("counting", None).is_none());
    }

    #[tokio::test]
    async fn a_revoked_token_with_a_dead_refresh_token_asks_for_a_new_login() {
        let _config = config_with_login(u64::MAX).await;
        let rejected = load_credentials()
            .unwrap()
            .get("counting", None)
            .map(|(_, creds)| creds.access_token.clone())
            .unwrap();

        let error = refresh_stored(&RejectingProvider, None, Stale::Rejected(&rejected))
            .await
            .unwrap_err();
        let error = crate::providers::failed_refresh_error("counting", None, error);

        assert!(
            last_refresh_failure("counting", None).is_none(),
            "the clock says the token is still good, the server revoked it"
        );
        assert!(
            matches!(&error, crate::registry::ProviderError::LoginExpired { reason, .. } if reason == "refresh token expired"),
            "{error:?}"
        );
        assert!(!error.is_retryable());
        assert!(
            error.to_string().contains("/login counting default"),
            "{error}"
        );
    }

    #[test]
    fn an_expired_login_error_names_the_login_command() {
        let error = crate::registry::ProviderError::LoginExpired {
            provider: "anthropic".into(),
            account: AccountLabel::parse("work").unwrap(),
            reason: "refresh token expired".into(),
        };
        let message = error.to_string();
        assert!(message.contains("/login anthropic work"), "{message}");
        assert!(message.contains("refresh token expired"), "{message}");
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
