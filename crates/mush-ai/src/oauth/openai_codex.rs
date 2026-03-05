//! openai codex oauth provider (chatgpt plus/pro subscription)

use base64::Engine;
use reqwest::Url;
use serde::Deserialize;

use super::{
    AuthPrompt, OAuthCredentials, OAuthError, OAuthProvider, PkceChallenge, generate_pkce,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
const CLAIM_PATH: &str = "https://api.openai.com/auth";
const EXPIRY_BUFFER_MS: u64 = 5 * 60 * 1000;

pub struct OpenaiCodexOAuth;

impl OAuthProvider for OpenaiCodexOAuth {
    fn id(&self) -> &str {
        "openai-codex"
    }

    fn name(&self) -> &str {
        "ChatGPT Plus/Pro (Codex subscription)"
    }

    fn begin_login(&self) -> Result<(AuthPrompt, PkceChallenge), OAuthError> {
        let pkce = generate_pkce()?;
        let url = build_auth_url(&pkce);
        let prompt = AuthPrompt {
            url,
            instructions: "after authorising, paste the full redirect URL (or code) here".into(),
        };
        Ok((prompt, pkce))
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
        Box::pin(async move { exchange_code_impl(&input, &verifier).await })
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
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPE),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "mush"),
        // keep state equal to verifier so we can validate paste input
        ("state", pkce.verifier.as_str()),
    ];

    let query: String = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    format!("{AUTHORIZE_URL}?{query}")
}

async fn exchange_code_impl(input: &str, verifier: &str) -> Result<OAuthCredentials, OAuthError> {
    let parsed = parse_authorization_input(input);
    let code = parsed.code.ok_or_else(|| {
        OAuthError::TokenExchange("missing authorization code in pasted input".into())
    })?;

    if let Some(state) = parsed.state
        && state != verifier
    {
        return Err(OAuthError::TokenExchange(
            "oauth state mismatch, please retry login".into(),
        ));
    }

    let body = [
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code.as_str()),
        ("code_verifier", verifier),
        ("redirect_uri", REDIRECT_URI),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
    .collect::<Vec<_>>()
    .join("&");

    let client = reqwest::Client::new();
    let response = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;

    if !response.status().is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(OAuthError::TokenExchange(text));
    }

    let data: TokenResponse = response.json().await?;
    let account_id =
        extract_account_id(&data.id_token).or_else(|| extract_account_id(&data.access_token));

    Ok(OAuthCredentials {
        access_token: data.access_token,
        refresh_token: data.refresh_token,
        expires_at: timestamp_ms() + data.expires_in * 1000 - EXPIRY_BUFFER_MS,
        account_id,
    })
}

async fn refresh_token_impl(refresh_token: &str) -> Result<OAuthCredentials, OAuthError> {
    let body = [
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", refresh_token),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
    .collect::<Vec<_>>()
    .join("&");

    let client = reqwest::Client::new();
    let response = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;

    if !response.status().is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(OAuthError::TokenRefresh(text));
    }

    let data: TokenResponse = response.json().await?;
    let account_id =
        extract_account_id(&data.id_token).or_else(|| extract_account_id(&data.access_token));

    Ok(OAuthCredentials {
        access_token: data.access_token,
        refresh_token: data.refresh_token,
        expires_at: timestamp_ms() + data.expires_in * 1000 - EXPIRY_BUFFER_MS,
        account_id,
    })
}

#[derive(Debug)]
struct ParsedInput {
    code: Option<String>,
    state: Option<String>,
}

fn parse_authorization_input(input: &str) -> ParsedInput {
    let value = input.trim();
    if value.is_empty() {
        return ParsedInput {
            code: None,
            state: None,
        };
    }

    if let Ok(url) = Url::parse(value) {
        return ParsedInput {
            code: url
                .query_pairs()
                .find_map(|(k, v)| (k == "code").then(|| v.to_string())),
            state: url
                .query_pairs()
                .find_map(|(k, v)| (k == "state").then(|| v.to_string())),
        };
    }

    if value.contains('#') {
        let mut parts = value.splitn(2, '#');
        return ParsedInput {
            code: parts.next().map(str::to_string),
            state: parts.next().map(str::to_string),
        };
    }

    if value.contains("code=") {
        let query = value.split('?').next_back().unwrap_or(value);
        let mut code = None;
        let mut state = None;

        for pair in query.split('&') {
            let mut kv = pair.splitn(2, '=');
            let key = kv.next().unwrap_or_default();
            let raw = kv.next().unwrap_or_default();
            let decoded = urlencoding::decode(raw)
                .map(|s| s.into_owned())
                .unwrap_or_else(|_| raw.to_string());

            match key {
                "code" => code = Some(decoded),
                "state" => state = Some(decoded),
                _ => {}
            }
        }

        return ParsedInput { code, state };
    }

    ParsedInput {
        code: Some(value.to_string()),
        state: None,
    }
}

fn extract_account_id(token: &str) -> Option<String> {
    let payload_b64 = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload_b64))
        .ok()?;

    let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;

    json.get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            json.get(CLAIM_PATH)
                .and_then(|v| v.get("chatgpt_account_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            json.get("organizations")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|org| org.get("id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: String,
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
    fn auth_url_contains_expected_params() {
        let pkce = generate_pkce().expect("pkce generation should succeed");
        let url = build_auth_url(&pkce);
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains("client_id="));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("originator=mush"));
    }

    #[test]
    fn parse_authorization_code_from_url() {
        let parsed =
            parse_authorization_input("http://localhost:1455/auth/callback?code=abc&state=xyz");
        assert_eq!(parsed.code.as_deref(), Some("abc"));
        assert_eq!(parsed.state.as_deref(), Some("xyz"));
    }

    #[test]
    fn parse_code_hash_format() {
        let parsed = parse_authorization_input("abc#xyz");
        assert_eq!(parsed.code.as_deref(), Some("abc"));
        assert_eq!(parsed.state.as_deref(), Some("xyz"));
    }

    #[test]
    fn extract_nested_account_id() {
        let payload = serde_json::json!({
            CLAIM_PATH: {
                "chatgpt_account_id": "acc_123"
            }
        });
        let payload_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string());
        let token = format!("header.{payload_b64}.sig");

        assert_eq!(extract_account_id(&token).as_deref(), Some("acc_123"));
    }

    #[test]
    fn provider_trait_basics() {
        let provider = OpenaiCodexOAuth;
        assert_eq!(provider.id(), "openai-codex");
        assert!(provider.name().contains("Codex"));
    }
}
