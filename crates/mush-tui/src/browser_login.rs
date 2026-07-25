//! in-flight oauth browser login
//!
//! `/login <provider>` opens the user's browser and leaves a loopback
//! callback server listening. that wait happens on a spawned task so the
//! event loop keeps drawing; the task hands its outcome back through a
//! oneshot that the runner drains each frame.

use mush_ai::oauth::{OAuthCredentials, OAuthError};
use tokio::sync::oneshot;

/// how long to keep waiting for the browser before giving up. long
/// enough to log in to a fresh account, short enough that a forgotten
/// tab eventually frees the callback port
pub const BROWSER_LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// a browser login waiting on its callback. dropping this aborts the
/// wait, which closes the loopback listener and frees the port
#[derive(Debug)]
pub struct PendingBrowserLogin {
    /// oauth provider id the flow will save credentials under
    pub provider_id: String,
    /// display name for status and system messages
    pub provider_name: String,
    outcome: oneshot::Receiver<Result<OAuthCredentials, String>>,
    task: tokio::task::JoinHandle<()>,
}

impl PendingBrowserLogin {
    /// spawn `completion` (a provider's `BrowserLogin::await_credentials`)
    /// and keep the handle needed to poll it
    pub fn spawn(
        provider_id: impl Into<String>,
        provider_name: impl Into<String>,
        completion: impl Future<Output = Result<OAuthCredentials, OAuthError>> + Send + 'static,
    ) -> Self {
        let (tx, outcome) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = match tokio::time::timeout(BROWSER_LOGIN_TIMEOUT, completion).await {
                Ok(Ok(creds)) => Ok(creds),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("timed out waiting for the browser".into()),
            };
            let _ = tx.send(result);
        });
        Self {
            provider_id: provider_id.into(),
            provider_name: provider_name.into(),
            outcome,
            task,
        }
    }

    /// the flow's outcome, once it has one. `None` while the browser
    /// callback is still outstanding
    pub fn poll(&mut self) -> Option<Result<OAuthCredentials, String>> {
        match self.outcome.try_recv() {
            Ok(result) => Some(result),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => {
                Some(Err("the login task stopped unexpectedly".into()))
            }
        }
    }
}

impl Drop for PendingBrowserLogin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> OAuthCredentials {
        OAuthCredentials {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: u64::MAX,
            account_id: None,
        }
    }

    async fn settle(pending: &mut PendingBrowserLogin) -> Result<OAuthCredentials, String> {
        for _ in 0..100 {
            if let Some(result) = pending.poll() {
                return result;
            }
            tokio::task::yield_now().await;
        }
        panic!("login never settled");
    }

    #[tokio::test]
    async fn poll_is_empty_until_the_browser_callback_lands() {
        let (release, wait) = oneshot::channel::<()>();
        let mut pending = PendingBrowserLogin::spawn("anthropic", "Anthropic", async move {
            let _ = wait.await;
            Ok(credentials())
        });

        assert!(
            pending.poll().is_none(),
            "must not report an outcome while the user is still in the browser"
        );

        release.send(()).expect("release the flow");
        let creds = settle(&mut pending).await.expect("should succeed");
        assert_eq!(creds.access_token, "access");
    }

    #[tokio::test]
    async fn poll_surfaces_the_provider_failure_as_a_message() {
        let mut pending = PendingBrowserLogin::spawn("openai-codex", "Codex", async {
            Err(OAuthError::TokenExchange("code already used".into()))
        });
        let err = settle(&mut pending).await.expect_err("should fail");
        assert!(err.contains("code already used"), "got {err}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_forgotten_browser_tab_times_out() {
        let mut pending =
            PendingBrowserLogin::spawn("anthropic", "Anthropic", std::future::pending());
        // let the task register its timer before the clock jumps
        tokio::task::yield_now().await;
        tokio::time::advance(BROWSER_LOGIN_TIMEOUT + std::time::Duration::from_secs(1)).await;
        let err = settle(&mut pending).await.expect_err("should time out");
        assert!(err.contains("timed out"), "got {err}");
    }
}
