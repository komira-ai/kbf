//! The last good version, the bad ones and the automatic rollback. The pure rules are
//! tested on [`plan`] and [`decide_marked`] with fleets built beat by beat; the effects, the
//! two writes of a rollback and what survives a restart, are tested on a coordinator with
//! the files of a real directory. Every time is explicit.

use std::collections::BTreeMap;
use std::fs;
use std::time::Duration;

use kbf_types::HostRole;
use kbf_updater::version::Version;
use kbf_updater::wire::{
    Class, DeferredReport, Heartbeat, Outcome, Phase, ServiceReply, Token, UpdateResult,
};

use crate::converge::State;
use crate::decide::{Decision, Hold, decide_marked, decide_why};
use crate::desired::{DesiredStore, KBF, StoreError};
use crate::fleet::Fleet;
use crate::identity::Identity;
use crate::ops::{AuditEntry, Halt, OpsError, OpsState, OpsStore, ServiceOps, Write};
use crate::reconcile::{Change, plan};
use crate::service::SetError;
use crate::services::{ServiceDef, Services};
use crate::testutil::{OPERATOR, Rig, beat, ci_token, desired, id, ops_of, version};

pub(crate) const AUTO: &str = "rollback = \"automatic\"";
pub(crate) const ONE: &str = r#"{"version":"1.0","min_up":{"storage":0,"frontend":0}}"#;
pub(crate) const TWO: &str = r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#;
pub(crate) const THREE: &str = r#"{"version":"3.0","min_up":{"storage":0,"frontend":0}}"#;
pub(crate) const HOSTS: [&str; 6] = ["d1", "d2", "d3", "d4", "d5", "d6"];

pub(crate) fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// `kbf` replaced by a service of daemons that all may update at once, with the definition
/// lines `extra` in its table.
pub(crate) fn definition(extra: &str) -> String {
    format!(
        "[[service]]\nname = \"kbf\"\n{extra}\n[service.roles.daemon]\nmax_updating = 6\nmin_up = 0\n"
    )
}

pub(crate) fn def(extra: &str) -> ServiceDef {
    Services::parse(&definition(extra))
        .expect("services")
        .get(KBF)
        .expect("kbf")
        .clone()
}

pub(crate) fn failure(phase: Option<Phase>, class: Option<Class>, target: &str) -> Heartbeat {
    let mut b = beat("1.0");
    b.last_update = Some(UpdateResult {
        target: version(target),
        outcome: Outcome::Failed,
        phase,
        class,
        reason: None,
    });
    b
}

/// The marks of a service whose last good version is 1.0.
pub(crate) fn good() -> ServiceOps {
    ServiceOps {
        last_good: Some(version("1.0")),
        ..ServiceOps::default()
    }
}

/// `n` daemons at 1.0 that heartbeated at 2000, the first `failed` of which failed an
/// update to 2.0 as `phase` and `class` say.
pub(crate) fn farm(n: usize, failed: usize, phase: Option<Phase>, class: Option<Class>) -> Fleet {
    let mut fleet = Fleet::new(Duration::ZERO);
    for i in 0..n {
        let b = if i < failed {
            failure(phase, class, "2.0")
        } else {
            beat("1.0")
        };
        fleet.record(id(&format!("d{i}")), HostRole::DAEMON, b, secs(2_000));
    }
    fleet
}

pub(crate) fn release() -> (Option<Phase>, Option<Class>) {
    (Some(Phase::AfterCommit), Some(Class::Release))
}

/// What the plan says for `def` at 2000 when 2.0 is wanted.
pub(crate) fn plan_for(def: &ServiceDef, marks: &ServiceOps, fleet: &Fleet) -> Vec<Change> {
    plan(
        def,
        &desired("2.0", 0, 0),
        marks,
        fleet,
        &OpsState::default(),
        secs(2_000),
    )
}

pub(crate) fn rolled_back() -> Vec<Change> {
    vec![
        Change::MarkBad(version("2.0")),
        Change::RollBackTo(version("1.0")),
    ]
}

// ---- the threshold ------------------------------------------------------------------

/// With the default threshold (a quarter of the hosts, at least two) one failed host of six
/// changes nothing and two roll the service back: the larger of the two numbers decides.
#[test]
fn one_failure_is_below_the_default_threshold() {
    let (phase, class) = release();
    let automatic = def(AUTO);
    assert_eq!(
        plan_for(&automatic, &good(), &farm(6, 1, phase, class)),
        vec![]
    );
    assert_eq!(
        plan_for(&automatic, &good(), &farm(6, 2, phase, class)),
        rolled_back()
    );
    // Two of four is past the minimum too, and a single host is never enough.
    assert_eq!(
        plan_for(&automatic, &good(), &farm(4, 2, phase, class)),
        rolled_back()
    );
    assert_eq!(
        plan_for(&automatic, &good(), &farm(2, 1, phase, class)),
        vec![]
    );
}

/// `max(min_hosts, ceil(percent * live / 100))`: ten hosts at 25% need three (the
/// percentage binds), four need two (the minimum binds), and six at 40% need three (the
/// percentage, rounded up from 2.4). The threshold is the definition's, not a constant.
#[test]
fn the_threshold_is_the_larger_of_min_hosts_and_the_percentage() {
    let (phase, class) = release();
    let automatic = def(AUTO);
    assert_eq!(
        plan_for(&automatic, &good(), &farm(10, 2, phase, class)),
        vec![],
        "25% of 10 is 3"
    );
    assert_eq!(
        plan_for(&automatic, &good(), &farm(10, 3, phase, class)),
        rolled_back()
    );
    let forty = def(&format!(
        "{AUTO}\nrollback_after_failed = {{ percent = 40, min_hosts = 1 }}"
    ));
    assert_eq!(
        plan_for(&forty, &good(), &farm(6, 2, phase, class)),
        vec![],
        "40% of 6 is 2.4, so 3"
    );
    assert_eq!(
        plan_for(&forty, &good(), &farm(6, 3, phase, class)),
        rolled_back()
    );
    let lone = def(&format!(
        "{AUTO}\nrollback_after_failed = {{ percent = 1, min_hosts = 1 }}"
    ));
    assert_eq!(
        plan_for(&lone, &good(), &farm(6, 1, phase, class)),
        rolled_back(),
        "a definition may ask for one host"
    );
}

/// Only a host that is live counts, and it counts against the live hosts: a failed host
/// that has gone silent is neither a failure nor a host to take a percentage of.
#[test]
fn a_silent_host_is_neither_a_failure_nor_part_of_the_percentage() {
    let (phase, class) = release();
    let mut fleet = farm(6, 0, phase, class);
    fleet.record(
        id("gone1"),
        HostRole::DAEMON,
        failure(phase, class, "2.0"),
        secs(1_000),
    );
    fleet.record(
        id("gone2"),
        HostRole::DAEMON,
        failure(phase, class, "2.0"),
        secs(1_000),
    );
    assert_eq!(plan_for(&def(AUTO), &good(), &fleet), vec![]);
}

/// A failure of another version, or of this one on a host that has since reached it, is not a
/// failure of the desired version.
#[test]
fn only_failures_of_the_desired_version_count() {
    let (phase, class) = release();
    let mut fleet = Fleet::new(Duration::ZERO);
    fleet.record(
        id("d1"),
        HostRole::DAEMON,
        failure(phase, class, "1.5"),
        secs(2_000),
    );
    fleet.record(
        id("d2"),
        HostRole::DAEMON,
        failure(phase, class, "1.5"),
        secs(2_000),
    );
    let mut ran_it = failure(phase, class, "2.0");
    ran_it.version = version("2.0");
    fleet.record(id("d3"), HostRole::DAEMON, ran_it.clone(), secs(2_000));
    fleet.record(id("d4"), HostRole::DAEMON, ran_it, secs(2_000));
    let mut done = beat("1.0");
    done.last_update = Some(UpdateResult::new(version("2.0"), Outcome::Succeeded));
    fleet.record(id("d5"), HostRole::DAEMON, done, secs(2_000));
    assert_eq!(plan_for(&def(AUTO), &good(), &fleet), vec![]);
}

/// Environment failures, failures before the commit, deferrals, halts and failures that say
/// nothing about where they happened never count, however many hosts report them. The
/// controls, a failure after the commit and one at the release gate, do.
#[test]
fn environment_before_commit_deferred_and_halt_outcomes_never_roll_back() {
    let automatic = def(AUTO);
    let never = [
        (Some(Phase::BeforeCommit), Some(Class::Release)),
        (Some(Phase::GateEnvironment), Some(Class::Environment)),
        (Some(Phase::GateRelease), Some(Class::Environment)),
        (Some(Phase::AfterCommit), Some(Class::Environment)),
        (Some(Phase::Halt), Some(Class::Release)),
        (Some(Phase::Halt), None),
        (None, Some(Class::Release)),
        (None, None),
    ];
    for (phase, class) in never {
        let fleet = farm(6, 6, phase, class);
        assert_eq!(
            plan_for(&automatic, &good(), &fleet),
            vec![],
            "{phase:?} {class:?}"
        );
    }
    let counted = [
        (Some(Phase::AfterCommit), Some(Class::Release)),
        (Some(Phase::AfterCommit), None),
        (Some(Phase::GateRelease), Some(Class::Release)),
        (Some(Phase::GateRelease), None),
    ];
    for (phase, class) in counted {
        let fleet = farm(6, 2, phase, class);
        assert_eq!(
            plan_for(&automatic, &good(), &fleet),
            rolled_back(),
            "{phase:?} {class:?}"
        );
    }
    let mut fleet = Fleet::new(Duration::ZERO);
    for i in 0..6 {
        let mut b = beat("1.0");
        b.deferred = Some(DeferredReport {
            target: version("2.0"),
            reason: Token::parse("busy").expect("a token"),
            retry_after_secs: 300,
        });
        fleet.record(id(&format!("d{i}")), HostRole::DAEMON, b, secs(2_000));
    }
    assert_eq!(plan_for(&automatic, &good(), &fleet), vec![]);
}

// ---- when it may roll back ------------------------------------------------------------

/// A `manual` service, named or by default, is never rolled back by the coordinator, however
/// many hosts fail.
#[test]
fn a_manual_service_is_never_rolled_back_by_the_coordinator() {
    let (phase, class) = release();
    let fleet = farm(6, 6, phase, class);
    for extra in ["", "rollback = \"manual\""] {
        assert_eq!(plan_for(&def(extra), &good(), &fleet), vec![], "{extra:?}");
    }
}

/// Without a last good version, or when the last good version is the desired one, there is
/// nothing to roll back to: the service stays blocked on the failures.
#[test]
fn without_last_good_the_service_is_blocked_and_not_rolled_back() {
    let (phase, class) = release();
    let fleet = farm(6, 6, phase, class);
    let automatic = def(AUTO);
    assert_eq!(plan_for(&automatic, &ServiceOps::default(), &fleet), vec![]);
    let itself = ServiceOps {
        last_good: Some(version("2.0")),
        ..ServiceOps::default()
    };
    assert_eq!(plan_for(&automatic, &itself, &fleet), vec![]);
}

/// A standing halt is the stronger stop: while one is up, nothing is rolled back and no
/// version is blessed.
#[test]
fn a_halt_beats_a_rollback() {
    let (phase, class) = release();
    let halted = ServiceOps {
        halt: Some(Halt {
            node_id: "d1".to_owned(),
            check: "tainted".to_owned(),
            target: version("2.0"),
            report: None,
        }),
        ..good()
    };
    let automatic = def(AUTO);
    assert_eq!(
        plan_for(&automatic, &halted, &farm(6, 6, phase, class)),
        vec![]
    );
    let converged = ServiceOps {
        last_good: None,
        ..halted
    };
    assert_eq!(
        plan(
            &automatic,
            &desired("1.0", 0, 0),
            &converged,
            &farm(6, 0, phase, class),
            &OpsState::default(),
            secs(2_000)
        ),
        vec![]
    );
}

/// A coordinator that has not finished listening (its quiet start) has not heard the hosts
/// that would outvote a few failures, so it changes nothing.
#[test]
fn nothing_is_planned_in_the_quiet_start() {
    let (phase, class) = release();
    let mut fleet = Fleet::new(secs(1_990));
    for i in 0..6 {
        fleet.record(
            id(&format!("d{i}")),
            HostRole::DAEMON,
            failure(phase, class, "2.0"),
            secs(2_000),
        );
    }
    assert_eq!(plan_for(&def(AUTO), &good(), &fleet), vec![]);
}

// ---- bad -------------------------------------------------------------------------------

/// A version marked bad is not ordered even when the desired state names it, and the status
/// says why. The same fleet without the mark is told to update.
#[test]
fn a_bad_version_is_not_ordered_even_if_desired_names_it() {
    let services = Services::parse(&definition("")).expect("services");
    let all = BTreeMap::from([(KBF.to_owned(), desired("2.0", 0, 0))]);
    let fleet = Fleet::new(Duration::ZERO);
    let mut fleet = fleet;
    fleet.record(id("d1"), HostRole::DAEMON, beat("1.0"), secs(2_000));
    let go = Decision::Update {
        version: version("2.0"),
    };
    let ask =
        |bad: &[Version]| decide_marked(&services, &all, bad, KBF, &fleet, &id("d1"), secs(2_000));
    assert_eq!(ask(&[]), go);
    assert_eq!(ask(&[version("2.0")]), Decision::Hold(Hold::Bad));
    assert_eq!(
        ask(&[version("1.5")]),
        go,
        "another version's mark is no hold"
    );
    assert_eq!(
        decide_why(&services, &all, KBF, &fleet, &id("d1"), secs(2_000)),
        go,
        "the unmarked decision knows no marks"
    );
}

/// When the desired version is already marked bad (the coordinator stopped between its two
/// writes), the plan only finishes the rollback: the mark is not written twice.
#[test]
fn a_plan_over_a_marked_desired_version_only_sets_the_desired_state() {
    let (phase, class) = release();
    let marked = ServiceOps {
        bad: vec![version("2.0")],
        ..good()
    };
    assert_eq!(
        plan_for(&def(AUTO), &marked, &farm(6, 2, phase, class)),
        vec![Change::RollBackTo(version("1.0"))]
    );
    // The mark was the decision: the hosts need not still say so.
    assert_eq!(
        plan_for(&def(AUTO), &marked, &farm(6, 0, phase, class)),
        vec![Change::RollBackTo(version("1.0"))]
    );
    // A manual service, whose version somebody marked, is left to its operator.
    assert_eq!(
        plan_for(&def(""), &marked, &farm(6, 2, phase, class)),
        vec![]
    );
}

// ---- last_good -------------------------------------------------------------------------

pub(crate) fn converged(beats: &[(&str, Heartbeat)]) -> Fleet {
    let mut fleet = Fleet::new(Duration::ZERO);
    for (name, b) in beats {
        fleet.record(id(name), HostRole::DAEMON, b.clone(), secs(2_000));
    }
    fleet
}

pub(crate) fn all_at_one() -> Vec<(&'static str, Heartbeat)> {
    HOSTS.iter().map(|h| (*h, beat("1.0"))).collect()
}

pub(crate) fn plan_one(def: &ServiceDef, marks: &ServiceOps, fleet: &Fleet) -> Vec<Change> {
    plan(
        def,
        &desired("1.0", 0, 0),
        marks,
        fleet,
        &OpsState::default(),
        secs(2_000),
    )
}

/// The desired version is recorded as last good when every live host runs it, healthy and
/// settled, and not before: not while a host is behind, waiting for its promotion, still
/// baking or holding a slot, not in the quiet start, and not for a version already recorded
/// or marked bad.
#[test]
fn last_good_is_recorded_only_when_converged_and_baked() {
    let plain = def(AUTO);
    let none = ServiceOps::default();
    let record = vec![Change::LastGood(version("1.0"))];
    assert_eq!(plan_one(&plain, &none, &converged(&all_at_one())), record);

    // Recorded already: nothing to say.
    assert_eq!(plan_one(&plain, &good(), &converged(&all_at_one())), vec![]);

    // A host behind.
    let mut behind = all_at_one();
    behind[0].1 = beat("0.9");
    assert_eq!(plan_one(&plain, &none, &converged(&behind)), vec![]);

    // A host waiting for its promotion.
    let mut pending = all_at_one();
    pending[0].1.promotion_pending = true;
    assert_eq!(plan_one(&plain, &none, &converged(&pending)), vec![]);

    // A host at the version that holds a slot: unhealthy after an older failure.
    let mut slot = all_at_one();
    slot[0].1 = failure(Some(Phase::AfterCommit), Some(Class::Release), "0.9");
    slot[0].1.version = version("1.0");
    slot[0].1 = crate::testutil::unhealthy(slot[0].1.clone());
    assert_eq!(plan_one(&plain, &none, &converged(&slot)), vec![]);

    // The quiet start.
    let mut quiet = Fleet::new(secs(1_990));
    for (name, b) in all_at_one() {
        quiet.record(id(name), HostRole::DAEMON, b, secs(2_000));
    }
    assert_eq!(plan_one(&plain, &none, &quiet), vec![]);

    // A version marked bad is never the last good one.
    let marked = ServiceOps {
        bad: vec![version("1.0")],
        ..ServiceOps::default()
    };
    assert_eq!(plan_one(&plain, &marked, &converged(&all_at_one())), vec![]);
}

/// With `bake_beats = 2` the version is recorded at the second healthy beat of every host and
/// not at the first.
#[test]
fn last_good_waits_for_the_bake() {
    let baking = def(&format!("{AUTO}\nbake_beats = 2"));
    let mut fleet = Fleet::new(Duration::ZERO);
    for round in 1..=2_u64 {
        for (name, b) in all_at_one() {
            fleet.record(id(name), HostRole::DAEMON, b, secs(2_000 + round));
        }
        let expect = if round == 1 {
            vec![]
        } else {
            vec![Change::LastGood(version("1.0"))]
        };
        assert_eq!(
            plan(
                &baking,
                &desired("1.0", 0, 0),
                &ServiceOps::default(),
                &fleet,
                &OpsState::default(),
                secs(2_000 + round)
            ),
            expect,
            "after beat {round}"
        );
    }
}

// ---- min_version -----------------------------------------------------------------------

/// Setting the desired state to `min_version` forgets the last good version (the version
/// before the first release with hooks is no rollback target) and a failure at it rolls
/// nothing back; any other desired version leaves it alone; and once the farm has converged
/// on `min_version` that version is recorded as last good.
#[test]
fn setting_min_version_clears_last_good() {
    let (phase, class) = release();
    let floored = def(&format!("{AUTO}\nmin_version = \"2.0\""));
    let fleet = farm(6, 0, phase, class);
    assert_eq!(
        plan_for(&floored, &good(), &fleet),
        vec![Change::ClearLastGood]
    );
    assert_eq!(
        plan_for(&floored, &ServiceOps::default(), &fleet),
        vec![],
        "nothing left to clear"
    );
    assert_eq!(
        plan_for(&floored, &good(), &farm(6, 6, phase, class)),
        vec![Change::ClearLastGood],
        "failures at min_version have no older version to go to"
    );
    // Another desired version leaves it alone.
    assert_eq!(
        plan(
            &floored,
            &desired("3.0", 0, 0),
            &good(),
            &farm(6, 0, phase, class),
            &OpsState::default(),
            secs(2_000)
        ),
        vec![]
    );
    // Converged on min_version: it becomes the last good version.
    let mut at_two = Fleet::new(Duration::ZERO);
    for h in HOSTS {
        at_two.record(id(h), HostRole::DAEMON, beat("2.0"), secs(2_000));
    }
    assert_eq!(
        plan_for(&floored, &good(), &at_two),
        vec![Change::LastGood(version("2.0"))]
    );
}

// ---- the coordinator -------------------------------------------------------------------

pub(crate) fn services_of(extra: &str) -> Services {
    Services::parse(&definition(extra)).expect("services")
}

/// A rig whose coordinator runs `extra`'s definition, with `body` desired, past the quiet
/// start.
pub(crate) async fn rig(test: &str, extra: &str, body: &str) -> Rig {
    let mut rig = Rig::new(test);
    rig.coordinator = Rig::start(&rig.dir, &rig.clock).with_services(services_of(extra));
    rig.set(body).await;
    rig.at(2_000);
    rig
}

/// The same files, a coordinator that starts now.
pub(crate) fn restart(rig: &mut Rig, extra: &str) {
    rig.coordinator = Rig::start(&rig.dir, &rig.clock).with_services(services_of(extra));
}

pub(crate) fn who(host: &str) -> Identity {
    Identity {
        node_id: id(host),
        role: HostRole::DAEMON,
    }
}

/// One heartbeat of every host at `at`, each as `overrides` say or healthy at 1.0, in node
/// order; the answers are those of the last host.
pub(crate) fn round(rig: &Rig, at: u64, overrides: &[(&str, Heartbeat)]) -> Vec<ServiceReply> {
    rig.at(at);
    HOSTS
        .iter()
        .map(|host| {
            let b = overrides
                .iter()
                .find(|(h, _)| h == host)
                .map_or_else(|| beat("1.0"), |(_, b)| b.clone());
            rig.coordinator.heartbeat(who(host), b).reply
        })
        .collect()
}

pub(crate) fn two_failed() -> Vec<(&'static str, Heartbeat)> {
    let (phase, class) = release();
    vec![
        ("d1", failure(phase, class, "2.0")),
        ("d2", failure(Some(Phase::GateRelease), None, "2.0")),
    ]
}

pub(crate) fn status(rig: &Rig) -> crate::service::Status {
    rig.coordinator.status(Some(OPERATOR)).expect("status")
}

pub(crate) fn wanted(rig: &Rig) -> Version {
    status(rig).services[KBF].version.clone()
}

pub(crate) fn marks(rig: &Rig) -> ServiceOps {
    status(rig)
        .ops
        .services
        .get(KBF)
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn persisted(rig: &Rig) -> ServiceOps {
    ops_of(&rig.dir)
        .state()
        .services
        .get(KBF)
        .cloned()
        .unwrap_or_default()
}

/// The ops file as a crash would leave it: `edit` made to the marks of `kbf`.
pub(crate) fn seed(rig: &Rig, edit: impl FnOnce(&mut ServiceOps)) {
    ops_of(&rig.dir)
        .change(
            Write::BeforeMemory,
            AuditEntry::new("operator", "seed"),
            |state| edit(state.services.entry(KBF.to_owned()).or_default()),
        )
        .expect("seeded");
}

/// Two hosts of six fail a release after the commit: the desired state goes back to the last
/// good version, the failed version is marked bad, both are on disk, and the audit says who
/// did it. One failure alone does nothing.
#[tokio::test]
async fn release_failures_past_the_threshold_roll_desired_back_and_mark_the_version_bad() {
    let rig = rig("rb-auto", AUTO, ONE).await;
    round(&rig, 2_000, &[]);
    assert_eq!(marks(&rig).last_good, Some(version("1.0")));

    rig.set(TWO).await;
    let (phase, class) = release();
    round(&rig, 2_015, &[("d1", failure(phase, class, "2.0"))]);
    assert_eq!(wanted(&rig), version("2.0"), "one failed host");
    assert!(marks(&rig).bad.is_empty());

    round(&rig, 2_030, &two_failed());
    assert_eq!(wanted(&rig), version("1.0"));
    assert_eq!(marks(&rig).bad, vec![version("2.0")]);
    assert_eq!(persisted(&rig).bad, vec![version("2.0")]);
    assert_eq!(persisted(&rig).last_good, Some(version("1.0")));
    let on_disk = DesiredStore::open(&rig.dir.path().join("desired.json")).expect("store");
    assert_eq!(on_disk.get(KBF).expect("desired").version, version("1.0"));

    let audit = status(&rig).ops.audit;
    let entry = audit
        .iter()
        .find(|e| e.action == "mark_bad")
        .expect("the mark is audited");
    assert_eq!(entry.by, "coordinator");
    assert_eq!(entry.service.as_deref(), Some(KBF));

    // The failures stay in the hosts' last updates; they are for a version no longer wanted.
    round(&rig, 2_045, &two_failed());
    assert_eq!(wanted(&rig), version("1.0"));
    assert_eq!(marks(&rig).bad, vec![version("2.0")], "marked once");
}

/// The mark is written first and a failed desired write leaves the version marked and wanted:
/// nobody is ordered it, and the next heartbeat, once the write works, finishes the rollback.
#[tokio::test]
async fn a_crash_between_the_two_writes_is_completed_by_the_next_beat() {
    let rig = rig("rb-crash", AUTO, ONE).await;
    round(&rig, 2_000, &[]);
    rig.set(TWO).await;
    let blocker = rig.dir.path().join("desired.json.new");
    fs::create_dir(&blocker).expect("blocked");

    let replies = round(&rig, 2_030, &two_failed());
    assert_eq!(marks(&rig).bad, vec![version("2.0")]);
    assert_eq!(persisted(&rig).bad, vec![version("2.0")]);
    assert_eq!(wanted(&rig), version("2.0"), "the desired write failed");
    assert!(
        replies.iter().all(|r| *r == ServiceReply::Nothing),
        "a bad version is ordered to nobody: {replies:?}"
    );

    fs::remove_dir(&blocker).expect("unblocked");
    round(&rig, 2_045, &two_failed());
    assert_eq!(wanted(&rig), version("1.0"));
    assert_eq!(marks(&rig).bad, vec![version("2.0")]);
}

/// The same crash, as a restart finds it: the file says 2.0 is bad and the desired state
/// still wants it, so the first beat past the quiet start finishes the rollback.
#[tokio::test]
async fn a_restart_after_the_mark_and_before_the_desired_write_finishes_the_rollback() {
    let mut rig = rig("rb-seeded", AUTO, TWO).await;
    seed(&rig, |m| {
        m.last_good = Some(version("1.0"));
        m.bad = vec![version("2.0")];
    });
    restart(&mut rig, AUTO);
    round(&rig, 2_100, &[]);
    assert_eq!(wanted(&rig), version("1.0"));
}

/// CI setting a version by hand removes its mark (design 5.3), in the file as well, and the
/// version is ordered again; the coordinator does not roll it back. Setting another version
/// leaves the mark where it is.
#[tokio::test]
async fn setting_a_bad_version_by_hand_clears_the_mark() {
    let mut rig = rig("rb-by-hand", AUTO, TWO).await;
    seed(&rig, |m| {
        m.last_good = Some(version("1.0"));
        m.bad = vec![version("2.0")];
    });
    restart(&mut rig, AUTO);
    rig.set(THREE).await;
    assert_eq!(marks(&rig).bad, vec![version("2.0")], "another version");

    rig.set(TWO).await;
    assert!(marks(&rig).bad.is_empty());
    assert!(persisted(&rig).bad.is_empty());
    assert_eq!(persisted(&rig).last_good, Some(version("1.0")));

    let replies = round(&rig, 2_100, &[]);
    assert_eq!(wanted(&rig), version("2.0"));
    assert!(
        replies
            .iter()
            .all(|r| matches!(r, ServiceReply::Update { .. })),
        "{replies:?}"
    );
    let audit = status(&rig).ops.audit;
    assert!(audit.iter().any(|e| e.action == "unmark_bad"), "{audit:?}");
}

/// A mark that cannot be removed from the file fails the set and leaves the desired state as
/// it was: a version CI believes unmarked would otherwise still be refused after a restart.
#[tokio::test]
async fn a_mark_that_cannot_be_cleared_fails_the_set_and_leaves_the_desired_state() {
    let rig = Rig::new("rb-unclearable");
    let mut state = OpsState::default();
    state.services.insert(
        KBF.to_owned(),
        ServiceOps {
            bad: vec![version("2.0")],
            ..ServiceOps::default()
        },
    );
    let failing = OpsStore::with_writer(
        state,
        Box::new(|_| {
            Err(OpsError::Io(StoreError::Io {
                path: "ops".into(),
                source: std::io::Error::other("disk full"),
            }))
        }),
    );
    let coordinator = Rig::start(&rig.dir, &rig.clock).with_ops(failing);
    let set = coordinator
        .set_desired(Some(&ci_token()), TWO.as_bytes())
        .await;
    assert!(matches!(set, Err(SetError::Ops(_))), "{set:?}");
    let status = coordinator.status(Some(OPERATOR)).expect("status");
    assert!(status.services.is_empty(), "desired was not set");
    assert_eq!(status.ops.services[KBF].bad, vec![version("2.0")]);
}

/// A version marked bad, with desired naming it, is held at `bad` in the status and no host
/// is ordered it; the same hosts without the mark are told to update.
#[tokio::test]
async fn a_marked_version_is_held_in_the_status_and_ordered_to_nobody() {
    let mut rig = rig("rb-marked", "", TWO).await;
    let before = round(&rig, 2_000, &[]);
    assert!(matches!(before[0], ServiceReply::Update { .. }));

    seed(&rig, |m| m.bad = vec![version("2.0")]);
    restart(&mut rig, "");
    let after = round(&rig, 2_100, &[]);
    assert!(
        after.iter().all(|r| *r == ServiceReply::Nothing),
        "{after:?}"
    );
    let shown = serde_json::to_value(status(&rig)).expect("json");
    let hosts = &shown["per_service"][KBF]["hosts"];
    assert_eq!(hosts[0]["hold"], "bad");
    assert_eq!(wanted(&rig), version("2.0"), "a manual service keeps it");
}

/// A manual service whose hosts fail stays on the failed version, blocked, and nothing is
/// marked.
#[tokio::test]
async fn a_manual_service_stays_blocked_on_the_failed_version() {
    let rig = rig("rb-manual", "", ONE).await;
    round(&rig, 2_000, &[]);
    rig.set(TWO).await;
    let (phase, class) = release();
    let failed: Vec<_> = HOSTS
        .iter()
        .map(|h| (*h, failure(phase, class, "2.0")))
        .collect();
    round(&rig, 2_030, &failed);
    let shown = status(&rig);
    assert_eq!(shown.services[KBF].version, version("2.0"));
    assert_eq!(shown.per_service[KBF].convergence.state, State::Blocked);
    assert!(marks(&rig).bad.is_empty());
}

/// An automatic service that has never converged has no last good version: it is blocked,
/// not rolled back.
#[tokio::test]
async fn an_automatic_service_with_no_last_good_stays_blocked() {
    let rig = rig("rb-nolast", AUTO, TWO).await;
    round(&rig, 2_000, &[]);
    round(&rig, 2_015, &two_failed());
    let shown = status(&rig);
    assert_eq!(shown.services[KBF].version, version("2.0"));
    assert_eq!(shown.per_service[KBF].convergence.state, State::Blocked);
    assert!(marks(&rig).bad.is_empty());
    assert_eq!(marks(&rig).last_good, None);
}

/// A standing halt stops the rollback: the failures do not move the desired state.
#[tokio::test]
async fn a_standing_halt_stops_the_rollback() {
    let mut rig = rig("rb-halt", AUTO, ONE).await;
    round(&rig, 2_000, &[]);
    rig.set(TWO).await;
    seed(&rig, |m| {
        m.halt = Some(Halt {
            node_id: "d1".to_owned(),
            check: "tainted".to_owned(),
            target: version("2.0"),
            report: None,
        });
    });
    restart(&mut rig, AUTO);
    round(&rig, 2_100, &[]);
    round(&rig, 2_115, &two_failed());
    assert_eq!(wanted(&rig), version("2.0"));
    assert!(marks(&rig).bad.is_empty());
}

/// The last good version is recorded on the heartbeats that find the farm converged and
/// baked, and both marks of a rollback, with the desired state, are what a restart finds.
#[tokio::test]
async fn last_good_and_bad_survive_a_restart() {
    let mut rig = rig("rb-restart", AUTO, ONE).await;
    assert_eq!(marks(&rig).last_good, None, "nothing has beaten yet");
    round(&rig, 2_000, &[]);
    assert_eq!(persisted(&rig).last_good, Some(version("1.0")));
    rig.set(TWO).await;
    round(&rig, 2_030, &two_failed());
    assert_eq!(wanted(&rig), version("1.0"));

    restart(&mut rig, AUTO);
    round(&rig, 2_100, &[]);
    let after = marks(&rig);
    assert_eq!(after.last_good, Some(version("1.0")));
    assert_eq!(after.bad, vec![version("2.0")]);
    assert_eq!(wanted(&rig), version("1.0"));
}

/// Desired state set to `min_version` forgets the last good version, in the file too, and a
/// failure at it is not rolled back.
#[tokio::test]
async fn min_version_clears_last_good_on_the_coordinator() {
    let extra = format!("{AUTO}\nmin_version = \"2.0\"");
    let rig = rig("rb-min", &extra, ONE).await;
    round(&rig, 2_000, &[]);
    assert_eq!(marks(&rig).last_good, Some(version("1.0")));
    rig.set(TWO).await;
    round(&rig, 2_015, &[]);
    assert_eq!(marks(&rig).last_good, None);
    assert_eq!(persisted(&rig).last_good, None);
    round(&rig, 2_030, &two_failed());
    assert_eq!(wanted(&rig), version("2.0"));
    assert!(marks(&rig).bad.is_empty());
}

/// An ops store whose file cannot be written, holding `state`.
pub(crate) fn unwritable(state: OpsState) -> OpsStore {
    OpsStore::with_writer(
        state,
        Box::new(|_| {
            Err(OpsError::Io(StoreError::Io {
                path: "ops".into(),
                source: std::io::Error::other("disk full"),
            }))
        }),
    )
}

/// A rig over `ops` instead of the files of its directory.
pub(crate) async fn rig_over(test: &str, extra: &str, body: &str, ops: OpsStore) -> Rig {
    let mut rig = Rig::new(test);
    rig.coordinator = Rig::start(&rig.dir, &rig.clock)
        .with_services(services_of(extra))
        .with_ops(ops);
    rig.set(body).await;
    rig.at(2_000);
    rig
}

/// A last good version that cannot be written is not recorded, and the heartbeats are
/// answered all the same.
#[tokio::test]
async fn a_last_good_that_cannot_be_written_is_not_recorded() {
    let rig = rig_over("rb-lg-fails", AUTO, ONE, unwritable(OpsState::default())).await;
    let replies = round(&rig, 2_000, &[]);
    assert!(replies.iter().all(|r| *r == ServiceReply::Nothing));
    assert_eq!(marks(&rig).last_good, None);
}

/// A mark that cannot be written stays raised in memory, so the version is not ordered, and
/// the desired state is left for the next beat: the rollback is not made on the beat whose
/// write failed.
#[tokio::test]
async fn a_mark_that_cannot_be_written_stays_raised_and_the_rollback_waits() {
    let mut state = OpsState::default();
    state.services.insert(KBF.to_owned(), good());
    let rig = rig_over("rb-mark-fails", AUTO, ONE, unwritable(state)).await;
    rig.set(TWO).await;
    round(&rig, 2_000, &[]);
    let (phase, class) = release();
    let heartbeat = |host: &str, b: Heartbeat| rig.coordinator.heartbeat(who(host), b).reply;
    rig.at(2_015);
    heartbeat("d1", failure(phase, class, "2.0"));
    heartbeat("d2", failure(phase, class, "2.0"));
    assert_eq!(marks(&rig).bad, vec![version("2.0")], "raised in memory");
    assert_eq!(wanted(&rig), version("2.0"), "not rolled back on that beat");
    assert_eq!(heartbeat("d3", beat("1.0")), ServiceReply::Nothing);
    assert_eq!(wanted(&rig), version("1.0"), "the next beat finishes it");
}

/// A manual service uses none of this: nothing is recorded for it, so a farm that never asks
/// for a rollback keeps its files as they were (no ops file, no `ops` in the status).
#[tokio::test]
async fn a_manual_service_records_nothing_and_creates_no_ops_file() {
    let rig = rig("rb-manual-quiet", "", ONE).await;
    round(&rig, 2_000, &[]);
    assert_eq!(
        plan(
            &def(""),
            &desired("1.0", 0, 0),
            &ServiceOps::default(),
            &converged(&all_at_one()),
            &OpsState::default(),
            secs(2_000)
        ),
        vec![]
    );
    assert!(status(&rig).ops.is_empty());
    assert!(!rig.dir.path().join("desired.json.ops").exists());
}
