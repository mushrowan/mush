//! oauth credential management
//!
//! handles the PKCE authorization code flow for claude.ai (anthropic oauth).
//! credentials are persisted to ~/.config/mush/oauth.json and auto-refreshed.

use std::path::PathBuf;

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::Digest;

// decoded from pi's base64: "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference";

/// buffer before expiry (5 minutes)
const EXPIRY_BUFFER_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum OAuthError {
    #[error("oauth token exchange failed: {0}")]
    TokenExchange(String),

    #[error("oauth token refresh failed: {0}")]
    TokenRefresh(String),

    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("failed to read/write credentials: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse credentials: {0}")]
    Json(#[from] serde_json::Error),
}

/// stored oauth credentials
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    pub access_token: String,
    pub refresh_token: String,
    /// epoch ms when the access token expires
    pub expires_at: u64,
}

impl OAuthCredentials {
    pub fn is_expired(&self) -> bool {
        timestamp_ms() >= self.expires_at
    }
}

/// PKCE verifier + challenge pair
#[derive(Debug)]
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
}

/// generate a PKCE code verifier and S256 challenge
pub fn generate_pkce() -> PkceChallenge {
    let mut verifier_bytes = [0u8; 32];
    getrandom::fill(&mut verifier_bytes).expect("failed to generate random bytes");
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes);

    let hash = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);

    PkceChallenge {
        verifier,
        challenge,
    }
}

/// build the authorization URL the user needs to visit
pub fn build_auth_url(pkce: &PkceChallenge) -> String {
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

/// exchange an authorization code for tokens
///
/// the auth_code should be in the format "code#state" as pasted by the user
pub async fn exchange_code(
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
    })
}

/// refresh an expired access token
pub async fn refresh_token(refresh_token: &str) -> Result<OAuthCredentials, OAuthError> {
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
    })
}

/// get a valid access token, refreshing if needed
pub async fn get_valid_token(creds: &OAuthCredentials) -> Result<OAuthCredentials, OAuthError> {
    if creds.is_expired() {
        refresh_token(&creds.refresh_token).await
    } else {
        Ok(creds.clone())
    }
}

// -- credential persistence --

fn credentials_path() -> PathBuf {
    if let Ok(dir) = std::env::var("MUSH_CONFIG_DIR") {
        PathBuf::from(dir).join("oauth.json")
    } else if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(config).join("mush/oauth.json")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/mush/oauth.json")
    } else {
        PathBuf::from(".mush/oauth.json")
    }
}

/// all stored credentials keyed by provider name
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CredentialStore {
    #[serde(flatten)]
    pub providers: std::collections::HashMap<String, OAuthCredentials>,
}

/// load credentials from disk
pub fn load_credentials() -> Result<CredentialStore, OAuthError> {
    let path = credentials_path();
    if !path.exists() {
        return Ok(CredentialStore::default());
    }
    let content = std::fs::read_to_string(path)?;
    let store: CredentialStore = serde_json::from_str(&content)?;
    Ok(store)
}

/// save credentials to disk
pub fn save_credentials(store: &CredentialStore) -> Result<(), OAuthError> {
    let path = credentials_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(store)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// get a valid anthropic oauth token, refreshing and saving if needed
pub async fn get_anthropic_oauth_token() -> Result<Option<String>, OAuthError> {
    let mut store = load_credentials()?;
    let Some(creds) = store.providers.get("anthropic") else {
        return Ok(None);
    };

    let updated = get_valid_token(creds).await?;
    let token = updated.access_token.clone();
    store.providers.insert("anthropic".into(), updated);
    save_credentials(&store)?;

    Ok(Some(token))
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
    fn pkce_generates_valid_pair() {
        let pkce = generate_pkce();
        assert!(!pkce.verifier.is_empty());
        assert!(!pkce.challenge.is_empty());
        assert_ne!(pkce.verifier, pkce.challenge);
    }

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        let pkce = generate_pkce();
        let hash = sha2::Sha256::digest(pkce.verifier.as_bytes());
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn pkce_is_unique() {
        let a = generate_pkce();
        let b = generate_pkce();
        assert_ne!(a.verifier, b.verifier);
    }

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
    fn credentials_expired_check() {
        let creds = OAuthCredentials {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: 0, // well in the past
        };
        assert!(creds.is_expired());

        let creds = OAuthCredentials {
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: timestamp_ms() + 60_000,
        };
        assert!(!creds.is_expired());
    }

    #[test]
    fn credential_store_roundtrip() {
        let mut store = CredentialStore::default();
        store.providers.insert(
            "anthropic".into(),
            OAuthCredentials {
                access_token: "acc".into(),
                refresh_token: "ref".into(),
                expires_at: 12345,
            },
        );

        let json = serde_json::to_string(&store).unwrap();
        let restored: CredentialStore = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.providers["anthropic"].access_token, "acc");
    }

    #[test]
    fn credential_store_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth.json");

        let mut store = CredentialStore::default();
        store.providers.insert(
            "anthropic".into(),
            OAuthCredentials {
                access_token: "token".into(),
                refresh_token: "refresh".into(),
                expires_at: 99999,
            },
        );

        let json = serde_json::to_string_pretty(&store).unwrap();
        std::fs::write(&path, &json).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let loaded: CredentialStore = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.providers.len(), 1);
    }
}
