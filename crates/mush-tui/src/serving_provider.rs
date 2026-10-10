#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    Implicit,
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServingProviderCache {
    pub ttl_secs: u16,
    pub mode: CacheMode,
}

#[must_use]
pub fn serving_provider_cache(name: &str) -> Option<ServingProviderCache> {
    match canonical_name(name).as_str() {
        "anthropic" | "amazonbedrock" | "bedrock" | "googlevertex" | "vertex" => {
            Some(ServingProviderCache {
                ttl_secs: 300,
                mode: CacheMode::Explicit,
            })
        }
        "openai" | "azure" => Some(ServingProviderCache {
            ttl_secs: 600,
            mode: CacheMode::Implicit,
        }),
        "google" | "googleaistudio" | "gemini" => Some(ServingProviderCache {
            ttl_secs: 240,
            mode: CacheMode::Implicit,
        }),
        "moonshot" | "moonshotai" => Some(ServingProviderCache {
            ttl_secs: 300,
            mode: CacheMode::Implicit,
        }),
        _ => None,
    }
}

fn canonical_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_is_explicit_5min() {
        let b = serving_provider_cache("Anthropic").expect("known");
        assert_eq!(b.ttl_secs, 300);
        assert_eq!(b.mode, CacheMode::Explicit);
    }

    #[test]
    fn gemini_routes_are_implicit_short_ttl() {
        let b = serving_provider_cache("Google AI Studio").expect("known");
        assert_eq!(b.ttl_secs, 240);
        assert_eq!(b.mode, CacheMode::Implicit);
        let vertex = serving_provider_cache("Google Vertex").expect("known");
        assert_eq!(vertex.mode, CacheMode::Explicit);
    }

    #[test]
    fn openai_is_implicit_medium_ttl() {
        let b = serving_provider_cache("OpenAI").expect("known");
        assert_eq!(b.ttl_secs, 600);
        assert_eq!(b.mode, CacheMode::Implicit);
    }

    #[test]
    fn lookup_is_insensitive_to_case_and_punctuation() {
        assert!(serving_provider_cache("anthropic").is_some());
        assert!(serving_provider_cache("ANTHROPIC").is_some());
        assert!(serving_provider_cache("  Anthropic  ").is_some());
        assert!(serving_provider_cache("amazon-bedrock").is_some());
        assert!(serving_provider_cache("Amazon Bedrock").is_some());
        assert!(serving_provider_cache("Moonshot AI").is_some());
    }

    #[test]
    fn unknown_provider_returns_none() {
        assert!(serving_provider_cache("NeverHeardOfYou").is_none());
        assert!(serving_provider_cache("").is_none());
        assert!(serving_provider_cache("Unknown").is_none());
    }
}
