use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::{Completion, Config, Decision, Policy, Snapshot};

pub trait Clock: Send + Sync {
    fn now(&self) -> Option<Duration>;
}

pub trait ActivitySource: Send + Sync {
    fn snapshot(&self) -> Pin<Box<dyn Future<Output = Option<Snapshot>> + Send + '_>>;
}

pub trait Transport: Send + Sync {
    fn available(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;
    fn send(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    LocalOnly,
    WouldNotify,
    Accepted,
    Failed,
}

pub struct Dispatcher {
    policy: Arc<std::sync::Mutex<Policy>>,
    clock: Arc<dyn Clock>,
    activity: Arc<dyn ActivitySource>,
    transport: Arc<dyn Transport>,
    grace: Duration,
    task: Option<JoinHandle<Outcome>>,
    delivery: Arc<tokio::sync::Semaphore>,
}

impl Dispatcher {
    pub fn new(
        config: Config,
        clock: Arc<dyn Clock>,
        activity: Arc<dyn ActivitySource>,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, crate::ConfigError> {
        let grace = Duration::from_secs(config.grace_seconds);
        Ok(Self {
            policy: Arc::new(std::sync::Mutex::new(Policy::new(config)?)),
            clock,
            activity,
            transport,
            grace,
            task: None,
            delivery: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }

    pub fn completion(&mut self, completion: Completion) {
        let scheduled = self
            .policy
            .lock()
            .is_ok_and(|mut p| p.completion(completion, self.clock.now()));
        if !scheduled {
            return;
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let policy = Arc::clone(&self.policy);
        let clock = Arc::clone(&self.clock);
        let activity = Arc::clone(&self.activity);
        let transport = Arc::clone(&self.transport);
        let grace = self.grace;
        let delivery = Arc::clone(&self.delivery);
        self.task = Some(tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let available = tokio::time::timeout(Duration::from_secs(1), transport.available())
                .await
                == Ok(true);
            let snapshot = tokio::time::timeout(Duration::from_secs(2), activity.snapshot())
                .await
                .ok()
                .flatten();
            let decision = match policy.lock() {
                Ok(mut p) => {
                    p.evaluate_completion(completion, clock.now(), snapshot.as_ref(), available)
                }
                Err(_) => Decision::LocalOnly,
            };
            match decision {
                Decision::LocalOnly => Outcome::LocalOnly,
                Decision::WouldNotify => Outcome::WouldNotify,
                Decision::Send => {
                    let Ok(_permit) = delivery.try_acquire() else {
                        return Outcome::LocalOnly;
                    };
                    if tokio::time::timeout(Duration::from_secs(5), transport.send()).await
                        == Ok(true)
                    {
                        Outcome::Accepted
                    } else {
                        Outcome::Failed
                    }
                }
            }
        }));
    }

    pub fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Ok(mut p) = self.policy.lock() {
            p.cancel();
        }
    }

    pub async fn finished(&mut self) -> Option<Outcome> {
        if !self.task.as_ref().is_some_and(JoinHandle::is_finished) {
            return None;
        }
        self.task.take()?.await.ok()
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        self.cancel();
    }
}
