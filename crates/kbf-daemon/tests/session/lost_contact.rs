//! A daemon whose server stops acknowledging its heartbeats fences its leases and
//! opens a new stream with a new Hello; a fence never stops the session loop while a
//! killed run takes its time to stop (a run in the middle of a slow output upload
//! does not see the kill until the upload ends). Each test runs real mutual TLS on
//! loopback against the fake worker service of `support`.
//!
//! A module of the `session` test binary: these are tests of the session loop.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::support::{Harness, PROMPT};
use kbf_daemon::{Event, Runtime, RuntimeError, Work};
use kbf_proto::google::rpc::Code;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{LeaseId, Lost};
use tokio::sync::Semaphore;

const LONG: Duration = Duration::from_secs(60);

/// T for these tests: short, and many heartbeat intervals long.
const T: Duration = Duration::from_millis(800);

fn lease(term: u64, seq: u64) -> Option<LeaseId> {
    Some(LeaseId { term, seq })
}

/// A runtime whose runs, once killed, stop only when the test lets them: as a run in
/// the middle of uploading large outputs to a slow store does, whose kill is seen
/// only once the upload ends.
struct SlowStop {
    /// One permit per run allowed to stop.
    stop: Semaphore,
    killed: Mutex<Vec<kbf_types::LeaseId>>,
}

impl Runtime for SlowStop {
    fn driver(&self) -> &'static str {
        "slow-stop"
    }

    fn serves(&self, _kind: &str) -> bool {
        true
    }

    async fn run(&self, _work: Work) -> Result<ActionResult, RuntimeError> {
        self.stop.acquire().await.expect("open").forget();
        Err(RuntimeError::Killed)
    }

    async fn kill(&self, lease_id: kbf_types::LeaseId) {
        // The run stops when the test adds its permit, not here.
        self.killed.lock().expect("lock").push(lease_id);
    }
}

/// Catches (a production wedge): a daemon that, once its server stops acknowledging
/// heartbeats, keeps the stream it has for ever. Its leases are fenced, but it never
/// says Hello again, so a server that gave its leases up, or whose stream is stuck,
/// never hears from it again. After T without an acknowledgement the daemon must open
/// a new stream and say Hello on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_without_acknowledgements_says_hello_again() {
    let mut h = Harness::start("silent-rehello", LONG, T).await;
    let peer = h.welcomed().await;
    peer.acking.store(false, Ordering::SeqCst);

    let mut next = h.session().await;
    let hello = next.hello().await;
    assert!(!hello.node_id.is_empty());
    next.welcome();
    // The new stream serves: its heartbeats are acknowledged and it takes work.
    next.start(1, 7, "action");
    h.started(1).await;
    assert_eq!(h.runtime.started(), [kbf_types::LeaseId::new(1, 7)]);
}

/// Catches (the same wedge, its other half): a fence that waits, inside the session
/// loop, for every killed run to stop. A run whose kill is seen only after a long
/// upload then stops the daemon dead: no heartbeat, no message read, no new stream,
/// for as long as the upload takes. The daemon must open a new stream while the run is
/// still stopping, list the lease there until it has stopped, and then report it
/// fenced (lost), not as the run ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_kill_does_not_stop_the_daemon() {
    let runtime = Arc::new(SlowStop {
        stop: Semaphore::new(0),
        killed: Mutex::new(Vec::new()),
    });
    let mut h = Harness::with_runtime("slow-kill", Arc::clone(&runtime), T).await;
    let peer = h.welcomed().await;
    peer.start(1, 3, "action");
    peer.acking.store(false, Ordering::SeqCst);

    h.event(T + PROMPT, |e| matches!(e, Event::Fenced(_)).then_some(()))
        .await
        .expect("the daemon fences");
    let mut next = h.session().await;
    next.hello().await;
    next.welcome();
    let beat = next.heartbeat().await;
    assert_eq!(
        beat.running,
        [LeaseId { term: 1, seq: 3 }],
        "the stopping lease is listed"
    );
    assert_eq!(
        *runtime.killed.lock().expect("lock"),
        [kbf_types::LeaseId::new(1, 3)]
    );
    assert_eq!(
        next.result(Duration::from_millis(300)).await,
        None,
        "reported before it stopped"
    );

    runtime.stop.add_permits(1);
    let (_, result) = next.result(PROMPT).await.expect("the fenced Result");
    assert_eq!(result.lease_id, lease(1, 3));
    assert_eq!(
        result.status.as_ref().map(|s| s.code),
        Some(Code::Unavailable as i32)
    );
    assert_eq!(result.lost(), Lost::Fenced);
}
