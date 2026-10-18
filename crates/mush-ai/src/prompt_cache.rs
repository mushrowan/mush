use std::time::Duration;

use crate::types::{Api, CacheRetention, Model, Provider, StreamOptions};

const FIVE_MINUTES: Duration = Duration::from_secs(5 * 60);
const HALF_AN_HOUR: Duration = Duration::from_secs(30 * 60);
const ONE_HOUR: Duration = Duration::from_secs(60 * 60);
const MIN_REUSE_MARGIN: Duration = Duration::from_secs(60);

#[must_use]
pub fn guaranteed_lifetime(model: &Model, options: &StreamOptions) -> Option<Duration> {
    let retention = options.cache_retention.unwrap_or(CacheRetention::Short);
    if retention == CacheRetention::None {
        return None;
    }
    match (&model.provider, model.api) {
        (Provider::Anthropic, Api::AnthropicMessages)
            if crate::providers::anthropic::is_direct_api(&model.base_url) =>
        {
            let is_oauth = options
                .api_key
                .clone()
                .or_else(crate::env::anthropic_api_key)
                .is_some_and(|key| key.is_oauth_token());
            Some(
                if crate::providers::anthropic::caches_for_an_hour(
                    &model.base_url,
                    retention,
                    is_oauth,
                ) {
                    ONE_HOUR
                } else {
                    FIVE_MINUTES
                },
            )
        }
        (Provider::Custom(name), Api::OpenaiResponses)
            if name == "openai" && model.base_url.contains("api.openai.com") =>
        {
            Some(if keeps_half_an_hour(model.id.as_str()) {
                HALF_AN_HOUR
            } else {
                FIVE_MINUTES
            })
        }
        _ => None,
    }
}

#[must_use]
pub fn reuse_deadline(lifetime: Duration) -> Duration {
    lifetime.saturating_sub((lifetime / 12).max(MIN_REUSE_MARGIN))
}

fn keeps_half_an_hour(model_id: &str) -> bool {
    let Some(version) = model_id.strip_prefix("gpt-") else {
        return false;
    };
    let digits = |text: &str| -> (u32, usize) {
        let end = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        (text[..end].parse().unwrap_or(0), end)
    };
    let (major, end) = digits(version);
    let minor = version[end..]
        .strip_prefix('.')
        .map_or(0, |rest| digits(rest).0);
    (major, minor) >= (5, 6)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ApiKey;

    fn find(models: Vec<Model>, id: &str) -> Model {
        models
            .into_iter()
            .find(|model| model.id.as_str() == id)
            .unwrap_or_else(|| panic!("{id} is in the catalogue"))
    }

    fn keyed(key: &str, retention: Option<CacheRetention>) -> StreamOptions {
        StreamOptions {
            api_key: ApiKey::new(key),
            cache_retention: retention,
            ..StreamOptions::default()
        }
    }

    #[test]
    fn the_anthropic_api_keeps_an_hour_for_a_login_or_a_long_retention_and_five_minutes_otherwise()
    {
        let opus = find(crate::models::anthropic_models(), "claude-opus-5-5");

        assert_eq!(
            guaranteed_lifetime(&opus, &keyed("sk-ant-oat01-login", None)),
            Some(ONE_HOUR)
        );
        assert_eq!(
            guaranteed_lifetime(&opus, &keyed("sk-ant-api03-key", None)),
            Some(FIVE_MINUTES)
        );
        assert_eq!(
            guaranteed_lifetime(
                &opus,
                &keyed("sk-ant-api03-key", Some(CacheRetention::Long))
            ),
            Some(ONE_HOUR)
        );
        assert_eq!(
            guaranteed_lifetime(
                &opus,
                &keyed("sk-ant-oat01-login", Some(CacheRetention::None))
            ),
            None
        );
    }

    #[test]
    fn a_proxy_or_a_router_promises_no_lifetime() {
        let mut proxied = find(crate::models::anthropic_models(), "claude-opus-5-5");
        proxied.base_url = "https://llm-gateway.example.com".into();
        let routed = crate::models::openrouter_models().remove(0);
        let codex = crate::models::openai_codex_models().remove(0);
        let options = keyed("sk-ant-oat01-login", Some(CacheRetention::Long));

        assert_eq!(guaranteed_lifetime(&proxied, &options), None);
        assert_eq!(guaranteed_lifetime(&routed, &options), None);
        assert_eq!(guaranteed_lifetime(&codex, &options), None);
    }

    #[test]
    fn the_openai_api_keeps_half_an_hour_from_gpt_5_6_and_five_minutes_before() {
        let mut model = find(crate::models::openai_models(), "gpt-5.2");
        let options = StreamOptions::default();
        assert_eq!(guaranteed_lifetime(&model, &options), Some(FIVE_MINUTES));

        for (id, lifetime) in [
            ("gpt-5.6", HALF_AN_HOUR),
            ("gpt-5.6-mini", HALF_AN_HOUR),
            ("gpt-6.1-sol", HALF_AN_HOUR),
            ("gpt-5.10", HALF_AN_HOUR),
            ("gpt-5", FIVE_MINUTES),
            ("gpt-4.1", FIVE_MINUTES),
            ("o4-mini", FIVE_MINUTES),
        ] {
            model.id = id.into();
            assert_eq!(
                guaranteed_lifetime(&model, &options),
                Some(lifetime),
                "{id}"
            );
        }
    }

    #[test]
    fn the_cache_is_reused_with_a_twelfth_of_its_lifetime_to_spare_and_at_least_a_minute() {
        assert_eq!(reuse_deadline(ONE_HOUR), Duration::from_secs(55 * 60));
        assert_eq!(
            reuse_deadline(HALF_AN_HOUR),
            Duration::from_secs(27 * 60 + 30)
        );
        assert_eq!(reuse_deadline(FIVE_MINUTES), Duration::from_secs(4 * 60));
        assert_eq!(reuse_deadline(Duration::from_secs(30)), Duration::ZERO);
    }
}
