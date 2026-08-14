use super::{
    AuthPrompt, BrowserLogin, OAuthCredentials, OAuthError, OAuthProvider, PkceChallenge,
    generate_pkce,
};
use crate::oauth::callback_server::{CallbackConfig, CallbackServer};

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const API_BASE_URL: &str = "https://openrouter.ai/api/v1";
const CALLBACK_PATH: &str = "/auth/callback";
const CALLBACK_PORT: u16 = 1456;
const DEFAULT_REDIRECT_URI: &str = "http://localhost:1456/auth/callback";

pub struct OpenRouterOAuth;

impl OAuthProvider for OpenRouterOAuth {
    fn id(&self) -> &str {
        "openrouter"
    }

    fn name(&self) -> &str {
        "OpenRouter (web sign-in)"
    }

    fn begin_login(&self) -> Result<(AuthPrompt, PkceChallenge), OAuthError> {
        let pkce = generate_pkce()?;
        let url = build_auth_url(DEFAULT_REDIRECT_URI, &pkce);
        Ok((
            AuthPrompt {
                url,
                instructions:
                    "after authorising, paste the full redirect URL (or just the code) here".into(),
            },
            pkce,
        ))
    }

    fn begin_browser_login(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<BrowserLogin, OAuthError>> + Send + '_>,
    > {
        Box::pin(async move {
            let session = OpenRouterLoginSession::start().await?;
            let auth_url = session.auth_url.clone();
            Ok(BrowserLogin::new(auth_url, session.await_credentials()))
        })
    }

    fn exchange_code(
        &self,
        code: &str,
        pkce: &PkceChallenge,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OAuthCredentials, OAuthError>> + Send + '_>,
    > {
        let input = code.to_string();
        let verifier = pkce.verifier.clone();
        Box::pin(async move {
            let code = extract_code(&input).ok_or_else(|| {
                OAuthError::TokenExchange("missing authorization code in pasted input".into())
            })?;
            let pkce = PkceChallenge {
                verifier,
                challenge: String::new(),
            };
            exchange_code_for_key(&code, &pkce).await
        })
    }

    fn refresh_token(
        &self,
        _refresh_token: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OAuthCredentials, OAuthError>> + Send + '_>,
    > {
        Box::pin(async move {
            Err(OAuthError::TokenRefresh(
                "openrouter api keys do not expire and cannot be refreshed; \
                 log in again to issue a new key"
                    .into(),
            ))
        })
    }
}

fn build_auth_url(callback_url: &str, pkce: &PkceChallenge) -> String {
    let params = [
        ("callback_url", callback_url),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
    ];
    let query: String = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZE_URL}?{query}")
}

pub struct OpenRouterLoginSession {
    pub auth_url: String,
    pkce: PkceChallenge,
    server: CallbackServer,
}

impl OpenRouterLoginSession {
    pub async fn start() -> Result<Self, OAuthError> {
        let pkce = generate_pkce()?;
        let server =
            CallbackServer::bind(CallbackConfig::new(CALLBACK_PATH, CALLBACK_PORT)).await?;
        let auth_url = build_auth_url(&server.redirect_uri(), &pkce);
        Ok(Self {
            auth_url,
            pkce,
            server,
        })
    }

    pub fn port(&self) -> u16 {
        self.server.port()
    }

    pub async fn await_credentials(self) -> Result<OAuthCredentials, OAuthError> {
        let pkce = self.pkce;
        let (params, writer) = self.server.await_callback().await?;

        if let Some(detail) = params.error_detail() {
            let _ = writer.write_error(&detail).await;
            return Err(OAuthError::TokenExchange(detail));
        }

        let code = params
            .code
            .ok_or_else(|| OAuthError::TokenExchange("oauth callback missing code".into()))?;

        match exchange_code_for_key(&code, &pkce).await {
            Ok(creds) => {
                let _ = writer.write_success().await;
                Ok(creds)
            }
            Err(err) => {
                let _ = writer.write_error(&err.to_string()).await;
                Err(err)
            }
        }
    }
}

fn extract_code(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(url) = reqwest::Url::parse(value) {
        return url
            .query_pairs()
            .find_map(|(k, v)| (k == "code").then(|| v.into_owned()));
    }
    if value.contains("code=") {
        let query = value.split('?').next_back().unwrap_or(value);
        for pair in query.split('&') {
            let mut kv = pair.splitn(2, '=');
            let key = kv.next().unwrap_or_default();
            let raw = kv.next().unwrap_or_default();
            if key == "code" {
                let decoded = urlencoding::decode(raw)
                    .map(|s| s.into_owned())
                    .unwrap_or_else(|_| raw.to_string());
                return Some(decoded);
            }
        }
    }
    Some(value.to_string())
}

async fn exchange_code_for_key(
    code: &str,
    pkce: &PkceChallenge,
) -> Result<OAuthCredentials, OAuthError> {
    use openrouter_rs::api::auth::{CodeChallengeMethod, exchange_code_for_api_key};

    let response = exchange_code_for_api_key(
        API_BASE_URL,
        code,
        Some(&pkce.verifier),
        Some(CodeChallengeMethod::S256),
    )
    .await
    .map_err(|e| OAuthError::TokenExchange(format!("openrouter: {e}")))?;

    Ok(OAuthCredentials {
        access_token: response.key,
        refresh_token: String::new(),
        expires_at: u64::MAX,
        account_id: response.user_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_trait_basics() {
        let provider = OpenRouterOAuth;
        assert_eq!(provider.id(), "openrouter");
        assert!(provider.name().to_lowercase().contains("openrouter"));
    }

    #[test]
    fn build_auth_url_carries_pkce_and_callback() {
        let pkce = generate_pkce().expect("pkce generation should succeed");
        let url = build_auth_url("http://localhost:1456/auth/callback", &pkce);
        assert!(url.starts_with("https://openrouter.ai/auth?"));
        assert!(url.contains("callback_url=http%3A%2F%2Flocalhost%3A1456%2Fauth%2Fcallback"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
    }

    #[tokio::test]
    async fn login_session_starts_and_exposes_redirect_url() {
        let session = OpenRouterLoginSession::start()
            .await
            .expect("session start");
        let port = session.port();
        assert!(session.auth_url.starts_with("https://openrouter.ai/auth?"));
        assert!(session.auth_url.contains(&format!(
            "callback_url=http%3A%2F%2Flocalhost%3A{port}%2Fauth%2Fcallback"
        )));
        assert!(session.auth_url.contains("code_challenge="));
    }
}
