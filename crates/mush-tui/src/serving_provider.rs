//! upstream provider identification and per-provider cache behaviour.
//!
//! the *serving provider* is the entity that actually executed a turn,
//! distinct from the *vendor* that trained the model. for openrouter
//! routes the same model id (e.g. `anthropic/claude-sonnet-4.5`) can
//! be served by anthropic, amazon bedrock, or google vertex on
//! different turns. each serving provider has its own cache mode and
//! TTL.
//!
//! we learn the serving provider from openrouter's `/generation`
//! response (`provider_name` field). this module is the lookup that
//! turns that name into known cache semantics

/// how a serving provider exposes prompt caching to clients
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    /// no caching available
    None,
    /// the provider caches automatically; clients don't opt in
    Implicit,
    /// the client must mark `cache_control` breakpoints to enable caching
    Explicit,
}

/// cache behaviour for a known serving provider
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServingProviderCache {
    /// approximate seconds the cache stays warm after the last hit.
    /// used to drive the warmth countdown UI. real TTLs vary so this
    /// is a midpoint estimate, not an exact value
    pub ttl_secs: u16,
    /// whether the provider needs explicit `cache_control` markers
    pub mode: CacheMode,
    /// minimum prompt length (in tokens) for caching to engage. shorter
    /// prompts skip the cache entirely on most providers
    pub min_tokens: u32,
}

/// look up cache behaviour for an upstream serving provider by name.
/// the name is openrouter's `/generation` `provider_name` value, or
/// any equivalent label (case-insensitive, whitespace/punctuation
/// insensitive). returns `None` when the provider isn't recognised so
/// callers can fall back to a conservative default
#[must_use]
pub fn serving_provider_cache(name: &str) -> Option<ServingProviderCache> {
    match canonical_name(name).as_str() {
        // anthropic direct: explicit cache_control, 5min default TTL,
        // 1h opt via ttl: "1h". minimum prompt is 1024 tokens for
        // sonnet/opus 4 and below, larger for newer models
        "anthropic" => Some(ServingProviderCache {
            ttl_secs: 300,
            mode: CacheMode::Explicit,
            min_tokens: 1024,
        }),
        // bedrock and vertex serve claude with the same explicit cache
        // control semantics. top-level cache_control isn't supported
        // on these but per-block breakpoints work
        "amazonbedrock" | "bedrock" => Some(ServingProviderCache {
            ttl_secs: 300,
            mode: CacheMode::Explicit,
            min_tokens: 1024,
        }),
        "googlevertex" | "vertex" => Some(ServingProviderCache {
            ttl_secs: 300,
            mode: CacheMode::Explicit,
            min_tokens: 1024,
        }),
        // openai: implicit caching, 5-10 min reported TTL, midpoint
        // is ~10 min. min 1024 tokens
        "openai" => Some(ServingProviderCache {
            ttl_secs: 600,
            mode: CacheMode::Implicit,
            min_tokens: 1024,
        }),
        "azure" => Some(ServingProviderCache {
            ttl_secs: 600,
            mode: CacheMode::Implicit,
            min_tokens: 1024,
        }),
        // gemini 2.5 implicit caching: openrouter docs say "3-5 min
        // average", use 240s as the midpoint. min 1028 (flash) or
        // 2048 (pro), use 2048 as the conservative ceiling
        "google" | "googleaistudio" | "gemini" => Some(ServingProviderCache {
            ttl_secs: 240,
            mode: CacheMode::Implicit,
            min_tokens: 2048,
        }),
        // deepseek: implicit caching with long-lived caches reported
        // by their docs. use 1h as the conservative display
        "deepseek" => Some(ServingProviderCache {
            ttl_secs: 3600,
            mode: CacheMode::Implicit,
            min_tokens: 0,
        }),
        // xai grok: implicit caching, exact TTL not published. assume
        // 5 min as a safe conservative
        "xai" => Some(ServingProviderCache {
            ttl_secs: 300,
            mode: CacheMode::Implicit,
            min_tokens: 0,
        }),
        // moonshot: implicit caching across kimi models
        "moonshot" | "moonshotai" => Some(ServingProviderCache {
            ttl_secs: 300,
            mode: CacheMode::Implicit,
            min_tokens: 0,
        }),
        // groq: caching only on kimi k2 models per openrouter docs.
        // we surface the general behaviour and let supports_caching()
        // gate at the model level
        "groq" => Some(ServingProviderCache {
            ttl_secs: 300,
            mode: CacheMode::Implicit,
            min_tokens: 0,
        }),
        _ => None,
    }
}

/// strip whitespace and punctuation, lowercase, so "Google Vertex",
/// "google-vertex", and "GoogleVertex" all match the same key
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
    fn anthropic_is_explicit_5min_with_1k_minimum() {
        let b = serving_provider_cache("Anthropic").expect("known");
        assert_eq!(b.ttl_secs, 300);
        assert_eq!(b.mode, CacheMode::Explicit);
        assert_eq!(b.min_tokens, 1024);
    }

    #[test]
    fn gemini_routes_are_implicit_short_ttl() {
        // openrouter exposes gemini via google ai studio and vertex.
        // both should resolve to the same implicit short-TTL behaviour
        let b = serving_provider_cache("Google AI Studio").expect("known");
        assert_eq!(b.ttl_secs, 240);
        assert_eq!(b.mode, CacheMode::Implicit);
        let vertex = serving_provider_cache("Google Vertex").expect("known");
        // vertex serves both gemini AND claude. when serving claude
        // it's explicit; we resolve to explicit since that's the more
        // common cache-control path through openrouter
        assert_eq!(vertex.mode, CacheMode::Explicit);
    }

    #[test]
    fn openai_is_implicit_medium_ttl() {
        let b = serving_provider_cache("OpenAI").expect("known");
        assert_eq!(b.ttl_secs, 600);
        assert_eq!(b.mode, CacheMode::Implicit);
        assert_eq!(b.min_tokens, 1024);
    }

    #[test]
    fn deepseek_caches_for_hours() {
        // deepseek docs note long-lived caches; we display 1h
        let b = serving_provider_cache("DeepSeek").expect("known");
        assert_eq!(b.ttl_secs, 3600);
        assert_eq!(b.mode, CacheMode::Implicit);
    }

    #[test]
    fn lookup_is_insensitive_to_case_and_punctuation() {
        // openrouter's provider_name strings use varying capitalisation
        // and sometimes spaces or dashes. all should canonicalise to
        // the same lookup key
        assert!(serving_provider_cache("anthropic").is_some());
        assert!(serving_provider_cache("ANTHROPIC").is_some());
        assert!(serving_provider_cache("  Anthropic  ").is_some());
        assert!(serving_provider_cache("amazon-bedrock").is_some());
        assert!(serving_provider_cache("Amazon Bedrock").is_some());
        assert!(serving_provider_cache("Moonshot AI").is_some());
        assert!(serving_provider_cache("xAI").is_some());
    }

    #[test]
    fn unknown_provider_returns_none() {
        // long-tail providers (together, deepinfra, fireworks, etc.)
        // we haven't yet researched should signal "unknown" so
        // callers can pick a conservative fallback rather than
        // pretending we know the TTL
        assert!(serving_provider_cache("NeverHeardOfYou").is_none());
        assert!(serving_provider_cache("").is_none());
        assert!(serving_provider_cache("Together").is_none());
    }
}
