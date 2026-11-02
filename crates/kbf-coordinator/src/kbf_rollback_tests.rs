//! kbf as shipped rolls back on its own (`kbf.services.toml`: `rollback = "automatic"` at the
//! default threshold). Every coordinator here has no services file, so kbf's definition is
//! the shipped one; its six daemon hosts beat one round at a time on a clock the test sets,
//! and their reports are what an agent sends after an update (`kbf_updater::wire`).

use kbf_updater::wire::{Class, Heartbeat, Outcome, Phase, ServiceReply, UpdateResult};

use crate::converge::State;
use crate::desired::KBF;
use crate::reconcile::{FailThreshold, Rollback};
use crate::rollback_tests::{HOSTS, ONE, THREE, TWO, marks, persisted, round, status, wanted};
use crate::services::{ServiceDef, Services};
use crate::testutil::{Rig, beat, version};

/// A host running `running` whose last update, to `target`, failed as `phase` and `class` say.
fn failed(running: &str, target: &str, phase: Option<Phase>, class: Option<Class>) -> Heartbeat {
    let mut b = beat(running);
    b.last_update = Some(UpdateResult {
        target: version(target),
        outcome: Outcome::Failed,
        phase,
        class,
        reason: None,
    });
    b
}

/// A failure of the release after the commit point: the one kind that counts.
fn after_commit(running: &str, target: &str) -> Heartbeat {
    failed(
        running,
        target,
        Some(Phase::AfterCommit),
        Some(Class::Release),
    )
}

/// Every one of the six hosts beating `b`.
fn all(b: &Heartbeat) -> Vec<(&'static str, Heartbeat)> {
    HOSTS.iter().map(|h| (*h, b.clone())).collect()
}

/// A coordinator with no services file, past its quiet start, with `body` desired.
async fn shipped(test: &str, body: &str) -> Rig {
    let rig = Rig::new(test);
    rig.set(body).await;
    rig.at(2_000);
    rig
}

/// [`shipped`] at 1.0, after one round of its six hosts healthy at 1.0 (which the coordinator
/// records as the last good version); then CI sets 2.0.
async fn rolling_out_two(test: &str) -> Rig {
    let rig = shipped(test, ONE).await;
    round(&rig, 2_000, &[]);
    assert_eq!(
        marks(&rig).last_good,
        Some(version("1.0")),
        "the version the farm converged on is recorded"
    );
    rig.set(TWO).await;
    rig
}

/// Catches: kbf shipped with manual rollback, or with a threshold other than the default (a
/// quarter of the live hosts, at least two). kbf's definition is the shipped one with no
/// services file and with a file that does not name kbf; a file whose first entry replaces kbf
/// decides for its farm, and one that says nothing about rollback gets the manual default.
#[test]
fn the_shipped_kbf_definition_loads_with_automatic_rollback() {
    let other = "[[service]]\nname = \"other\"\n[service.roles.daemon]\n";
    let loaded = [
        ServiceDef::kbf(),
        Services::default().get(KBF).expect("kbf").clone(),
        Services::parse(other)
            .expect("services")
            .get(KBF)
            .expect("kbf")
            .clone(),
    ];
    for def in loaded {
        assert_eq!(def.rollback, Rollback::Automatic);
        assert_eq!(
            def.rollback_after_failed,
            FailThreshold {
                percent: 25,
                min_hosts: 2
            }
        );
        assert_eq!(def.min_version, None);
    }
    let replaced =
        Services::parse("[[service]]\nname = \"kbf\"\n[service.roles.daemon]\n").expect("services");
    assert_eq!(replaced.get(KBF).expect("kbf").rollback, Rollback::Manual);
}

/// Catches: a kbf farm that stays on a release two of its six hosts failed after the commit.
/// The coordinator marks 2.0 bad and sets 1.0, the last good version, back as desired, both on
/// disk, and the audit names the coordinator; the hosts, all back on 1.0, are told nothing.
#[tokio::test]
async fn two_post_commit_failures_of_six_hosts_set_last_good_back_and_mark_the_release_bad() {
    let rig = rolling_out_two("kbf-rb-two").await;
    round(
        &rig,
        2_015,
        &[
            ("d1", after_commit("1.0", "2.0")),
            ("d2", after_commit("1.0", "2.0")),
        ],
    );
    assert_eq!(wanted(&rig), version("1.0"));
    assert_eq!(marks(&rig).bad, vec![version("2.0")]);
    assert_eq!(persisted(&rig).bad, vec![version("2.0")]);
    assert_eq!(persisted(&rig).last_good, Some(version("1.0")));
    let audit = status(&rig).ops.audit;
    let entry = audit
        .iter()
        .find(|e| e.action == "mark_bad")
        .expect("the mark is audited");
    assert_eq!(
        (entry.by.as_str(), entry.service.as_deref()),
        ("coordinator", Some(KBF))
    );
    let replies = round(&rig, 2_030, &[]);
    assert!(
        replies.iter().all(|r| *r == ServiceReply::Nothing),
        "{replies:?}"
    );
}

/// Catches: a threshold of one host. One host of six fails 2.0 after the commit, beat after
/// beat: 2.0 stays desired and unmarked, and the farm is blocked on it, as before G2.
#[tokio::test]
async fn one_post_commit_failure_of_six_hosts_changes_nothing() {
    let rig = rolling_out_two("kbf-rb-one").await;
    for at in [2_015, 2_030, 2_045] {
        round(&rig, at, &[("d1", after_commit("1.0", "2.0"))]);
        assert_eq!(wanted(&rig), version("2.0"), "at {at}");
        assert!(marks(&rig).bad.is_empty(), "at {at}");
    }
    assert_eq!(marks(&rig).last_good, Some(version("1.0")));
    assert_eq!(
        status(&rig).per_service[KBF].convergence.state,
        State::Blocked
    );
}

/// Catches: a rollback with nowhere to go. The first version a farm is given has no last good
/// version: every host failing it after the commit leaves it desired and blocked, nothing
/// marked. Once the farm converges on it, it is the last good version, and the next release
/// that fails on two hosts goes back to it.
#[tokio::test]
async fn a_first_version_with_no_last_good_is_never_rolled_back() {
    let rig = shipped("kbf-rb-first", TWO).await;
    for at in [2_000, 2_015] {
        round(&rig, at, &all(&after_commit("1.0", "2.0")));
    }
    let shown = status(&rig);
    assert_eq!(shown.services[KBF].version, version("2.0"));
    assert_eq!(shown.per_service[KBF].convergence.state, State::Blocked);
    assert!(marks(&rig).bad.is_empty());
    assert_eq!(marks(&rig).last_good, None);

    round(&rig, 2_030, &all(&beat("2.0")));
    assert_eq!(marks(&rig).last_good, Some(version("2.0")));
    rig.set(THREE).await;
    round(
        &rig,
        2_045,
        &[
            ("d1", after_commit("2.0", "3.0")),
            ("d2", after_commit("2.0", "3.0")),
            ("d3", beat("2.0")),
            ("d4", beat("2.0")),
            ("d5", beat("2.0")),
            ("d6", beat("2.0")),
        ],
    );
    assert_eq!(wanted(&rig), version("2.0"));
    assert_eq!(marks(&rig).bad, vec![version("3.0")]);
}

/// Catches: a rollback for a failure that is not the release's after its commit. Every host
/// failing 2.0 before the commit, at the environment gate, after the commit blamed on the
/// environment, or with no phase at all (what kbf's own A/B update on a host reports today,
/// `kbf_updater::agent::update`) changes nothing, round after round. The control, two hosts
/// failing it after the commit as a release, does roll it back.
#[tokio::test]
async fn before_commit_and_environment_failures_never_count() {
    let rig = rolling_out_two("kbf-rb-kinds").await;
    let not_counted = [
        (Some(Phase::BeforeCommit), Some(Class::Release)),
        (Some(Phase::GateEnvironment), Some(Class::Environment)),
        (Some(Phase::AfterCommit), Some(Class::Environment)),
        (None, None),
    ];
    let mut at = 2_000;
    for (phase, class) in not_counted {
        at += 15;
        round(&rig, at, &all(&failed("1.0", "2.0", phase, class)));
        assert_eq!(wanted(&rig), version("2.0"), "{phase:?} {class:?}");
        assert!(marks(&rig).bad.is_empty(), "{phase:?} {class:?}");
    }
    round(
        &rig,
        at + 15,
        &[
            ("d1", after_commit("1.0", "2.0")),
            ("d2", after_commit("1.0", "2.0")),
        ],
    );
    assert_eq!(wanted(&rig), version("1.0"));
    assert_eq!(marks(&rig).bad, vec![version("2.0")]);
}
