//! openrouter oauth provider (PKCE web sign-in)
//!
//! openrouter's auth flow differs from anthropic/codex in two important
//! ways:
//!
//! 1. the upstream redirect target uses a `callback_url` query param
//!    (rather than a registered redirect_uri), and the PKCE proofs are
//!    sent on the `/auth` URL itself (`code_challenge`,
//!    `code_challenge_method`).
//! 2. the exchange returns a permanent, user-controlled API key rather
//!    than a refresh-token-style access token. there's no expiry and
//!    no refresh: once issued the key is valid until the user revokes
//!    it from openrouter's dashboard.
//!
//! we lean on the `openrouter-rs` SDK for the actual `auth/keys`
//! exchange so we don't hand-roll the request shape, but everything
//! around it (URL building, the localhost callback server, credential
//! storage in mush's `oauth.json`) reuses the same primitives the
//! anthropic and codex providers already share.

use super::{
    AuthPrompt, OAuthCredentials, OAuthError, OAuthProvider, PkceChallenge, generate_pkce,
};
use crate::oauth::openrouter_callback_server::OpenRouterCallbackServer;

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const API_BASE_URL: &str = "https://openrouter.ai/api/v1";
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
        // mirror codex's trait surface: return a localhost callback
        // URL that the better-UX path ([`OpenRouterLoginSession`])
        // will also bind. paste-the-code flows can still extract the
        // code from the browser URL bar after the redirect fails to
        // connect, but the local-server path is preferred everywhere
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
            // accept either a raw code or the full redirect URL the
            // browser landed on. mirrors codex's "paste anything that
            // includes ?code=..." UX
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
        // openrouter issues permanent api keys, there's no refresh
        // flow. returning an error here ensures any code path that
        // calls refresh on a (somehow) expired credential surfaces a
        // clear failure rather than silently looping
        Box::pin(async move {
            Err(OAuthError::TokenRefresh(
                "openrouter api keys do not expire and cannot be refreshed; \
                 log in again to issue a new key"
                    .into(),
            ))
        })
    }
}

/// canonical openrouter authorize URL with PKCE proofs and the
/// localhost callback the user will redirect back to. broken out so
/// tests can pin the param shape without driving the whole flow
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

/// in-progress openrouter login: holds the local callback server and
/// the auth url to show the user. call [`Self::await_credentials`] to
/// wait for the browser callback and complete the token exchange.
/// modelled after [`crate::oauth::openai_codex::CodexLoginSession`].
pub struct OpenRouterLoginSession {
    pub auth_url: String,
    pkce: PkceChallenge,
    server: OpenRouterCallbackServer,
}

impl OpenRouterLoginSession {
    /// bind the local callback server and prepare the auth url
    pub async fn start() -> Result<Self, OAuthError> {
        let pkce = generate_pkce()?;
        let server = OpenRouterCallbackServer::bind().await?;
        let redirect_uri = server.redirect_uri();
        let auth_url = build_auth_url(&redirect_uri, &pkce);
        Ok(Self {
            auth_url,
            pkce,
            server,
        })
    }

    pub fn port(&self) -> u16 {
        self.server.port()
    }

    /// wait for the browser callback, exchange the code for a
    /// permanent openrouter api key, and wrap it in our
    /// [`OAuthCredentials`] envelope. the issued key never expires, so
    /// `expires_at` is set to the far future to short-circuit any
    /// future refresh attempts
    pub async fn await_credentials(self) -> Result<OAuthCredentials, OAuthError> {
        let pkce = self.pkce;
        let (params, writer) = self.server.await_callback().await?;

        if let Some(error) = params.error {
            let detail = params
                .error_description
                .map(|d| format!("{error}: {d}"))
                .unwrap_or(error);
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

/// pull the `code` out of either a raw value or a full redirect URL.
/// supports the same flexible paste behaviour codex's flow does, so
/// users can hand us either copy form
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

/// drive openrouter-rs's `auth/keys` exchange and translate the
/// result into mush's credential envelope. kept as a standalone
/// function so unit tests can swap the SDK call in the future without
/// touching the session plumbing
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
        // openrouter keys are permanent; pin expiry to the far future
        // so `is_expired` returns false and the refresh path stays
        // unreachable for this provider
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
        // openrouter's PKCE flow expects the challenge + method on the
        // `/auth` URL itself (not as a separate `state` param). the
        // callback_url must round-trip url-encoded so colons and
        // slashes survive
        let pkce = generate_pkce().expect("pkce generation should succeed");
        let url = build_auth_url("http://localhost:1456/auth/callback", &pkce);
        assert!(url.starts_with("https://openrouter.ai/auth?"));
        assert!(url.contains("callback_url=http%3A%2F%2Flocalhost%3A1456%2Fauth%2Fcallback"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
    }

    #[tokio::test]
    async fn login_session_starts_and_exposes_redirect_url() {
        // session::start binds a localhost listener and folds its
        // ephemeral port into the canonical /auth URL. callers only
        // need to surface `auth_url` to the user; the rest of the
        // flow drives itself off the bound socket
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
