//! orchestrate discovery across every provider with available credentials.
//!
//! produces a [`DiscoveryRunSummary`] suitable for log output or status-line
//! feedback. providers without configured credentials are silently skipped
//! (recorded as `skipped`), so this is safe to fire-and-forget at startup.
//!
//! the returned summary does *not* persist the cache - call
//! [`DiscoveryRunSummary::apply_to_cache_and_save`] for that, or use the
//! [`refresh_and_save`] convenience wrapper.

use std::path::Path;
use std::time::Duration;

use super::anthropic::AnthropicDiscovery;
use super::cache::{DiscoveryCache, cache_path};
use super::codex::CodexDiscovery;
use super::openai::OpenAiDiscovery;
use super::openrouter::OpenRouterDiscovery;
use super::{DiscoveryError, DiscoveryReport, ModelDiscovery};
use crate::env;
use crate::oauth::get_openai_codex_oauth_token;
use crate::types::{ApiKey, Provider};

/// per-provider outcome of one discovery run.
#[derive(Debug)]
pub struct DiscoveryRunResult {
    pub provider: Provider,
    pub outcome: DiscoveryOutcome,
}

#[derive(Debug)]
pub enum DiscoveryOutcome {
    /// the fetch succeeded and returned this many models
    Success {
        models: usize,
        report: DiscoveryReport,
    },
    /// no credentials were configured for this provider
    Skipped,
    /// the upstream returned an error
    Failed(DiscoveryError),
}

/// summary of a discovery run across all providers.
#[derive(Debug, Default)]
pub struct DiscoveryRunSummary {
    pub results: Vec<DiscoveryRunResult>,
}

impl DiscoveryRunSummary {
    /// total number of models successfully fetched across providers
    #[must_use]
    pub fn total_models(&self) -> usize {
        self.results
            .iter()
            .map(|r| match &r.outcome {
                DiscoveryOutcome::Success { models, .. } => *models,
                _ => 0,
            })
            .sum()
    }

    /// providers that succeeded
    pub fn succeeded(&self) -> impl Iterator<Item = &Provider> + '_ {
        self.results.iter().filter_map(|r| match r.outcome {
            DiscoveryOutcome::Success { .. } => Some(&r.provider),
            _ => None,
        })
    }

    /// providers that failed (with their error)
    pub fn failed(&self) -> impl Iterator<Item = (&Provider, &DiscoveryError)> + '_ {
        self.results.iter().filter_map(|r| match &r.outcome {
            DiscoveryOutcome::Failed(e) => Some((&r.provider, e)),
            _ => None,
        })
    }

    /// providers that were skipped because no credentials were configured
    pub fn skipped(&self) -> impl Iterator<Item = &Provider> + '_ {
        self.results.iter().filter_map(|r| match r.outcome {
            DiscoveryOutcome::Skipped => Some(&r.provider),
            _ => None,
        })
    }

    /// merge every successful report into the given cache.
    /// failed/skipped providers are left untouched.
    pub fn apply_to_cache(&self, cache: &mut DiscoveryCache) {
        for result in &self.results {
            if let DiscoveryOutcome::Success { report, .. } = &result.outcome {
                cache.apply_report(report.clone());
            }
        }
    }

    /// load the on-disk cache, merge this summary's reports, write it back.
    pub fn apply_to_cache_and_save(&self, path: &Path) -> Result<(), std::io::Error> {
        let mut cache = DiscoveryCache::load(path).unwrap_or_default();
        self.apply_to_cache(&mut cache);
        cache.save(path)
    }

    /// human-readable one-line summary for status messages.
    #[must_use]
    pub fn one_line(&self) -> String {
        let ok: Vec<_> = self.succeeded().map(Provider::to_string).collect();
        let skipped: Vec<_> = self.skipped().map(Provider::to_string).collect();
        let failed: Vec<_> = self.failed().map(|(p, _)| p.to_string()).collect();

        let mut parts = vec![format!("{} models", self.total_models())];
        if !ok.is_empty() {
            parts.push(format!("from {}", ok.join(", ")));
        }
        if !skipped.is_empty() {
            parts.push(format!("(no creds: {})", skipped.join(", ")));
        }
        if !failed.is_empty() {
            parts.push(format!("(errors: {})", failed.join(", ")));
        }
        parts.join(" ")
    }
}

/// run discovery across every provider with available credentials.
///
/// providers are queried sequentially to keep the request budget low and
/// share the reqwest client. concurrent fetches would be marginally faster
/// but invite rate limit pile-ups.
pub async fn run_all_available(client: reqwest::Client) -> DiscoveryRunSummary {
    let mut results = Vec::new();

    // anthropic - api key or oauth token
    let anthropic_key = env::anthropic_api_key();
    results.push(
        run_one(Provider::Anthropic, anthropic_key, |key| {
            let d = AnthropicDiscovery::new(client.clone(), "", key);
            async move { d.fetch().await }
        })
        .await,
    );

    // openrouter - api key optional, /models works unauthenticated for the public catalogue
    let openrouter_key = env::env_api_key(&Provider::OpenRouter);
    let openrouter_present = openrouter_key.is_some();
    let d = OpenRouterDiscovery::new(client.clone(), "", openrouter_key);
    results.push(DiscoveryRunResult {
        provider: Provider::OpenRouter,
        outcome: classify_outcome(d.fetch().await, openrouter_present),
    });

    // openai - api key required
    let openai_provider = Provider::Custom("openai".into());
    let openai_key = env::env_api_key(&openai_provider);
    results.push(
        run_one(openai_provider, openai_key, |key| {
            let d = OpenAiDiscovery::new(client.clone(), "", key);
            async move { d.fetch().await }
        })
        .await,
    );

    // openai-codex (chatgpt subscription) - oauth token + chatgpt account id
    let codex_provider = Provider::Custom("openai-codex".into());
    let codex_token = match get_openai_codex_oauth_token().await {
        Ok(token) => token.and_then(ApiKey::new),
        Err(e) => {
            tracing::debug!(error = %e, "failed to read codex oauth token; skipping codex discovery");
            None
        }
    };
    results.push(
        run_one(codex_provider, codex_token, |key| {
            let d = CodexDiscovery::new(client.clone(), "", key, None, env!("CARGO_PKG_VERSION"));
            async move { d.fetch().await }
        })
        .await,
    );

    DiscoveryRunSummary { results }
}

/// shorthand: detect-or-skip + classify the outcome.
async fn run_one<F, Fut>(
    provider: Provider,
    key: Option<ApiKey>,
    build_and_fetch: F,
) -> DiscoveryRunResult
where
    F: FnOnce(ApiKey) -> Fut,
    Fut: std::future::Future<Output = Result<DiscoveryReport, DiscoveryError>>,
{
    let outcome = match key {
        Some(key) => classify_outcome(build_and_fetch(key).await, true),
        None => DiscoveryOutcome::Skipped,
    };
    DiscoveryRunResult { provider, outcome }
}

/// classify a discovery fetch result. `creds_present` lets us distinguish
/// "no creds" from "creds rejected by upstream" for openrouter (the only
/// provider whose `/models` works unauthenticated).
fn classify_outcome(
    res: Result<DiscoveryReport, DiscoveryError>,
    creds_present: bool,
) -> DiscoveryOutcome {
    match res {
        Ok(report) => DiscoveryOutcome::Success {
            models: report.models.len(),
            report,
        },
        Err(DiscoveryError::Auth { .. }) if !creds_present => DiscoveryOutcome::Skipped,
        Err(e) => DiscoveryOutcome::Failed(e),
    }
}

/// run discovery and persist the cache. used by the slash command and
/// the optional startup spawn.
///
/// returns the summary so the caller can render status-line feedback.
pub async fn refresh_and_save(client: reqwest::Client) -> DiscoveryRunSummary {
    let summary = tokio::time::timeout(Duration::from_secs(30), run_all_available(client))
        .await
        .unwrap_or_default();

    if let Err(e) = summary.apply_to_cache_and_save(&cache_path()) {
        tracing::warn!(error = %e, "failed to persist discovery cache");
    }
    summary
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::types::Model;

    #[test]
    fn one_line_with_no_results_is_zero_models() {
        let summary = DiscoveryRunSummary::default();
        assert!(summary.one_line().contains("0 models"));
    }

    #[test]
    fn one_line_lists_succeeded_providers() {
        let summary = DiscoveryRunSummary {
            results: vec![DiscoveryRunResult {
                provider: Provider::Anthropic,
                outcome: DiscoveryOutcome::Success {
                    models: 5,
                    report: DiscoveryReport {
                        provider: Provider::Anthropic,
                        fetched_at: SystemTime::UNIX_EPOCH,
                        models: vec![],
                    },
                },
            }],
        };
        let line = summary.one_line();
        assert!(line.contains("5 models"));
        assert!(line.contains("anthropic"));
    }

    #[test]
    fn one_line_includes_skipped_and_failed() {
        let summary = DiscoveryRunSummary {
            results: vec![
                DiscoveryRunResult {
                    provider: Provider::OpenRouter,
                    outcome: DiscoveryOutcome::Skipped,
                },
                DiscoveryRunResult {
                    provider: Provider::Custom("openai".into()),
                    outcome: DiscoveryOutcome::Failed(DiscoveryError::Auth {
                        status: 401,
                        body: "nope".into(),
                    }),
                },
            ],
        };
        let line = summary.one_line();
        assert!(line.contains("openrouter"));
        assert!(line.contains("openai"));
        assert!(line.contains("no creds"));
        assert!(line.contains("errors"));
    }

    #[test]
    fn apply_to_cache_only_records_successes() {
        let success_report = DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: SystemTime::UNIX_EPOCH,
            models: vec![sample_model("claude-x").into()],
        };
        let summary = DiscoveryRunSummary {
            results: vec![
                DiscoveryRunResult {
                    provider: Provider::Anthropic,
                    outcome: DiscoveryOutcome::Success {
                        models: 1,
                        report: success_report,
                    },
                },
                DiscoveryRunResult {
                    provider: Provider::OpenRouter,
                    outcome: DiscoveryOutcome::Skipped,
                },
            ],
        };

        let mut cache = DiscoveryCache::default();
        summary.apply_to_cache(&mut cache);
        assert!(cache.providers.contains_key("anthropic"));
        assert!(!cache.providers.contains_key("openrouter"));
    }

    #[test]
    fn classify_outcome_with_no_creds_and_auth_error_is_skipped() {
        let res: Result<DiscoveryReport, DiscoveryError> = Err(DiscoveryError::Auth {
            status: 401,
            body: "missing".into(),
        });
        match classify_outcome(res, false) {
            DiscoveryOutcome::Skipped => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn classify_outcome_with_creds_and_auth_error_is_failed() {
        let res: Result<DiscoveryReport, DiscoveryError> = Err(DiscoveryError::Auth {
            status: 401,
            body: "bad key".into(),
        });
        match classify_outcome(res, true) {
            DiscoveryOutcome::Failed(_) => {}
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    fn sample_model(id: &str) -> Model {
        use crate::types::{Api, InputModality, ModelCost, TokenCount};
        Model {
            id: id.into(),
            name: id.into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(64_000),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        }
    }
}
