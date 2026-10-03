use std::path::Path;
use std::time::Duration;

use super::anthropic::AnthropicDiscovery;
use super::cache::{DiscoveryCache, cache_path};
use super::codex::CodexDiscovery;
use super::openai::OpenAiDiscovery;
use super::openrouter::OpenRouterDiscovery;
use super::zai::ZaiDiscovery;
use super::{DiscoveryError, DiscoveryReport, ModelDiscovery};
use crate::auth::resolve_auth_from_default_sources;
use crate::env;
use crate::types::{ApiKey, Provider};

#[derive(Debug)]
pub struct DiscoveryRunResult {
    pub provider: Provider,
    pub outcome: DiscoveryOutcome,
}

#[derive(Debug)]
pub enum DiscoveryOutcome {
    Success {
        models: usize,
        report: DiscoveryReport,
    },
    Skipped,
    Failed(DiscoveryError),
}

#[derive(Debug, Default)]
pub struct DiscoveryRunSummary {
    pub results: Vec<DiscoveryRunResult>,
}

impl DiscoveryRunSummary {
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

    pub fn succeeded(&self) -> impl Iterator<Item = &Provider> + '_ {
        self.results.iter().filter_map(|r| match r.outcome {
            DiscoveryOutcome::Success { .. } => Some(&r.provider),
            _ => None,
        })
    }

    pub fn failed(&self) -> impl Iterator<Item = (&Provider, &DiscoveryError)> + '_ {
        self.results.iter().filter_map(|r| match &r.outcome {
            DiscoveryOutcome::Failed(e) => Some((&r.provider, e)),
            _ => None,
        })
    }

    pub fn skipped(&self) -> impl Iterator<Item = &Provider> + '_ {
        self.results.iter().filter_map(|r| match r.outcome {
            DiscoveryOutcome::Skipped => Some(&r.provider),
            _ => None,
        })
    }

    pub fn apply_to_cache(&self, cache: &mut DiscoveryCache) {
        for result in &self.results {
            if let DiscoveryOutcome::Success { report, .. } = &result.outcome {
                cache.apply_report(report.clone());
            }
        }
    }

    pub fn apply_to_cache_and_save(&self, path: &Path) -> Result<(), std::io::Error> {
        let mut cache = DiscoveryCache::load(path).unwrap_or_default();
        self.apply_to_cache(&mut cache);
        cache.save(path)
    }

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

pub async fn run_all_available(client: crate::http::HttpClient) -> DiscoveryRunSummary {
    let mut results = Vec::new();

    let anthropic_key = discovery_auth(&Provider::Anthropic).await.api_key;
    results.push(
        run_one(Provider::Anthropic, anthropic_key, |key| {
            let d = AnthropicDiscovery::new(client.clone(), "", key);
            async move { d.fetch().await }
        })
        .await,
    );

    let openrouter_key = env::env_api_key(&Provider::OpenRouter);
    let openrouter_present = openrouter_key.is_some();
    let d = OpenRouterDiscovery::new(client.clone(), "", openrouter_key);
    results.push(DiscoveryRunResult {
        provider: Provider::OpenRouter,
        outcome: classify_outcome(d.fetch().await, openrouter_present),
    });

    let openai_provider = Provider::Custom("openai".into());
    let openai_key = resolve_auth_from_default_sources(&openai_provider)
        .await
        .api_key;
    results.push(
        run_one(openai_provider, openai_key, |key| {
            let d = OpenAiDiscovery::new(client.clone(), "", key);
            async move { d.fetch().await }
        })
        .await,
    );

    let codex_provider = Provider::Custom("openai-codex".into());
    let codex_auth = resolve_auth_from_default_sources(&codex_provider).await;
    results.push(
        run_one(codex_provider, codex_auth.api_key, |key| {
            let d = CodexDiscovery::new(
                client.clone(),
                "",
                key,
                codex_auth.account_id,
                env!("CARGO_PKG_VERSION"),
            );
            async move { d.fetch().await }
        })
        .await,
    );

    let zai_provider = Provider::Custom("zai".into());
    let zai_key = resolve_auth_from_default_sources(&zai_provider)
        .await
        .api_key;
    results.push(
        run_one(zai_provider, zai_key, |_| {
            let d = ZaiDiscovery::new(client.clone(), "");
            async move { d.fetch().await }
        })
        .await,
    );

    DiscoveryRunSummary { results }
}

async fn discovery_auth(provider: &Provider) -> crate::auth::ResolvedAuth {
    match provider {
        Provider::OpenRouter => crate::auth::ResolvedAuth {
            api_key: env::env_api_key(provider),
            account_id: None,
        },
        _ => resolve_auth_from_default_sources(provider).await,
    }
}

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

pub async fn refresh_and_save(client: crate::http::HttpClient) -> DiscoveryRunSummary {
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

    #[tokio::test]
    async fn discovery_auth_uses_shared_resolution_for_anthropic() {
        let auth = discovery_auth(&Provider::Anthropic).await;
        assert!(auth.api_key.is_some() || auth.account_id.is_none());
    }

    fn sample_model(id: &str) -> Model {
        use crate::types::{Api, TokenCount};
        Model::builder()
            .id(id.into())
            .name(id.into())
            .api(Api::AnthropicMessages)
            .provider(Provider::Anthropic)
            .base_url("https://api.anthropic.com".into())
            .context_window(TokenCount::new(200_000))
            .max_output_tokens(TokenCount::new(64_000))
            .build()
    }
}
