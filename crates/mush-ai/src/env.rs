//! api key resolution from environment variables

use crate::types::Provider;

/// resolve an api key for a provider from environment variables
pub fn env_api_key(provider: &Provider) -> Option<String> {
    let var_name = match provider {
        Provider::Anthropic => "ANTHROPIC_API_KEY",
        Provider::OpenRouter => "OPENROUTER_API_KEY",
        Provider::Custom(name) => {
            // convention: PROVIDER_NAME_API_KEY (uppercase, hyphens to underscores)
            let var = format!("{}_API_KEY", name.to_uppercase().replace('-', "_"));
            return std::env::var(var).ok().filter(|v| !v.is_empty());
        }
    };
    std::env::var(var_name).ok().filter(|v| !v.is_empty())
}

/// check for anthropic oauth token (takes precedence over api key)
pub fn anthropic_api_key() -> Option<String> {
    std::env::var("ANTHROPIC_OAUTH_TOKEN")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok().filter(|v| !v.is_empty()))
}

/// whether a key looks like an anthropic oauth token
pub fn is_oauth_token(key: &str) -> bool {
    key.contains("sk-ant-oat")
}

#[cfg(test)]
mod tests {
    use super::*;

    // env var tests are unsafe in edition 2024 since set_var/remove_var
    // are inherently racy. we wrap them in unsafe blocks and accept the risk
    // since these tests run serially in practice.

    #[test]
    fn env_api_key_missing() {
        assert_eq!(env_api_key(&Provider::Custom("nonexistent".into())), None);
    }

    #[test]
    fn is_oauth_token_detection() {
        assert!(is_oauth_token("sk-ant-oat-abc123"));
        assert!(!is_oauth_token("sk-ant-api-abc123"));
        assert!(!is_oauth_token("some-random-key"));
    }

    #[test]
    fn custom_provider_env_var_name() {
        // just test the convention without touching env vars
        let name = "my-proxy";
        let expected = format!("{}_API_KEY", name.to_uppercase().replace('-', "_"));
        assert_eq!(expected, "MY_PROXY_API_KEY");
    }
}
