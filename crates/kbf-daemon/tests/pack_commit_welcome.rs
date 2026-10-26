//! A daemon given a `PackCommit` ([`kbf_daemon::Daemon::with_pack_writes`]) sets it from every
//! Welcome: on when the frontend says it takes pack commits, off when a later one does not.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Event, FakeRuntime, PackCommit};
use kbf_proto::worker::{Welcome, server_message};
use support::{Harness, PROMPT};

/// Asserts that the handle is off until a Welcome says the frontend takes commits, on after
/// it, and off again after the Welcome of a later stream that does not say it (a reconnect to
/// another frontend); a Welcome of an old frontend, which has no such field, is the latter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_welcome_sets_the_pack_commit_handle() {
    let commit = PackCommit::new();
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let mut h = Harness::with_pack_commit("pack-commit", runtime, commit.clone()).await;
    let mut first = h.session().await;
    first.hello().await;
    assert!(!commit.enabled(), "off before any Welcome");
    first.welcome_pack_commit(true);
    welcomed(&mut h).await;
    assert!(commit.enabled());

    first.close();
    let mut second = h.session().await;
    second.hello().await;
    second.welcome();
    welcomed(&mut h).await;
    assert!(!commit.enabled());
}

/// Waits for the daemon to take a Welcome.
async fn welcomed(h: &mut Harness<FakeRuntime>) {
    let seen = h
        .event(PROMPT, |e| {
            matches!(e, Event::Welcomed { .. }).then_some(())
        })
        .await;
    assert!(seen.is_some(), "a Welcome was taken");
}

/// Asserts that a Welcome the daemon refuses does not turn the handle on, though it says the
/// frontend takes commits: one of an unacceptable protocol version, and one with a heartbeat
/// interval of zero. The daemon opens another stream after each refusal, which is how the test
/// knows the Welcome was judged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_welcome_leaves_the_pack_commit_handle_alone() {
    let commit = PackCommit::new();
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let mut h = Harness::with_pack_commit("pack-commit-refused", runtime, commit.clone()).await;
    let mut peer = h.session().await;
    peer.hello().await;
    for (protocol_version, heartbeat_interval_ms) in [(2, 100), (1, 0)] {
        peer.send(server_message::Message::Welcome(Welcome {
            protocol_version,
            heartbeat_interval_ms,
            pack_commit: true,
            ..Welcome::default()
        }));
        peer = h.session().await;
        peer.hello().await;
        assert!(!commit.enabled(), "version {protocol_version}");
    }
}
