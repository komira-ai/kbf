//! The decision names the rule that held. One test per rule, each with a fleet where only
//! that rule fails, so a reply of nothing can be told from every other nothing.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::HostRole;
use kbf_updater::wire::{Outcome, Reply};

use crate::decide::{Decision, Hold, decide_in, decide_why};
use crate::desired::{Desired, KBF};
use crate::fleet::Fleet;
use crate::services::Services;
use crate::testutil::{beat, desired, fleet, id, last, secs, updating, version};

const NOW: Duration = Duration::from_secs(1_000);

const TWO: &str = r#"
[[service]]
name = "server"
[service.roles.daemon]
max_updating = 1
min_up = 0

[[service]]
name = "agent"
after = ["server"]
[service.roles.daemon]
max_updating = 1
min_up = 0
"#;

fn held(why: Hold) -> Decision {
    Decision::Hold(why)
}

fn update(target: &str) -> Decision {
    Decision::Update {
        version: version(target),
    }
}

/// The decision, asserted to carry the reply `decide_in` gives for the same question.
fn why(
    services: &Services,
    all: &BTreeMap<String, Desired>,
    service: &str,
    f: &Fleet,
    host: &str,
    now: Duration,
) -> Decision {
    let decision = decide_why(services, all, service, f, &id(host), now);
    let reply = decide_in(services, all, service, f, &id(host), now);
    match &decision {
        Decision::Update { version } => assert_eq!(
            reply,
            Reply::Update {
                version: version.clone()
            }
        ),
        Decision::Hold(_) => assert_eq!(reply, Reply::Nothing),
    }
    decision
}

/// `kbf`'s decision for `host` under `d` at `now`.
fn kbf(d: &Desired, f: &Fleet, host: &str, now: Duration) -> Decision {
    let all = BTreeMap::from([(KBF.to_owned(), d.clone())]);
    why(&Services::default(), &all, KBF, f, host, now)
}

fn two(want: &Desired) -> (Services, BTreeMap<String, Desired>) {
    let all = ["server", "agent"]
        .map(|name| (name.to_owned(), want.clone()))
        .into();
    (Services::parse(TWO).expect("services"), all)
}

/// A host behind the desired version with every rule satisfied is ordered, and the decision
/// carries the desired version.
#[test]
fn a_host_that_passes_every_rule_is_ordered() {
    let f = fleet(NOW, &[("d1", HostRole::DAEMON, beat("1.0"))]);
    assert_eq!(kbf(&desired("2.0", 1, 1), &f, "d1", NOW), update("2.0"));
}

/// A service no definition names holds with `NoDefinition`, even with a desired state.
#[test]
fn a_service_without_a_definition_holds_as_no_definition() {
    let f = fleet(NOW, &[("d1", HostRole::DAEMON, beat("1.0"))]);
    let all = BTreeMap::from([("ghost".to_owned(), desired("2.0", 1, 1))]);
    let services = Services::default();
    assert_eq!(
        why(&services, &all, "ghost", &f, "d1", NOW),
        held(Hold::NoDefinition)
    );
}

/// A defined service with no desired state holds with `NoDesired`.
#[test]
fn a_service_without_a_desired_state_holds_as_no_desired() {
    let f = fleet(NOW, &[("d1", HostRole::DAEMON, beat("1.0"))]);
    let services = Services::default();
    assert_eq!(
        why(&services, &BTreeMap::new(), KBF, &f, "d1", NOW),
        held(Hold::NoDesired)
    );
}

/// A host the coordinator has not heard from holds with `NotAMember`.
#[test]
fn a_host_that_has_not_heartbeated_holds_as_not_a_member() {
    let f = fleet(NOW, &[("d1", HostRole::DAEMON, beat("1.0"))]);
    assert_eq!(
        kbf(&desired("2.0", 1, 1), &f, "d2", NOW),
        held(Hold::NotAMember)
    );
}

/// A host whose role the definition does not run on holds with `NotAMember`: a storage host
/// asked about a service that runs on daemons.
#[test]
fn a_host_whose_role_the_service_does_not_run_on_holds_as_not_a_member() {
    let want = desired("2.0", 0, 0);
    let (services, all) = two(&want);
    let mut f = Fleet::new(Duration::ZERO);
    f.record_in("server", id("s1"), HostRole::STORAGE, beat("1.0"), NOW);
    assert_eq!(
        why(&services, &all, "server", &f, "s1", NOW),
        held(Hold::NotAMember)
    );
}

/// Within thirty seconds of the coordinator starting the hold is `Quiet`; at thirty seconds
/// the same host is ordered.
#[test]
fn the_first_thirty_seconds_hold_as_quiet() {
    let d = desired("2.0", 1, 1);
    let mut f = Fleet::new(secs(1_000));
    f.record(id("d1"), HostRole::DAEMON, beat("1.0"), secs(1_029));
    assert_eq!(kbf(&d, &f, "d1", secs(1_000)), held(Hold::Quiet));
    assert_eq!(kbf(&d, &f, "d1", secs(1_029)), held(Hold::Quiet));
    assert_eq!(kbf(&d, &f, "d1", secs(1_030)), update("2.0"));
}

/// A host in the middle of an update holds with `Updating`.
#[test]
fn a_host_that_is_updating_holds_as_updating() {
    let f = fleet(
        NOW,
        &[("d1", HostRole::DAEMON, updating(beat("1.0"), "2.0"))],
    );
    assert_eq!(
        kbf(&desired("2.0", 1, 1), &f, "d1", NOW),
        held(Hold::Updating)
    );
}

/// A host with an order outstanding for another service holds with `BusyElsewhere`, and
/// the service that ordered it still answers with the order.
#[test]
fn a_host_busy_with_another_service_holds_as_busy_elsewhere() {
    let want = desired("2.0", 0, 0);
    let (services, all) = two(&want);
    let mut f = Fleet::new(Duration::ZERO);
    f.record_in("server", id("h"), HostRole::DAEMON, beat("1.0"), NOW);
    f.record_in("agent", id("h"), HostRole::DAEMON, beat("1.0"), NOW);
    f.order("server", id("h"), version("2.0"), NOW);
    assert_eq!(
        why(&services, &all, "agent", &f, "h", NOW),
        held(Hold::BusyElsewhere)
    );
    assert_eq!(why(&services, &all, "server", &f, "h", NOW), update("2.0"));
}

/// A host that runs the desired version holds with `UpToDate`.
#[test]
fn a_host_at_the_desired_version_holds_as_up_to_date() {
    let f = fleet(NOW, &[("d1", HostRole::DAEMON, beat("2.0"))]);
    assert_eq!(
        kbf(&desired("2.0", 1, 1), &f, "d1", NOW),
        held(Hold::UpToDate)
    );
}

/// A host that failed an update to the desired version holds with `FailedHere`; a different
/// desired version lifts it.
#[test]
fn a_host_that_failed_this_version_holds_as_failed_here() {
    let f = fleet(
        NOW,
        &[(
            "d1",
            HostRole::DAEMON,
            last(beat("1.0"), "2.0", Outcome::Failed),
        )],
    );
    assert_eq!(
        kbf(&desired("2.0", 1, 1), &f, "d1", NOW),
        held(Hold::FailedHere)
    );
    assert_eq!(kbf(&desired("3.0", 1, 1), &f, "d1", NOW), update("3.0"));
}

/// A storage host asked while another storage host updates (one slot) holds with `NoSlot`.
#[test]
fn a_role_with_no_free_slot_holds_as_no_slot() {
    let f = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, updating(beat("1.0"), "2.0")),
            ("s2", HostRole::STORAGE, beat("1.0")),
        ],
    );
    assert_eq!(
        kbf(&desired("2.0", 0, 0), &f, "s2", NOW),
        held(Hold::NoSlot)
    );
}

/// A frontend asked while a storage host is behind holds with `WaitingForRole`.
#[test]
fn a_role_that_waits_for_another_holds_as_waiting_for_role() {
    let f = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, beat("1.0")),
            ("f1", HostRole::FRONTEND, beat("1.0")),
            ("f2", HostRole::FRONTEND, beat("1.0")),
        ],
    );
    assert_eq!(
        kbf(&desired("2.0", 0, 1), &f, "f1", NOW),
        held(Hold::WaitingForRole)
    );
}

/// A service whose `after` service has a host behind holds with `WaitingForService`; with
/// that host at the desired version the host is ordered.
#[test]
fn a_service_that_waits_for_another_holds_as_waiting_for_service() {
    let want = desired("2.0", 0, 0);
    let (services, all) = two(&want);
    let mut f = Fleet::new(Duration::ZERO);
    f.record_in("server", id("h"), HostRole::DAEMON, beat("1.0"), NOW);
    f.record_in("agent", id("h"), HostRole::DAEMON, beat("1.0"), NOW);
    assert_eq!(
        why(&services, &all, "agent", &f, "h", NOW),
        held(Hold::WaitingForService)
    );
    f.record_in("server", id("h"), HostRole::DAEMON, beat("2.0"), NOW);
    assert_eq!(why(&services, &all, "agent", &f, "h", NOW), update("2.0"));
}

/// The only storage host, with one storage host required up besides it, holds with `MinUp`.
#[test]
fn too_few_others_up_holds_as_min_up() {
    let f = fleet(NOW, &[("s1", HostRole::STORAGE, beat("1.0"))]);
    assert_eq!(kbf(&desired("2.0", 1, 1), &f, "s1", NOW), held(Hold::MinUp));
    assert_eq!(kbf(&desired("2.0", 0, 1), &f, "s1", NOW), update("2.0"));
}

/// The first failing rule is the one named: a host that is both up to date and in a role
/// with no slot is `UpToDate`, and an updating host in a quiet start is `Quiet`.
#[test]
fn the_first_failing_rule_is_the_one_named() {
    let f = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, updating(beat("1.0"), "2.0")),
            ("s2", HostRole::STORAGE, beat("2.0")),
        ],
    );
    let d = desired("2.0", 0, 0);
    assert_eq!(kbf(&d, &f, "s2", NOW), held(Hold::UpToDate));
    let mut quiet = Fleet::new(secs(1_000));
    quiet.record(
        id("s1"),
        HostRole::STORAGE,
        updating(beat("1.0"), "2.0"),
        secs(1_000),
    );
    assert_eq!(kbf(&d, &quiet, "s1", secs(1_000)), held(Hold::Quiet));
}
