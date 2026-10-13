use std::time::Duration;

use mush_sync::{Opened, checkout};

use crate::harness::{machine, server_at};

#[tokio::test]
async fn taking_a_session_over_tells_the_open_checkout_at_once() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("contested", &["start"]);
    dot.sync().await;
    esme.sync().await;
    let lease = dot.store().acquire(&id).unwrap();
    let Opened::Claimed(open) =
        checkout(dot.engine.clone(), &lease, Duration::from_secs(60), false)
            .await
            .unwrap()
    else {
        panic!("dot reaches the server");
    };
    let mut state = open.state();
    assert!(!state.borrow().read_only());

    let started = std::time::Instant::now();
    assert_eq!(
        esme.engine
            .take(id.as_str(), Duration::from_secs(60))
            .await
            .unwrap()
            .as_deref(),
        Some("dot")
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        state.wait_for(mush_session::SyncState::read_only),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(state.borrow().taken_by.as_deref(), Some("esme"));
    assert!(started.elapsed() < Duration::from_secs(3));
    open.finish().await;
}
