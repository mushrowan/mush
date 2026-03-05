//! anthropic oauth provider (claude pro/max)

use super::{
    AuthPrompt, OAuthCredentials, OAuthError, OAuthProvider, PkceChallenge, generate_pkce,
};
use serde::Deserialize;

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
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

    fn begin_login(&self) -> (AuthPrompt, PkceChallenge) {
        let pkce = generate_pkce();
        let url = build_auth_url(&pkce);
        let prompt = AuthPrompt {
            url,
            instructions: "after authorising, paste the code here (format: code#state)".into(),
        };
        (prompt, pkce)
    }

    fn exchange_code(
        &self,
        code: &str,
        pkce: &PkceChallenge,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OAuthCredentials, OAuthError>> + Send + '_>,
    > {
        let code = code.to_string();
        let verifier = pkce.verifier.clone();
        Box::pin(async move { exchange_code_impl(&code, &verifier).await })
    }

    fn refresh_token(
        &self,
        refresh_token: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OAuthCredentials, OAuthError>> + Send + '_>,
    > {
        let token = refresh_token.to_string();
        Box::pin(async move { refresh_token_impl(&token).await })
    }
}

fn build_auth_url(pkce: &PkceChallenge) -> String {
    let params = [
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
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

async fn exchange_code_impl(
    auth_code: &str,
    verifier: &str,
) -> Result<OAuthCredentials, OAuthError> {
    let (code, state) = auth_code.split_once('#').unwrap_or((auth_code, ""));

    let client = reqwest::Client::new();
    let response = client
        .post(TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "state": state,
            "redirect_uri": REDIRECT_URI,
            "code_verifier": verifier,
        }))
        .send()
        .await?;

    if !response.status().is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(OAuthError::TokenExchange(text));
    }

    let data: TokenResponse = response.json().await?;
    Ok(OAuthCredentials {
        access_token: data.access_token,
        refresh_token: data.refresh_token,
        expires_at: timestamp_ms() + data.expires_in * 1000 - EXPIRY_BUFFER_MS,
        account_id: None,
    })
}

async fn refresh_token_impl(refresh_token: &str) -> Result<OAuthCredentials, OAuthError> {
    let client = reqwest::Client::new();
    let response = client
        .post(TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh_token,
        }))
        .send()
        .await?;

    if !response.status().is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(OAuthError::TokenRefresh(text));
    }

    let data: TokenResponse = response.json().await?;
    Ok(OAuthCredentials {
        access_token: data.access_token,
        refresh_token: data.refresh_token,
        expires_at: timestamp_ms() + data.expires_in * 1000 - EXPIRY_BUFFER_MS,
        account_id: None,
    })
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

fn timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_url_contains_required_params() {
        let pkce = generate_pkce();
        let url = build_auth_url(&pkce);
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
}
