use super::{
    AuthPrompt, BrowserLogin, OAuthCredentials, OAuthError, OAuthProvider, PkceChallenge,
    generate_pkce,
};
use crate::oauth::callback_server::{CallbackConfig, CallbackServer};
use serde::Deserialize;

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const CALLBACK_PATH: &str = "/callback";
const CALLBACK_PORT: u16 = 54545;
const SCOPES: &str = "org:create_api_key user:profile user:inference";
const EXPIRY_BUFFER_MS: u64 = 5 * 60 * 1000;

pub struct AnthropicOAuth;

impl OAuthProvider for AnthropicOAuth {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn name(&self) -> &str {
        "Anthropic (Claude Pro/Max)"
    }

    fn begin_login(&self) -> Result<(AuthPrompt, PkceChallenge), OAuthError> {
        let pkce = generate_pkce()?;
        let url = build_auth_url(&pkce, REDIRECT_URI);
        let prompt = AuthPrompt {
            url,
            instructions: "after authorising, paste the code here (format: code#state)".into(),
        };
        Ok((prompt, pkce))
    }

    fn begin_browser_login(&self) -> super::OAuthFuture<'_, BrowserLogin> {
        Box::pin(async move {
            let pkce = generate_pkce()?;
            let state = pkce.verifier.clone();
            let server = CallbackServer::bind(
                CallbackConfig::new(CALLBACK_PATH, CALLBACK_PORT).expecting_state(state.clone()),
            )
            .await?;
            let redirect_uri = server.redirect_uri();
            let auth_url = build_auth_url(&pkce, &redirect_uri);
            Ok(BrowserLogin::new(
                auth_url,
                await_browser_credentials(server, pkce, redirect_uri),
            ))
        })
    }

    fn exchange_code(
        &self,
        code: &str,
        pkce: &PkceChallenge,
    ) -> super::OAuthFuture<'_, OAuthCredentials> {
        let code = code.to_string();
        let verifier = pkce.verifier.clone();
        Box::pin(async move {
            let (code, state) = split_code_and_state(&code);
            exchange_code_impl(&code, &state, &verifier, REDIRECT_URI).await
        })
    }

    fn refresh_token(&self, refresh_token: &str) -> super::OAuthFuture<'_, OAuthCredentials> {
        let token = refresh_token.to_string();
        Box::pin(async move { refresh_token_impl(&token).await })
    }
}

async fn await_browser_credentials(
    server: CallbackServer,
    pkce: PkceChallenge,
    redirect_uri: String,
) -> Result<OAuthCredentials, OAuthError> {
    let (params, writer) = server.await_callback().await?;

    if let Some(detail) = params.error_detail() {
        let _ = writer.write_error(&detail).await;
        return Err(OAuthError::TokenExchange(detail));
    }

    let code = params
        .code
        .ok_or_else(|| OAuthError::TokenExchange("oauth callback missing code".into()))?;
    let state = params.state.unwrap_or_else(|| pkce.verifier.clone());

    match exchange_code_impl(&code, &state, &pkce.verifier, &redirect_uri).await {
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

fn split_code_and_state(input: &str) -> (String, String) {
    let (code, state) = input.split_once('#').unwrap_or((input, ""));
    (code.to_string(), state.to_string())
}

fn build_auth_url(pkce: &PkceChallenge, redirect_uri: &str) -> String {
    let params = [
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPES),
        ("code_challenge", &pkce.challenge),
        ("code_challenge_method", "S256"),
        ("state", &pkce.verifier),
    ];

    let query: String = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    format!("{AUTHORIZE_URL}?{query}")
}

async fn request_tokens(
    body: serde_json::Value,
    error: fn(String) -> OAuthError,
) -> Result<OAuthCredentials, OAuthError> {
    let response = crate::http::client()
        .post(TOKEN_URL)
        .json(&body)
        .send()
        .await?;

    if !response.status().is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(error(text));
    }

    let data: TokenResponse = response.json().await?;
    Ok(OAuthCredentials {
        access_token: data.access_token,
        refresh_token: data.refresh_token,
        expires_at: crate::types::Timestamp::now().as_ms() + data.expires_in * 1000
            - EXPIRY_BUFFER_MS,
        account_id: None,
    })
}

async fn exchange_code_impl(
    code: &str,
    state: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthCredentials, OAuthError> {
    request_tokens(
        serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "state": state,
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
        }),
        OAuthError::TokenExchange,
    )
    .await
}

async fn refresh_token_impl(refresh_token: &str) -> Result<OAuthCredentials, OAuthError> {
    request_tokens(
        serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh_token,
        }),
        OAuthError::TokenRefresh,
    )
    .await
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_url_contains_required_params() {
        let pkce = generate_pkce().expect("pkce generation should succeed");
        let url = build_auth_url(&pkce, REDIRECT_URI);
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        assert!(url.contains("client_id="));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("redirect_uri="));
        assert!(url.contains("scope="));
    }

    #[test]
    fn provider_trait_basics() {
        let provider = AnthropicOAuth;
        assert_eq!(provider.id(), "anthropic");
        assert!(provider.name().contains("Anthropic"));

        let (prompt, pkce) = provider.begin_login().expect("begin_login should succeed");
        assert!(prompt.url.contains("claude.ai"));
        assert!(!pkce.verifier.is_empty());
    }

    #[tokio::test]
    async fn browser_login_points_the_redirect_at_the_bound_loopback_port() {
        let login = AnthropicOAuth
            .begin_browser_login()
            .await
            .expect("browser login should start");
        assert!(
            login
                .auth_url
                .starts_with("https://claude.ai/oauth/authorize?")
        );
        assert!(
            login
                .auth_url
                .contains("redirect_uri=http%3A%2F%2Flocalhost%3A"),
            "got: {}",
            login.auth_url
        );
        assert!(login.auth_url.contains("%2Fcallback"));
        assert!(login.auth_url.contains("code_challenge="));
    }
}
