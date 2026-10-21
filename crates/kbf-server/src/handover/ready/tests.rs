//! The frontend telling the service manager it serves, against a manager the test plays.

use std::os::unix::net::UnixDatagram;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures::future::BoxFuture;

use super::*;
use crate::handover::tests::Scratch;
use crate::handover::wire::Message;
use crate::health::{Probe, Readiness};

/// A dependency that fails until the test lets it pass, and counts being asked.
struct Dependency {
    up: Arc<AtomicBool>,
    asked: Arc<AtomicUsize>,
}

impl Probe for Dependency {
    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let up = self.up.load(Ordering::SeqCst);
        Box::pin(async move { up.then_some(()).ok_or_else(|| "not up".to_owned()) })
    }
}

/// Catches: `READY=1` sent before the frontend's probes pass (the service manager would
/// start the next unit, or the updater would think the new process serves, while the
/// storage node or state store is not reachable), never sent once they do, and a manager
/// that is not there (no `$NOTIFY_SOCKET`, or a socket nobody listens on) stopping the
/// frontend from running.
#[tokio::test]
async fn ready_waits_for_the_probes_then_tells_the_manager() {
    let dir = Scratch::new();
    let path = dir.0.join("notify");
    let manager = UnixDatagram::bind(&path).expect("a datagram socket");
    manager.set_nonblocking(true).expect("nonblocking");
    let up = Arc::new(AtomicBool::new(false));
    let asked = Arc::new(AtomicUsize::new(0));
    let dependency = Dependency {
        up: Arc::clone(&up),
        asked: Arc::clone(&asked),
    };
    let checks = Arc::new(
        Checks::new(Arc::new(Readiness::default())).probing("dependency", Arc::new(dependency)),
    );
    let ready = Ready {
        checks: Arc::clone(&checks),
        notify: Some(path.clone().into_os_string()),
        predecessor: None,
    };
    let task = tokio::spawn(ready.run());
    // Asked twice while the dependency is down: the first failure was followed by a wait.
    while asked.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    let mut buffer = [0_u8; 32];
    assert!(
        manager.recv(&mut buffer).is_err(),
        "ready sent while a probe fails"
    );
    up.store(true, Ordering::SeqCst);
    task.await.expect("ready ends once the probes pass");
    let got = manager.recv(&mut buffer).expect("READY=1");
    assert_eq!(&buffer[..got], b"READY=1");

    for notify in [None, Some(dir.0.join("nobody-listens").into_os_string())] {
        Ready {
            checks: Arc::clone(&checks),
            notify,
            predecessor: None,
        }
        .run()
        .await;
    }
}

/// Catches: a successor that tells its predecessor it serves before its probes pass (the
/// predecessor would leave Envoy's rotation while the successor cannot answer), never
/// tells it once they do (the predecessor waits for its watchdog and leaves anyway), or
/// stops running because the predecessor is gone.
#[tokio::test]
async fn the_predecessor_is_told_once_the_probes_pass() {
    let up = Arc::new(AtomicBool::new(false));
    let asked = Arc::new(AtomicUsize::new(0));
    let dependency = Dependency {
        up: Arc::clone(&up),
        asked: Arc::clone(&asked),
    };
    let checks = Arc::new(
        Checks::new(Arc::new(Readiness::default())).probing("dependency", Arc::new(dependency)),
    );
    let (ours, theirs) = tokio::net::UnixStream::pair().expect("a pair");
    let mut predecessor = crate::handover::wire::Link::new(theirs);
    let task = tokio::spawn(
        Ready {
            checks: Arc::clone(&checks),
            notify: None,
            predecessor: Some(Released::over(crate::handover::wire::Link::new(ours))),
        }
        .run(),
    );
    while asked.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    let early = tokio::time::timeout(Duration::from_millis(50), predecessor.hear()).await;
    assert!(early.is_err(), "told it serves while a probe fails");
    up.store(true, Ordering::SeqCst);
    assert_eq!(predecessor.hear().await.expect("told"), Message::Serving);
    task.await.expect("ready ends");

    // A predecessor that has gone is not an error that stops anything.
    let (ours, theirs) = tokio::net::UnixStream::pair().expect("a pair");
    drop(theirs);
    Ready {
        checks,
        notify: None,
        predecessor: Some(Released::over(crate::handover::wire::Link::new(ours))),
    }
    .run()
    .await;
}

/// Catches: the frontend waiting on the `storage` probe for `READY=1` and for the
/// predecessor's "serving": with only the storage probe failing both are sent; with
/// another probe (the state store) failing neither is, until it passes. Mutant: `run`
/// asks `failing` (every probe) again.
#[tokio::test]
async fn ready_does_not_wait_for_storage_but_waits_for_the_rest() {
    let dir = Scratch::new();
    let path = dir.0.join("notify");
    let manager = UnixDatagram::bind(&path).expect("a datagram socket");
    manager.set_nonblocking(true).expect("nonblocking");
    let state_up = Arc::new(AtomicBool::new(false));
    let asked = Arc::new(AtomicUsize::new(0));
    let checks = Arc::new(
        Checks::new(Arc::new(Readiness::default()))
            .probing(
                "storage",
                Arc::new(Dependency {
                    up: Arc::new(AtomicBool::new(false)),
                    asked: Arc::new(AtomicUsize::new(0)),
                }),
            )
            .probing(
                "state_store",
                Arc::new(Dependency {
                    up: Arc::clone(&state_up),
                    asked: Arc::clone(&asked),
                }),
            ),
    );
    let task = tokio::spawn(
        Ready {
            checks,
            notify: Some(path.into_os_string()),
            predecessor: None,
        }
        .run(),
    );
    while asked.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    let mut buffer = [0_u8; 32];
    assert!(
        manager.recv(&mut buffer).is_err(),
        "ready sent with the state store down"
    );
    state_up.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("ready waited on the storage probe")
        .expect("ready ends with storage still down");
    let got = manager.recv(&mut buffer).expect("READY=1");
    assert_eq!(&buffer[..got], b"READY=1");
}
