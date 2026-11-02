//! A halt report counts only from a host the service counts, and a halted service is neither
//! rolled back nor advanced. Every time is explicit.

use kbf_updater::wire::{Heartbeat, ServiceBeat};
use serde_json::json;

use crate::decide::Hold;
use crate::desired::KBF;
use crate::membership_tests::{DAEMONS, V2, ZONE_A, labels, rig, who};
use crate::rollback_tests::{
    AUTO, ONE, TWO, marks, restart, rig as rollback_rig, round, seed, status, two_failed, wanted,
};
use crate::testutil::{Rig, beat, version};

/// The heartbeat of a host that lists `other` at 1.0 and reports that its update to 2.0
/// halted.
fn halting(v2: bool) -> Heartbeat {
    let mut b = beat("1.0");
    if v2 {
        b.agent_features = vec![V2.to_owned()];
    }
    b.services = Some(
        serde_json::from_value::<Vec<ServiceBeat>>(json!([{
            "name": "other", "version": "1.0", "health": "healthy",
            "last_update": {"target": "2.0", "outcome": "failed", "phase": "halt",
                "reason": "disk_tainted"}
        }]))
        .expect("beats"),
    );
    b
}

fn halted_by(rig: &Rig) -> Option<String> {
    let status = rig
        .coordinator
        .status(Some(crate::testutil::OPERATOR))
        .expect("status");
    status
        .ops
        .services
        .get("other")
        .and_then(|m| m.halt.as_ref())
        .map(|halt| halt.node_id.clone())
}

/// Catches: a host the selector leaves out raising a halt that stops the whole service,
/// though its word about the service is not recorded.
#[tokio::test]
async fn a_halt_report_from_a_host_the_selector_drops_raises_nothing() {
    let rig = rig("halt-mem-dropped", ZONE_A, &["other"]).await;
    rig.coordinator.heartbeat(who("d1"), halting(true));
    assert_eq!(halted_by(&rig), None);
}

/// Catches: a host that cannot list services (not counted, so not recorded) raising a halt.
#[tokio::test]
async fn a_halt_report_from_an_unmanaged_host_raises_nothing() {
    let rig = rig("halt-mem-unmanaged", DAEMONS, &["other"]).await;
    rig.coordinator.heartbeat(who("d1"), halting(false));
    assert_eq!(halted_by(&rig), None);
}

/// The same report from a host the selector picks halts the service (arrives green: it pins
/// behaviour that exists).
#[tokio::test]
async fn the_same_halt_report_from_a_counted_host_halts_the_service() {
    let rig = rig("halt-mem-counted", ZONE_A, &["other"]).await;
    labels(&rig, "d1", r#"{"zone":"a"}"#);
    rig.coordinator.heartbeat(who("d1"), halting(true));
    assert_eq!(halted_by(&rig), Some("d1".to_owned()));
}

/// A halted service whose desired version is also marked bad answers `halted` to every host,
/// not `bad`, and nothing is ordered (arrives green: the halt gate is checked first).
#[tokio::test]
async fn a_halted_service_with_a_bad_desired_version_holds_at_halted() {
    let mut rig = rollback_rig("halt-mem-bad", AUTO, ONE).await;
    round(&rig, 2_000, &[]);
    rig.set(TWO).await;
    seed(&rig, |m| {
        m.halt = Some(crate::ops::Halt {
            node_id: "d1".to_owned(),
            check: "tainted".to_owned(),
            target: version("2.0"),
            report: None,
        });
        m.bad = vec![version("2.0")];
    });
    restart(&mut rig, AUTO);
    let replies = round(&rig, 2_100, &[]);
    assert_eq!(
        replies.last(),
        Some(&kbf_updater::wire::ServiceReply::Nothing)
    );
    for host in &status(&rig).per_service[KBF].hosts {
        assert_eq!(host.hold, Some(Hold::Halted), "{}", host.host.node_id);
    }
}

/// A halted service with failures past the threshold marks nothing bad and keeps its desired
/// version (arrives green: the rollback plan returns nothing while a halt stands).
#[tokio::test]
async fn a_halted_service_with_release_failures_marks_nothing_bad() {
    let mut rig = rollback_rig("halt-mem-nobad", AUTO, ONE).await;
    round(&rig, 2_000, &[]);
    rig.set(TWO).await;
    seed(&rig, |m| {
        m.halt = Some(crate::ops::Halt {
            node_id: "d1".to_owned(),
            check: "tainted".to_owned(),
            target: version("2.0"),
            report: None,
        });
    });
    restart(&mut rig, AUTO);
    round(&rig, 2_100, &[]);
    round(&rig, 2_115, &two_failed());
    assert!(marks(&rig).bad.is_empty());
    assert_eq!(wanted(&rig), version("2.0"));
}
