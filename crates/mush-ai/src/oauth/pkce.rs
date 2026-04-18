//! PKCE (proof key for code exchange) utilities

use base64::Engine;
use sha2::Digest;

use super::OAuthError;

/// PKCE verifier + challenge pair
#[derive(Debug, Clone)]
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
}

/// generate a PKCE code verifier and S256 challenge
pub fn generate_pkce() -> Result<PkceChallenge, OAuthError> {
    let mut verifier_bytes = [0u8; 32];
    getrandom::fill(&mut verifier_bytes)
        .map_err(|e| OAuthError::TokenExchange(format!("failed to generate random bytes: {e}")))?;
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes);

    let hash = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);

    Ok(PkceChallenge {
        verifier,
        challenge,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_valid_pair() {
        let pkce = generate_pkce().expect("pkce generation should succeed");
        assert!(!pkce.verifier.is_empty());
        assert!(!pkce.challenge.is_empty());
        assert_ne!(pkce.verifier, pkce.challenge);
    }

    #[test]
    fn challenge_is_sha256_of_verifier() {
        let pkce = generate_pkce().expect("pkce generation should succeed");
        let hash = sha2::Sha256::digest(pkce.verifier.as_bytes());
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn is_unique() {
        let a = generate_pkce().expect("pkce generation should succeed");
        let b = generate_pkce().expect("pkce generation should succeed");
        assert_ne!(a.verifier, b.verifier);
    }
}
