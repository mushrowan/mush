use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::Duration;

use mush_notify::*;

fn config(mode: Mode) -> Config {
    Config {
        mode,
        account: "+12025550100".into(),
        recipients: vec!["+12025550101".into()],
        socket: Some("/run/user/1000/private/socket".into()),
        ..Config::default()
    }
}

fn snapshot(at: u64, activity: Activity) -> Snapshot {
    Snapshot {
        observed_at: Duration::from_secs(at),
        sessions: vec![SessionActivity {
            local: true,
            activity,
        }],
    }
}

#[test]
fn grace_idle_locked_stale_remote_and_multiple_sessions() {
    for (name, data, expected) in [
        (
            "locked",
            Some(snapshot(1060, Activity::Locked)),
            Decision::Send,
        ),
        (
            "idle",
            Some(snapshot(
                1060,
                Activity::IdleSince(Duration::from_secs(760)),
            )),
            Decision::Send,
        ),
        (
            "recent idle",
            Some(snapshot(
                1060,
                Activity::IdleSince(Duration::from_secs(761)),
            )),
            Decision::LocalOnly,
        ),
        (
            "future idle",
            Some(snapshot(
                1060,
                Activity::IdleSince(Duration::from_secs(1061)),
            )),
            Decision::LocalOnly,
        ),
        (
            "zero idle",
            Some(snapshot(1060, Activity::IdleSince(Duration::ZERO))),
            Decision::LocalOnly,
        ),
        (
            "active",
            Some(snapshot(1060, Activity::Active)),
            Decision::LocalOnly,
        ),
        (
            "unknown",
            Some(snapshot(1060, Activity::Unknown)),
            Decision::LocalOnly,
        ),
        (
            "stale",
            Some(snapshot(1054, Activity::Locked)),
            Decision::LocalOnly,
        ),
        (
            "future snapshot",
            Some(snapshot(1061, Activity::Locked)),
            Decision::LocalOnly,
        ),
        ("no bus", None, Decision::LocalOnly),
        (
            "no session",
            Some(Snapshot {
                observed_at: Duration::from_secs(1060),
                sessions: vec![],
            }),
            Decision::LocalOnly,
        ),
        (
            "remote only",
            Some(Snapshot {
                observed_at: Duration::from_secs(1060),
                sessions: vec![SessionActivity {
                    local: false,
                    activity: Activity::Locked,
                }],
            }),
            Decision::LocalOnly,
        ),
        (
            "another active session",
            Some(Snapshot {
                observed_at: Duration::from_secs(1060),
                sessions: vec![
                    SessionActivity {
                        local: true,
                        activity: Activity::Locked,
                    },
                    SessionActivity {
                        local: true,
                        activity: Activity::Active,
                    },
                ],
            }),
            Decision::LocalOnly,
        ),
    ] {
        let mut p = Policy::new(config(Mode::Send)).unwrap();
        assert!(p.completion(Completion(1), Some(Duration::from_secs(1000))));
        assert_eq!(
            p.evaluate(Some(Duration::from_secs(1059)), data.as_ref(), true),
            Decision::LocalOnly,
            "{name}"
        );
        assert_eq!(
            p.evaluate(Some(Duration::from_secs(1060)), data.as_ref(), true),
            expected,
            "{name}"
        );
        assert_eq!(
            p.evaluate(Some(Duration::from_secs(1120)), data.as_ref(), true),
            Decision::LocalOnly
        );
        assert!(!p.completion(Completion(1), Some(Duration::from_secs(1120))));
    }
}

#[test]
fn cancellation_no_clock_unavailable_and_rate_limit() {
    let mut p = Policy::new(config(Mode::Send)).unwrap();
    assert!(!p.completion(Completion(1), None));
    assert!(p.completion(Completion(2), Some(Duration::from_secs(1000))));
    p.cancel();
    assert_eq!(
        p.evaluate(
            Some(Duration::from_secs(1060)),
            Some(&snapshot(1060, Activity::Locked)),
            true
        ),
        Decision::LocalOnly
    );
    p.completion(Completion(3), Some(Duration::from_secs(1000)));
    assert_eq!(
        p.evaluate(
            Some(Duration::from_secs(1060)),
            Some(&snapshot(1060, Activity::Locked)),
            false
        ),
        Decision::LocalOnly
    );
    p.completion(Completion(4), Some(Duration::from_secs(1000)));
    assert_eq!(p.evaluate(None, None, true), Decision::LocalOnly);
    p.completion(Completion(5), Some(Duration::from_secs(1000)));
    assert_eq!(
        p.evaluate(Some(Duration::from_secs(999)), None, true),
        Decision::LocalOnly
    );
    p.completion(Completion(6), Some(Duration::from_secs(1000)));
    assert_eq!(
        p.evaluate(
            Some(Duration::from_secs(1060)),
            Some(&snapshot(1060, Activity::Locked)),
            true
        ),
        Decision::Send
    );
    p.completion(Completion(7), Some(Duration::from_secs(1001)));
    assert_eq!(
        p.evaluate(
            Some(Duration::from_secs(1061)),
            Some(&snapshot(1061, Activity::Locked)),
            true
        ),
        Decision::LocalOnly
    );
}

struct Fake {
    time: AtomicU64,
    calls: AtomicUsize,
    hang: bool,
    activity: bool,
}

impl Clock for Fake {
    fn now(&self) -> Option<Duration> {
        Some(Duration::from_secs(self.time.load(Ordering::SeqCst)))
    }
}

impl ActivitySource for Fake {
    fn snapshot(&self) -> Pin<Box<dyn Future<Output = Option<Snapshot>> + Send + '_>> {
        Box::pin(async {
            self.activity
                .then(|| snapshot(self.time.load(Ordering::SeqCst), Activity::Locked))
        })
    }
}

impl Transport for Fake {
    fn available(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async { true })
    }

    fn send(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.hang {
                std::future::pending::<()>().await;
            }
            false
        })
    }
}

async fn advance(fake: &Fake, seconds: u64) {
    tokio::task::yield_now().await;
    fake.time.fetch_add(seconds, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(seconds)).await;
    tokio::task::yield_now().await;
}

#[tokio::test(start_paused = true)]
async fn dispatcher_dry_run_failure_timeout_and_input_cancellation() {
    for (mode, hang, cancel, activity, outcome, calls) in [
        (Mode::Disabled, false, false, true, None, 0),
        (
            Mode::DryRun,
            false,
            false,
            true,
            Some(Outcome::WouldNotify),
            0,
        ),
        (Mode::Send, false, false, true, Some(Outcome::Failed), 1),
        (Mode::Send, true, false, true, Some(Outcome::Failed), 1),
        (Mode::Send, false, true, true, None, 0),
        (Mode::Send, false, false, false, Some(Outcome::LocalOnly), 0),
    ] {
        let f = Arc::new(Fake {
            time: AtomicU64::new(1000),
            calls: AtomicUsize::new(0),
            hang,
            activity,
        });
        let mut d = Dispatcher::new(config(mode), f.clone(), f.clone(), f.clone()).unwrap();
        d.completion(Completion(1));
        d.completion(Completion(1));
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
        if cancel {
            d.cancel();
        }
        advance(&f, 60).await;
        if hang {
            advance(&f, 5).await;
        }
        assert_eq!(d.finished().await, outcome);
        advance(&f, 600).await;
        assert_eq!(f.calls.load(Ordering::SeqCst), calls);
    }
}

#[tokio::test(start_paused = true)]
async fn replacement_drop_and_inflight_cancel_do_not_retry() {
    let f = Arc::new(Fake {
        time: AtomicU64::new(1000),
        calls: AtomicUsize::new(0),
        hang: true,
        activity: true,
    });
    let mut d = Dispatcher::new(config(Mode::Send), f.clone(), f.clone(), f.clone()).unwrap();
    d.completion(Completion(1));
    advance(&f, 30).await;
    d.cancel();
    d.completion(Completion(2));
    advance(&f, 30).await;
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    advance(&f, 30).await;
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    d.cancel();
    advance(&f, 600).await;
    assert_eq!(d.finished().await, None);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    d.completion(Completion(3));
    drop(d);
    advance(&f, 60).await;
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

struct Unavailable;

impl Transport for Unavailable {
    fn available(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async { false })
    }
    #[expect(clippy::panic, reason = "the fixture must not be sent through")]
    fn send(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        panic!("unavailable transport was called")
    }
}

#[tokio::test(start_paused = true)]
async fn unavailable_transport_keeps_dry_run_local() {
    let f = Arc::new(Fake {
        time: AtomicU64::new(1000),
        calls: AtomicUsize::new(0),
        hang: false,
        activity: true,
    });
    let mut d = Dispatcher::new(
        config(Mode::DryRun),
        f.clone(),
        f.clone(),
        Arc::new(Unavailable),
    )
    .unwrap();
    d.completion(Completion(1));
    advance(&f, 60).await;
    assert_eq!(d.finished().await, Some(Outcome::LocalOnly));
}

struct TimedActivity {
    fake: Arc<Fake>,
    captured: AtomicUsize,
    delay: Duration,
}

impl ActivitySource for TimedActivity {
    fn snapshot(&self) -> Pin<Box<dyn Future<Output = Option<Snapshot>> + Send + '_>> {
        Box::pin(async {
            self.captured.fetch_add(1, Ordering::SeqCst);
            let observed_at = self.fake.now()?;
            tokio::time::sleep(self.delay).await;
            Some(Snapshot {
                observed_at,
                sessions: vec![SessionActivity {
                    local: true,
                    activity: Activity::Locked,
                }],
            })
        })
    }
}

#[tokio::test(start_paused = true)]
async fn capture_follows_grace_and_slow_collection_cannot_refresh_its_timestamp() {
    for delay in [0, 6] {
        let fake = Arc::new(Fake {
            time: AtomicU64::new(1000),
            calls: AtomicUsize::new(0),
            hang: false,
            activity: true,
        });
        let activity = Arc::new(TimedActivity {
            fake: fake.clone(),
            captured: AtomicUsize::new(0),
            delay: Duration::from_secs(delay),
        });
        let mut d = Dispatcher::new(
            config(Mode::DryRun),
            fake.clone(),
            activity.clone(),
            fake.clone(),
        )
        .unwrap();
        d.completion(Completion(1));
        advance(&fake, 59).await;
        assert_eq!(activity.captured.load(Ordering::SeqCst), 0);
        advance(&fake, 1).await;
        assert_eq!(activity.captured.load(Ordering::SeqCst), 1);
        advance(&fake, delay).await;
        assert_eq!(
            d.finished().await,
            Some(if delay == 0 {
                Outcome::WouldNotify
            } else {
                Outcome::LocalOnly
            })
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn snapshot_before_grace_is_rejected_even_when_fresh() {
    let mut c = config(Mode::DryRun);
    c.grace_seconds = 1;
    let mut p = Policy::new(c).unwrap();
    p.completion(Completion(1), Some(Duration::from_secs(1000)));
    assert_eq!(
        p.evaluate(
            Some(Duration::from_secs(1001)),
            Some(&snapshot(1000, Activity::Locked)),
            true
        ),
        Decision::LocalOnly
    );
}
