//! The floor `min_version` sets under a rollback, and the failures a by-hand set of a bad
//! version acknowledges: an override of the coordinator is not undone by the reports of the
//! failure it overrode, and a new failure counts again.

use std::time::Duration;

use kbf_types::HostRole;
use kbf_updater::wire::{Class, Outcome, Phase, ServiceReply, Token, UpdateResult};

use crate::fleet::Fleet;
use crate::ops::{FailureAck, OpsState, ServiceOps};
use crate::reconcile::{Change, plan};
use crate::rollback_tests::{
    AUTO, ONE, THREE, TWO, def, failure, farm, good, marks, persisted, plan_for, release, restart,
    rig, rig_over, round, secs, seed, two_failed, unwritable, wanted,
};
use crate::testutil::{beat, desired, id, version};

/// The ack of the failure `node` reported: the whole report.
fn ack_of(node: &str, phase: Option<Phase>, class: Option<Class>, target: &str) -> FailureAck {
    FailureAck {
        node_id: node.to_owned(),
        report: UpdateResult {
            target: version(target),
            outcome: Outcome::Failed,
            phase,
            class,
            reason: None,
        },
    }
}

/// The ack of a failure after the commit that the release is blamed for.
fn ack(node: &str, target: &str) -> FailureAck {
    ack_of(node, Some(Phase::AfterCommit), Some(Class::Release), target)
}

/// The marks of a service whose last good version is `last_good`.
fn last_good(last_good: &str) -> ServiceOps {
    ServiceOps {
        last_good: Some(version(last_good)),
        ..ServiceOps::default()
    }
}

// ---- min_version is a floor --------------------------------------------------------------

/// A last good version below `min_version` is no rollback target, even with the desired
/// version above the floor: the failed version is marked bad and the desired state stays. At
/// the floor it is a target, and a version already marked is not marked twice.
#[test]
fn a_last_good_below_min_version_is_never_the_rollback_target() {
    let (phase, class) = release();
    let floored = def(&format!("{AUTO}\nmin_version = \"1.5\""));
    let fleet = farm(6, 2, phase, class);
    assert_eq!(
        plan_for(&floored, &last_good("1.0"), &fleet),
        vec![Change::MarkBad(version("2.0"))]
    );
    assert_eq!(
        plan_for(&floored, &last_good("1.5"), &fleet),
        vec![
            Change::MarkBad(version("2.0")),
            Change::RollBackTo(version("1.5"))
        ]
    );
    assert_eq!(
        plan_for(&floored, &last_good("1.9"), &fleet),
        vec![
            Change::MarkBad(version("2.0")),
            Change::RollBackTo(version("1.9"))
        ]
    );
    let marked = ServiceOps {
        bad: vec![version("2.0")],
        ..last_good("1.0")
    };
    assert_eq!(plan_for(&floored, &marked, &fleet), vec![]);
    assert_eq!(
        plan_for(&floored, &last_good("1.0"), &farm(6, 1, phase, class)),
        vec![],
        "below the threshold nothing is marked"
    );
}

/// The floor compares the numbers of the version, not its text: 0.1.9 is below 0.1.10 and a
/// build suffix does not matter.
#[test]
fn min_version_is_compared_by_number_and_not_by_text() {
    let (phase, class) = release();
    let floored = def(&format!("{AUTO}\nmin_version = \"0.1.10\""));
    let fleet = farm(6, 2, phase, class);
    assert_eq!(
        plan_for(&floored, &last_good("0.1.9+g0123456789ab-7"), &fleet),
        vec![Change::MarkBad(version("2.0"))]
    );
    assert_eq!(
        plan_for(&floored, &last_good("0.1.10+g0123456789ab-8"), &fleet),
        vec![
            Change::MarkBad(version("2.0")),
            Change::RollBackTo(version("0.1.10+g0123456789ab-8"))
        ]
    );
    assert_eq!(
        plan_for(&floored, &last_good("0.2"), &fleet),
        vec![
            Change::MarkBad(version("2.0")),
            Change::RollBackTo(version("0.2"))
        ]
    );
}

// ---- acked failures ----------------------------------------------------------------------

/// Failure reports an operator override acked do not count: with two of six acked, two more
/// failures are needed, and one is not enough. An ack that no longer matches its host's report
/// is dropped by the plan.
#[test]
fn an_acked_failure_does_not_count_and_a_stale_ack_is_dropped() {
    let (phase, class) = release();
    let acked = ServiceOps {
        failure_acked: vec![ack("d0", "2.0"), ack("d1", "2.0")],
        ..good()
    };
    let automatic = def(AUTO);
    assert_eq!(
        plan_for(&automatic, &acked, &farm(6, 2, phase, class)),
        vec![]
    );
    assert_eq!(
        plan_for(&automatic, &acked, &farm(6, 3, phase, class)),
        vec![]
    );
    assert_eq!(
        plan_for(&automatic, &acked, &farm(6, 4, phase, class)),
        vec![
            Change::MarkBad(version("2.0")),
            Change::RollBackTo(version("1.0"))
        ]
    );

    // d0 reports another update now, d1 still the failure, d9 is not known at all.
    let mut fleet = Fleet::new(Duration::ZERO);
    fleet.record(id("d0"), HostRole::DAEMON, beat("1.0"), secs(2_000));
    fleet.record(
        id("d1"),
        HostRole::DAEMON,
        failure(phase, class, "2.0"),
        secs(2_000),
    );
    let stale = ServiceOps {
        failure_acked: vec![ack("d0", "2.0"), ack("d1", "2.0"), ack("d9", "2.0")],
        ..good()
    };
    assert_eq!(
        plan(
            &automatic,
            &desired("2.0", 0, 0),
            &stale,
            &fleet,
            secs(2_000)
        ),
        vec![Change::DropAck("d0".to_owned())]
    );
}

/// The roll-back, then CI sets the bad version again by hand while the hosts that failed
/// still report it: the failures were acknowledged, so the next beats do not roll it back,
/// the mark is gone, and the acks are in the file and survive a restart.
#[tokio::test]
async fn re_setting_a_bad_version_by_hand_is_not_rolled_back_by_the_old_failures() {
    let mut rig = rig("ov-reset", AUTO, ONE).await;
    round(&rig, 2_000, &[]);
    rig.set(TWO).await;
    round(&rig, 2_030, &two_failed());
    assert_eq!(wanted(&rig), version("1.0"));
    assert_eq!(marks(&rig).bad, vec![version("2.0")]);

    rig.set(TWO).await;
    assert!(marks(&rig).bad.is_empty());
    assert_eq!(
        persisted(&rig).failure_acked,
        vec![
            ack("d1", "2.0"),
            ack_of("d2", Some(Phase::GateRelease), None, "2.0")
        ]
    );

    round(&rig, 2_045, &two_failed());
    assert_eq!(wanted(&rig), version("2.0"), "the override stands");
    assert!(marks(&rig).bad.is_empty());

    restart(&mut rig, AUTO);
    round(&rig, 2_100, &two_failed());
    round(&rig, 2_115, &two_failed());
    assert_eq!(wanted(&rig), version("2.0"), "and after a restart");
    assert!(marks(&rig).bad.is_empty());
}

/// After the override, failures of hosts that had not failed count as before: one new one
/// beside the two acked is below the threshold, two are at it.
#[tokio::test]
async fn a_new_failure_after_the_override_counts_again() {
    let rig = rig("ov-new", AUTO, ONE).await;
    round(&rig, 2_000, &[]);
    rig.set(TWO).await;
    round(&rig, 2_030, &two_failed());
    rig.set(TWO).await;

    let (phase, class) = release();
    let mut three = two_failed();
    three.push(("d3", failure(phase, class, "2.0")));
    round(&rig, 2_045, &three);
    assert_eq!(wanted(&rig), version("2.0"), "one new failure");

    let mut four = three.clone();
    four.push(("d4", failure(Some(Phase::AfterCommit), None, "2.0")));
    round(&rig, 2_060, &four);
    assert_eq!(wanted(&rig), version("1.0"), "two new failures");
    assert_eq!(marks(&rig).bad, vec![version("2.0")]);
}

/// A host that reports anything but the failure it was acked for loses the ack: when it fails
/// again, that counts.
#[tokio::test]
async fn an_ack_drops_when_the_host_reports_something_else() {
    let rig = rig("ov-drop", AUTO, ONE).await;
    round(&rig, 2_000, &[]);
    rig.set(TWO).await;
    round(&rig, 2_030, &two_failed());
    rig.set(TWO).await;
    assert_eq!(persisted(&rig).failure_acked.len(), 2);

    round(&rig, 2_045, &[]);
    assert!(persisted(&rig).failure_acked.is_empty());
    assert!(marks(&rig).failure_acked.is_empty());

    round(&rig, 2_060, &two_failed());
    assert_eq!(wanted(&rig), version("1.0"), "the new failures count");
}

/// A host that is next ordered the service loses its ack, whatever it still reports.
#[tokio::test]
async fn an_ack_drops_when_the_host_is_next_ordered() {
    let mut rig = rig("ov-ordered", AUTO, TWO).await;
    seed(&rig, |m| {
        m.last_good = Some(version("1.0"));
        m.failure_acked = vec![ack("d1", "2.0")];
    });
    rig.set(THREE).await;
    restart(&mut rig, AUTO);

    let (phase, class) = release();
    let replies = round(&rig, 2_100, &[("d1", failure(phase, class, "2.0"))]);
    assert!(matches!(replies[0], ServiceReply::Update { .. }));
    assert!(persisted(&rig).failure_acked.is_empty());
}

/// An ack matches one report: with the reports of four hosts the same as before, the two whose
/// acks are for exactly that report stay hidden, and the two whose acks are for a report of
/// another reason count, and their stale acks are dropped.
#[test]
fn an_ack_matches_the_whole_report_and_not_one_of_the_same_target() {
    let (phase, class) = release();
    let automatic = def(AUTO);
    let fleet = farm(6, 4, phase, class);
    let mut other = ack("d2", "2.0");
    other.report.reason = Some(Token::parse("disk").expect("a token"));
    let mut another = ack("d3", "2.0");
    another.report.reason = Some(Token::parse("net").expect("a token"));
    let acked = ServiceOps {
        failure_acked: vec![ack("d0", "2.0"), ack("d1", "2.0"), other, another],
        ..good()
    };
    assert_eq!(
        plan_for(&automatic, &acked, &fleet),
        vec![
            Change::DropAck("d2".to_owned()),
            Change::DropAck("d3".to_owned()),
            Change::MarkBad(version("2.0")),
            Change::RollBackTo(version("1.0"))
        ]
    );
}

/// After the override, a host that fails again at the same target but with another class or
/// phase has made a different report: it counts at once.
#[tokio::test]
async fn a_new_failure_at_the_same_target_with_another_class_or_phase_counts_again() {
    let rig = rig("ov-same-target", AUTO, ONE).await;
    round(&rig, 2_000, &[]);
    rig.set(TWO).await;
    round(&rig, 2_030, &two_failed());
    rig.set(TWO).await;

    let (phase, class) = release();
    round(&rig, 2_045, &two_failed());
    assert_eq!(wanted(&rig), version("2.0"), "the same reports stay hidden");

    let again = [
        ("d1", failure(Some(Phase::AfterCommit), None, "2.0")),
        ("d2", failure(phase, class, "2.0")),
    ];
    round(&rig, 2_060, &again);
    assert_eq!(wanted(&rig), version("1.0"), "other reports count");
    assert_eq!(marks(&rig).bad, vec![version("2.0")]);
}

/// A `min_version` and a desired version that are the same release by number (the desired
/// one has a build suffix) are the same for the floor: the last good version is forgotten.
#[test]
fn a_min_version_that_is_the_same_release_by_number_is_the_floor() {
    let (phase, class) = release();
    let floored = def(&format!("{AUTO}\nmin_version = \"0.1.10\""));
    assert_eq!(
        plan(
            &floored,
            &desired("0.1.10+gabc-8", 0, 0),
            &last_good("0.1.9"),
            &farm(6, 0, phase, class),
            secs(2_000)
        ),
        vec![Change::ClearLastGood]
    );
}

/// An order is never given while its host's ack is on disk: when the ack cannot be dropped
/// durably, the host is not ordered and holds no slot, and the ack stands.
#[tokio::test]
async fn an_order_is_not_sent_when_its_ack_cannot_be_dropped() {
    let mut state = OpsState::default();
    state.services.insert(
        "kbf".to_owned(),
        ServiceOps {
            failure_acked: vec![ack("d1", "2.0")],
            ..good()
        },
    );
    let rig = rig_over("ov-unwritable", AUTO, THREE, unwritable(state)).await;
    let (phase, class) = release();
    let replies = round(&rig, 2_000, &[("d1", failure(phase, class, "2.0"))]);
    assert_eq!(replies[0], ServiceReply::Nothing);
    assert_eq!(marks(&rig).failure_acked, vec![ack("d1", "2.0")]);
    let shown = crate::rollback_tests::status(&rig);
    let host = shown.hosts.iter().find(|h| h.node_id == "d1").expect("d1");
    assert!(!host.holds_slot, "no order exists for it");
}

/// The same, with the file: while the ops file cannot be written the host is not ordered; a
/// restart and a file that can be written again order it, and the ack is gone from the file by
/// then.
#[tokio::test]
async fn a_blocked_ack_drop_holds_the_order_across_a_restart() {
    let mut rig = rig("ov-blocked", AUTO, TWO).await;
    seed(&rig, |m| {
        m.last_good = Some(version("1.0"));
        m.failure_acked = vec![ack("d1", "2.0")];
    });
    rig.set(THREE).await;
    restart(&mut rig, AUTO);
    let blocker = rig.dir.path().join("desired.json.ops.new");
    std::fs::create_dir(&blocker).expect("blocked");

    let (phase, class) = release();
    let replies = round(&rig, 2_100, &[("d1", failure(phase, class, "2.0"))]);
    assert_eq!(replies[0], ServiceReply::Nothing);
    assert_eq!(persisted(&rig).failure_acked, vec![ack("d1", "2.0")]);

    restart(&mut rig, AUTO);
    std::fs::remove_dir(&blocker).expect("unblocked");
    let replies = round(&rig, 2_200, &[("d1", failure(phase, class, "2.0"))]);
    assert!(matches!(replies[0], ServiceReply::Update { .. }));
    assert!(persisted(&rig).failure_acked.is_empty());
}

/// An ops file written before `failure_acked` existed (marks with only `last_good` and `bad`)
/// loads with no acks, and writing it back does not add the key.
#[test]
fn an_ops_file_without_failure_acked_loads_and_is_written_back_without_it() {
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "ov-old-file");
    let path = dir.path().join("desired.json.ops");
    let old = r#"{"schema":1,"services":{"kbf":{"halt":null,"halt_acked":[],"last_good":"1.0","bad":["2.0"]}}}"#;
    std::fs::write(&path, old).expect("written");
    let store = crate::ops::OpsStore::open(&path).expect("opened");
    let marks = store.marks("kbf");
    assert_eq!(marks.last_good, Some(version("1.0")));
    assert_eq!(marks.bad, vec![version("2.0")]);
    assert!(marks.failure_acked.is_empty());
    store
        .change(
            crate::ops::Write::BeforeMemory,
            crate::ops::AuditEntry::new("operator", "seed"),
            |_| {},
        )
        .expect("changed");
    let text = std::fs::read_to_string(&path).expect("read");
    assert!(!text.contains("failure_acked"), "{text}");
    assert!(text.contains("\"last_good\": \"1.0\""), "{text}");
}
