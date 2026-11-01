//! The start of a frontend with a state store reads the deny rows once before any listener
//! serves, so the worker listener does not refuse every daemon until the sweeper's first beat.

use std::sync::Arc;
use std::time::Duration;

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Cond, ManualClock, Store, Txn};
use tonic::Code;

use crate::identity::{PeerCert, Peers};
use crate::members::MembershipConfig;
use crate::serve::members_of;
use crate::{EpochPolicy, StateStore};

fn state(fake: &Arc<Fake>) -> StateStore {
    StateStore {
        store: Arc::clone(fake) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: MembershipConfig {
            ttl: Duration::from_secs(60),
            dwell: Duration::from_secs(60),
            replication: 2,
        },
        history: None,
    }
}

fn fake() -> Arc<Fake> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

fn peer(node: &str) -> PeerCert {
    PeerCert::proxied(node, &[])
}

/// Right after the members are made, with no sweeper running, a daemon is admitted and a
/// node whose deny row is already in the store is refused.
///
/// Catches: a start that leaves the first read of the deny rows to the sweeper, so that every
/// daemon that connects before its first beat (a whole beat if that refresh fails) is refused
/// UNAVAILABLE at every frontend restart.
#[tokio::test]
async fn the_start_reads_the_deny_rows_before_any_listener_serves() {
    let fake = fake();
    let first = members_of(Some(&state(&fake)), None).expect("members");
    first
        .deny(&"node node-a".parse().expect("entry"), "revoked")
        .expect("deny");
    // A restarted frontend over the same store.
    let restarted = members_of(Some(&state(&fake)), None).expect("members");
    let peers = Peers::Certified {
        deny_list: None,
        denied_rows: None,
    }
    .with_denied_rows(Some(restarted));
    assert!(peers.admit(Some(&peer("node-b")), "node-b").await.is_ok());
    let refused = peers
        .admit(Some(&peer("node-a")), "node-a")
        .await
        .expect_err("revoked before the restart");
    assert_eq!(refused.code(), Code::PermissionDenied);
}

/// A first refresh that fails does not stop the start: the members come back, and the
/// bounded-staleness rule refuses the callers until a refresh reads the rows.
#[tokio::test]
async fn a_failing_first_refresh_does_not_stop_the_start() {
    let fake = fake();
    let written = fake
        .txn(Txn::new().put("deny/bogus/x", b"?".to_vec(), Cond::Any))
        .expect("write the row");
    assert!(written.is_ok());
    let members = members_of(Some(&state(&fake)), None).expect("the start goes on");
    assert!(members.denied_snapshot().is_none());
    let peers = Peers::Certified {
        deny_list: None,
        denied_rows: None,
    }
    .with_denied_rows(Some(members));
    let refused = peers
        .admit(Some(&peer("node-b")), "node-b")
        .await
        .expect_err("nobody is admitted yet");
    assert_eq!(refused.code(), Code::Unavailable);
}

/// A start with `--exclusive-actions` on says so, and a start with it off says nothing
/// about it: an operator reading the start-up log can tell a farm that books a whole
/// worker per action from one that does not. Catches a line that is missing when on or
/// printed when off.
#[test]
fn a_start_logs_exclusive_actions_only_when_on() {
    let off = crate::test_log::capture();
    crate::serve::log_exclusive(false);
    assert!(!off.text().contains("exclusive-actions"));
    drop(off);
    let on = crate::test_log::capture();
    crate::serve::log_exclusive(true);
    assert!(on.text().contains(crate::serve::EXCLUSIVE_LINE));
    assert_eq!(crate::serve::EXCLUSIVE_LINE, "exclusive-actions: on");
}
