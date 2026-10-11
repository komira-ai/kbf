//! The node's phase in `Hello` (`docs/design/worker-protocol.md#node-phases`): a
//! runtime that switches a Mac's GUI session hands the daemon its phase; each Hello
//! carries the newest and a change resends the Hello. A lease the server no longer
//! holds, or one of another epoch, that the runtime still switches for is killed on
//! every runtime, which is how the whole-machine runtime learns to clean up after it.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError, Work};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{
    Hello, LeaseId as WireLease, NodePhase as WirePhase, Phase, daemon_message,
};
use kbf_types::{LeaseId, NodePhase};
use support::{Harness, PROMPT, Peer};
use tokio::sync::watch;

/// A runtime that runs nothing and records each kill, running or not.
#[derive(Debug, Default)]
struct Kills(Mutex<Vec<LeaseId>>);

impl Kills {
    fn killed(&self) -> Vec<LeaseId> {
        self.0.lock().expect("kills").clone()
    }

    async fn until(&self, lease: LeaseId) {
        let deadline = tokio::time::Instant::now() + PROMPT;
        while !self.killed().contains(&lease) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{lease} was never killed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Runtime for Kills {
    fn driver(&self) -> &'static str {
        "native-whole-machine"
    }

    fn serves(&self, kind: &str) -> bool {
        kind == "whole_machine"
    }

    async fn run(&self, _: Work) -> Result<ActionResult, RuntimeError> {
        std::future::pending().await
    }

    async fn kill(&self, lease_id: LeaseId) {
        self.0.lock().expect("kills").push(lease_id);
    }
}

fn wire(phase: Phase, lease: Option<(u64, u64)>, reason: &str) -> Option<WirePhase> {
    Some(WirePhase {
        phase: phase.into(),
        lease_id: lease.map(|(term, seq)| WireLease { term, seq }),
        reason: reason.to_owned(),
    })
}

/// The next resent Hello on `peer`, within `within`.
async fn resent(peer: &mut Peer, within: Duration) -> Option<Hello> {
    peer.expect(within, |m| match m {
        daemon_message::Message::Hello(h) => Some(h.clone()),
        _ => None,
    })
    .await
    .map(|(_, hello)| hello)
}

/// Catches a phase that never reaches the server (in the first Hello of a stream, or
/// at all), a change that is not resent at once (the server would place work on a Mac
/// whose session is switching, or keep placing on a quarantined one), a resend for a
/// value that did not change, a stream after a reconnect that names an old phase, and
/// serving sent as anything but no phase (what a daemon that predates the field sends).
#[tokio::test]
async fn every_hello_carries_the_phase_and_a_change_resends_it() {
    let (phase, rx) = watch::channel(NodePhase::Preparing(LeaseId::new(1, 5)));
    let mut h = Harness::with_phase("phase-hello", Arc::new(Kills::default()), rx).await;
    let mut peer = h.session().await;
    assert_eq!(
        peer.hello().await.phase,
        wire(Phase::Preparing, Some((1, 5)), "")
    );
    peer.welcome();

    phase.send_replace(NodePhase::Restoring(LeaseId::new(1, 5)));
    let hello = resent(&mut peer, PROMPT).await.expect("resent");
    assert_eq!(hello.phase, wire(Phase::Restoring, Some((1, 5)), ""));
    phase.send_replace(NodePhase::Restoring(LeaseId::new(1, 5)));
    assert_eq!(resent(&mut peer, Duration::from_millis(300)).await, None);
    phase.send_replace(NodePhase::Quarantined("user-delete failed".to_owned()));
    let hello = resent(&mut peer, PROMPT).await.expect("resent");
    assert_eq!(
        hello.phase,
        wire(Phase::Quarantined, None, "user-delete failed")
    );

    peer.close();
    phase.send_replace(NodePhase::Serving);
    let mut next = h.session().await;
    assert_eq!(next.hello().await.phase, None);
}

/// Catches a `Cancel` for a lease that is not running here that is dropped (the
/// whole-machine runtime keeps such a lease across its restart and never cleans up),
/// and a Welcome of another lease epoch than the switching lease's term that leaves it
/// alone (that server never granted it, so no `Start` for it will come). A Welcome of
/// the lease's own epoch kills nothing.
#[tokio::test]
async fn a_lease_the_server_does_not_hold_is_killed_on_every_runtime() {
    let kills = Arc::new(Kills::default());
    let (_phase, rx) = watch::channel(NodePhase::Preparing(LeaseId::new(5, 1)));
    let mut h = Harness::with_phase("phase-cancel", Arc::clone(&kills), rx).await;
    let mut peer = h.session().await;
    peer.hello().await;
    peer.welcome_epoch(5);
    peer.heartbeat().await;
    assert_eq!(kills.killed(), []);
    peer.cancel(Some((5, 2)));
    kills.until(LeaseId::new(5, 2)).await;

    peer.close();
    let mut next = h.session().await;
    next.hello().await;
    next.welcome_epoch(6);
    kills.until(LeaseId::new(5, 1)).await;
}
