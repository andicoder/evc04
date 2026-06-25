//! Integration tests for the resilient gateway link (SPECS.md §7).

use evc04_charge::slave::{run_link, LinkConfig, LinkHealth, PollMatch};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::time::timeout;

fn zero_amps() -> [f32; 3] {
    [0.0, 0.0, 0.0]
}

fn fast_config() -> LinkConfig {
    LinkConfig {
        poll: PollMatch::default(),
        stall_timeout: Duration::from_millis(100),
        backoff_initial: Duration::from_millis(20),
        backoff_max: Duration::from_millis(100),
    }
}

#[tokio::test]
async fn reconnects_after_the_link_drops() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, _rx) = watch::channel(LinkHealth::Down);
    let task = tokio::spawn(run_link(addr, fast_config(), zero_amps, tx));

    // Accept the first connection, then drop it to simulate a gateway dropout.
    let (first, _) = listener.accept().await.unwrap();
    drop(first);

    // The supervisor must dial out again.
    let reconnected = timeout(Duration::from_secs(2), listener.accept()).await;
    assert!(
        reconnected.is_ok(),
        "expected a reconnect attempt after the link dropped"
    );
    task.abort();
}

#[tokio::test]
async fn flags_stalled_link_when_polls_go_silent() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = watch::channel(LinkHealth::Down);
    let task = tokio::spawn(run_link(addr, fast_config(), zero_amps, tx));

    // Hold the connection open but never send a poll.
    let (_held, _) = listener.accept().await.unwrap();

    let saw_stalled = timeout(Duration::from_secs(2), async {
        loop {
            rx.changed().await.unwrap();
            if *rx.borrow() == LinkHealth::Stalled {
                break;
            }
        }
    })
    .await;
    assert!(
        saw_stalled.is_ok(),
        "watchdog should flag the link as stalled after poll silence"
    );
    task.abort();
}
