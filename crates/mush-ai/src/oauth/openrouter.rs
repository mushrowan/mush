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

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";

pub struct OpenRouterOAuth;

impl OAuthProvider for OpenRouterOAuth {
    fn id(&self) -> &str {
        "openrouter"
    }

    fn name(&self) -> &str {
        "OpenRouter (web sign-in)"
    }

    fn begin_login(&self) -> Result<(AuthPrompt, PkceChallenge), OAuthError> {
        // full localhost-callback flow lands in a follow-up; the
        // skeleton here just confirms the provider registers and shows
        // up in the picker. concrete URL building is exercised by
        // `build_auth_url` once we wire in the local callback server
        let pkce = generate_pkce()?;
        Ok((
            AuthPrompt {
                url: AUTHORIZE_URL.into(),
                instructions: "openrouter web sign-in not yet implemented".into(),
            },
            pkce,
        ))
    }

    fn exchange_code(
        &self,
        _code: &str,
        _pkce: &PkceChallenge,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OAuthCredentials, OAuthError>> + Send + '_>,
    > {
        Box::pin(async move {
            Err(OAuthError::TokenExchange(
                "openrouter web sign-in not yet implemented".into(),
            ))
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
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn build_auth_url(callback_url: &str, pkce: &PkceChallenge) -> String {
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
}
