use std::path::Path;
use std::time::Duration;

use super::anthropic::AnthropicDiscovery;
use super::cache::{DiscoveryCache, cache_path};
use super::codex::CodexDiscovery;
use super::openai::OpenAiDiscovery;
use super::openrouter::OpenRouterDiscovery;
use super::zai::ZaiDiscovery;
use super::{DiscoveryError, DiscoveryReport};
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
        let failed: Vec<_> = self
            .failed()
            .map(|(provider, error)| match error {
                DiscoveryError::TimedOut(limit) => {
                    format!("{provider} gave no answer within {}s", limit.as_secs())
                }
                _ => provider.to_string(),
            })
            .collect();

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

const PROVIDER_TIMEOUT: Duration = Duration::from_secs(20);

pub async fn run_all_available(client: crate::http::HttpClient) -> DiscoveryRunSummary {
    let anthropic = async {
        let key = resolve_auth_from_default_sources(&Provider::Anthropic)
            .await
            .api_key;
        run_one(Provider::Anthropic, key, |key| {
            let d = AnthropicDiscovery::new(client.clone(), "", key);
            async move { d.fetch().await }
        })
        .await
    };

    let openrouter = async {
        let key = env::env_api_key(&Provider::OpenRouter);
        let present = key.is_some();
        let d = OpenRouterDiscovery::new(client.clone(), "", key);
        DiscoveryRunResult {
            provider: Provider::OpenRouter,
            outcome: classify_outcome(d.fetch().await, present),
        }
    };

    let openai_provider = Provider::Custom("openai".into());
    let openai = async {
        let key = resolve_auth_from_default_sources(&openai_provider)
            .await
            .api_key;
        run_one(openai_provider.clone(), key, |key| {
            let d = OpenAiDiscovery::new(client.clone(), "", key);
            async move { d.fetch().await }
        })
        .await
    };

    let codex_provider = Provider::Custom("openai-codex".into());
    let codex = async {
        let auth = resolve_auth_from_default_sources(&codex_provider).await;
        run_one(codex_provider.clone(), auth.api_key, |key| {
            let d = CodexDiscovery::new(
                client.clone(),
                "",
                key,
                auth.account_id,
                env!("CARGO_PKG_VERSION"),
            );
            async move { d.fetch().await }
        })
        .await
    };

    let zai_provider = Provider::Custom("zai".into());
    let zai = async {
        let key = resolve_auth_from_default_sources(&zai_provider)
            .await
            .api_key;
        run_one(zai_provider.clone(), key, |_| {
            let d = ZaiDiscovery::new(client.clone(), "");
            async move { d.fetch().await }
        })
        .await
    };

    let results = tokio::join!(
        within(Provider::Anthropic, PROVIDER_TIMEOUT, anthropic),
        within(Provider::OpenRouter, PROVIDER_TIMEOUT, openrouter),
        within(openai_provider.clone(), PROVIDER_TIMEOUT, openai),
        within(codex_provider.clone(), PROVIDER_TIMEOUT, codex),
        within(zai_provider.clone(), PROVIDER_TIMEOUT, zai),
    );
    DiscoveryRunSummary {
        results: vec![results.0, results.1, results.2, results.3, results.4],
    }
}

async fn within(
    provider: Provider,
    limit: Duration,
    run: impl std::future::Future<Output = DiscoveryRunResult>,
) -> DiscoveryRunResult {
    tokio::time::timeout(limit, run)
        .await
        .unwrap_or_else(|_| DiscoveryRunResult {
            provider,
            outcome: DiscoveryOutcome::Failed(DiscoveryError::TimedOut(limit)),
        })
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
    let summary = run_all_available(client).await;

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

    #[tokio::test(start_paused = true)]
    async fn a_provider_that_never_answers_fails_as_timed_out_while_the_others_land() {
        let (stalled, answered) = tokio::join!(
            within(
                Provider::Custom("slow".into()),
                PROVIDER_TIMEOUT,
                std::future::pending()
            ),
            within(Provider::Anthropic, PROVIDER_TIMEOUT, async {
                DiscoveryRunResult {
                    provider: Provider::Anthropic,
                    outcome: classify_outcome(
                        Ok(DiscoveryReport {
                            provider: Provider::Anthropic,
                            fetched_at: SystemTime::UNIX_EPOCH,
                            models: vec![sample_model("claude-x").into()],
                        }),
                        true,
                    ),
                }
            }),
        );

        assert!(matches!(
            stalled.outcome,
            DiscoveryOutcome::Failed(DiscoveryError::TimedOut(limit)) if limit == PROVIDER_TIMEOUT
        ));
        let summary = DiscoveryRunSummary {
            results: vec![answered, stalled],
        };
        assert_eq!(summary.total_models(), 1);
        assert_eq!(
            summary.one_line(),
            "1 models from anthropic (errors: slow gave no answer within 20s)"
        );
    }

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
