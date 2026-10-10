use crate::types::{ApiKey, Provider};

pub fn env_api_key(provider: &Provider) -> Option<ApiKey> {
    let var_name = match provider {
        Provider::Anthropic => "ANTHROPIC_API_KEY",
        Provider::OpenRouter => "OPENROUTER_API_KEY",
        Provider::Custom(name) => {
            let var = format!("{}_API_KEY", name.to_uppercase().replace('-', "_"));
            return std::env::var(var).ok().and_then(ApiKey::new);
        }
    };
    std::env::var(var_name).ok().and_then(ApiKey::new)
}

pub fn anthropic_api_key() -> Option<ApiKey> {
    std::env::var("ANTHROPIC_OAUTH_TOKEN")
        .ok()
        .and_then(ApiKey::new)
        .or_else(|| {
            std::env::var("ANTHROPIC_API_KEY")
                .ok()
                .and_then(ApiKey::new)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_api_key_missing() {
        assert_eq!(env_api_key(&Provider::Custom("nonexistent".into())), None);
    }

    #[test]
    fn custom_provider_env_var_name() {
        let name = "my-proxy";
        let expected = format!("{}_API_KEY", name.to_uppercase().replace('-', "_"));
        assert_eq!(expected, "MY_PROXY_API_KEY");
    }
}
