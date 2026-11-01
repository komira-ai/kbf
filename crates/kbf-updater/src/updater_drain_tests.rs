//! A daemon host's turn: its identity is checked before anything is drained, its node is
//! drained and its leases waited for, it is installed, and the node is given back only once
//! the host runs the target. Every failure after the drain leaves the node drained and names it.

use std::time::Duration;

use crate::Timing;
use crate::Updater;
use crate::api::{ErrorCode, Hosts, Phase, Roll, RollState};
use crate::fakes::Event;
use crate::inventory::Inventory;
use crate::ports::{Health, Installed, PortError};
use crate::updater_restart_tests::{OLD, TARGET, record, restart};
use crate::updater_tests::{KEY, Rig, all, request};

/// The roll's events for the daemon host `dm`, as short words.
fn dm_events(rig: &Rig) -> Vec<String> {
    rig.log
        .touching("dm")
        .into_iter()
        .map(|e| match e {
            Event::Install(..) => "install".to_owned(),
            other => other.word(),
        })
        .collect()
}

fn failure(roll: &Roll) -> (ErrorCode, String) {
    let f = roll.failure.as_ref().expect("a failure");
    (f.code, f.detail.clone())
}

fn dm_phase(roll: &Roll) -> Phase {
    roll.hosts
        .iter()
        .find(|h| h.alias == "dm")
        .expect("dm")
        .phase
}

fn roll_dm(rig: &Rig) -> Roll {
    let u = rig.open();
    let started = u
        .start(&request(
            "2.0",
            Hosts::Aliases(vec!["dm".into(), "st2".into()]),
            "k",
        ))
        .expect("start");
    u.drive(&started.roll.id).expect("drive")
}

const NOTE: &str = "node dm-node (dm) is left drained";

/// Catches a drain aimed at a host that is not the node the inventory lists: the host
/// reports another node id, none, or cannot be asked. The roll stops at it with
/// `node_identity`, naming both ids, and the host's only call is the query: it is not checked,
/// drained or installed on, and the host after it is not touched.
#[test]
fn an_identity_mismatch_drains_nothing() {
    let cases: [(Option<&str>, Result<(), PortError>, &str); 3] = [
        (
            Some("other-node"),
            Ok(()),
            "reports node id other-node, not the dm-node",
        ),
        (None, Ok(()), "reports no node id, not the dm-node"),
        (
            Some("dm-node"),
            Err(PortError::unreachable("timeout")),
            "could not be asked its node id",
        ),
    ];
    for (reported, ask, want) in cases {
        let rig = Rig::new("id-mismatch");
        rig.inst.set_identity("dm", reported, None);
        if ask.is_err() {
            rig.inst
                .script_query("dm", vec![Err(PortError::unreachable("timeout"))]);
        }
        let roll = roll_dm(&rig);
        assert_eq!(roll.state, RollState::Failed, "{reported:?}");
        let (code, detail) = failure(&roll);
        assert_eq!(code, ErrorCode::NodeIdentity);
        assert!(detail.contains(want), "{detail}");
        assert!(!detail.contains("left drained"), "{detail}");
        assert_eq!(dm_events(&rig), ["query"], "{reported:?}");
        assert!(rig.log.touching("st2").is_empty());
        assert_eq!(dm_phase(&roll), Phase::Failed);
    }
}

/// Catches a daemon host that is drained without an id to check: listed without a node id,
/// it is refused with a reason that says what to add, before the host is even asked.
#[test]
fn a_daemon_host_listed_without_a_node_id_is_refused() {
    let rig = Rig::new("id-missing");
    let inventory = Inventory::parse(&format!(
        "dm dm.farm.example daemon {KEY}\nst2 st2.farm.example storage {KEY}\n"
    ))
    .expect("inventory");
    let u = Updater::open(inventory, rig.store(), rig.ports(), Timing::default()).expect("open");
    let started = u.start(&all("2.0", "k")).expect("start");
    let roll = u.drive(&started.roll.id).expect("drive");
    let (code, detail) = failure(&roll);
    assert_eq!(code, ErrorCode::NodeIdentity);
    assert!(
        detail.contains("dm is listed without a node id"),
        "{detail}"
    );
    assert!(rig.log.events().is_empty(), "{:?}", rig.log.events());
}

/// Catches a host that lists a node id being trusted without a look: a storage host whose
/// inventory line has an id is checked against its report too, and one without is not asked.
#[test]
fn a_storage_host_is_checked_when_it_lists_a_node_id() {
    let rig = Rig::new("id-storage");
    let inventory = Inventory::parse(&format!(
        "st1 st1.farm.example storage {KEY} st1-node\nst2 st2.farm.example storage {KEY}\n"
    ))
    .expect("inventory");
    rig.inst.set_identity("st1", Some("someone-else"), None);
    let u = Updater::open(inventory, rig.store(), rig.ports(), Timing::default()).expect("open");
    let started = u.start(&all("2.0", "k")).expect("start");
    let roll = u.drive(&started.roll.id).expect("drive");
    assert_eq!(failure(&roll).0, ErrorCode::NodeIdentity);
    assert_eq!(rig.log.touching("st1"), [Event::Query("st1".into())]);
    assert!(rig.log.touching("st2").is_empty());
}

/// Catches an install that does not wait for the node's leases: the leases are read until
/// they are zero (3, 2, then 0, one poll apart), and only then is the host installed. With no
/// deadline set the frontend is asked to drain with no deadline of its own, and the roll waited
/// two polls.
#[test]
fn a_drained_daemon_is_installed_only_once_its_leases_are_zero() {
    let rig = Rig::new("drain-leases");
    rig.drain.script_leases("dm", vec![Ok(3), Ok(2)], Ok(0));
    let roll = roll_dm(&rig);
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(
        dm_events(&rig),
        [
            "query",
            "check",
            "drain none",
            "leases",
            "leases",
            "leases",
            "install",
            "health",
            "undrain",
            "health"
        ]
    );
    assert_eq!(rig.elapsed(), Duration::from_secs(10));
}

/// Catches a drain that gives up on leases that run past the old default of 600 s: with no
/// deadline set, a node that holds a lease for 1000 s (200 polls) is waited for, nothing is
/// installed in that time, and the host is installed after the poll that reads 0.
#[test]
fn with_no_deadline_the_drain_waits_past_600_seconds_and_installs_when_the_leases_end() {
    let rig = Rig::new("drain-no-deadline");
    rig.drain.script_leases("dm", vec![Ok(1); 200], Ok(0));
    let roll = roll_dm(&rig);
    assert_eq!(roll.state, RollState::Done);
    let events = dm_events(&rig);
    let installed = events.iter().position(|e| e == "install").expect("install");
    let looks = events[..installed]
        .iter()
        .filter(|e| *e == "leases")
        .count();
    assert_eq!(looks, 201);
    assert_eq!(rig.elapsed(), Duration::from_secs(1000));
    assert!(events.contains(&"undrain".to_owned()));
}

/// Catches a configured deadline that installs over running work: a node keeps one lease, the
/// operator's deadline of 600 s passes, and the roll stops at the host with `drain_failed`
/// saying how many leases were held, that nothing was installed and how to go on; the node is
/// left drained (named, never undrained), nothing is installed on the host and the next host
/// is not touched. The wait ended at the deadline, not before.
#[test]
fn a_deadline_that_passes_installs_nothing_and_leaves_the_node_drained() {
    let rig = Rig::new("drain-deadline");
    rig.drain.script_leases("dm", vec![], Ok(1));
    let timing = Timing {
        drain_deadline: Some(Duration::from_secs(600)),
        ..Timing::default()
    };
    let u = rig.open_timed(timing);
    let started = u
        .start(&request(
            "2.0",
            Hosts::Aliases(vec!["dm".into(), "st2".into()]),
            "k",
        ))
        .expect("start");
    let roll = u.drive(&started.roll.id).expect("drive");
    assert_eq!(roll.state, RollState::Failed);
    let (code, detail) = failure(&roll);
    assert_eq!(code, ErrorCode::DrainFailed);
    assert_eq!(
        detail,
        format!(
            "dm still holds 1 leases after the drain deadline of 600 s; nothing was installed \
             and the leases run on; start a new roll to drain it again and wait again; {NOTE}"
        )
    );
    assert_eq!(dm_phase(&roll), Phase::Failed);
    let events = dm_events(&rig);
    assert!(!events.contains(&"install".to_owned()));
    assert!(!events.contains(&"undrain".to_owned()));
    assert_eq!(events[2], "drain 600");
    assert_eq!(rig.elapsed(), Duration::from_secs(600));
    assert!(rig.log.installs().is_empty());
    assert!(rig.log.touching("st2").is_empty());
}

/// Catches a node given back before it runs the target: the host reports the old build
/// twice, then the target's, and the undrain is the call after the third (held) health
/// check, never before. The check after the undrain is the one that asks for `serving`.
#[test]
fn the_node_is_undrained_only_once_the_host_runs_the_target() {
    let rig = Rig::new("undrain-after");
    let old = |why: &str| Ok(Health::Waiting(why.to_owned()));
    rig.health.script_held(
        "dm",
        vec![
            old("dm runs build abc, not the target's"),
            old("dm is not connected"),
        ],
        Ok(Health::Healthy),
    );
    rig.health.script(
        "dm",
        vec![old("dm is cordoned, not serving")],
        Ok(Health::Healthy),
    );
    let roll = roll_dm(&rig);
    assert_eq!(roll.state, RollState::Done);
    let events = dm_events(&rig);
    let tail: Vec<&str> = events
        .iter()
        .skip_while(|e| *e != "install")
        .map(String::as_str)
        .collect();
    assert_eq!(
        tail,
        [
            "install", "health", "health", "health", "undrain", "health", "health"
        ]
    );
}

/// Catches an install that fails after the drain putting the node back: it stays drained,
/// the roll stops at the host with `install_failed`, the failure names the node, nothing is
/// undrained and the next host is not touched.
#[test]
fn a_failed_install_leaves_the_node_drained() {
    let rig = Rig::new("install-fails");
    rig.inst
        .script_install("dm", vec![], Err(PortError("dpkg: lock held".into())));
    let roll = roll_dm(&rig);
    assert_eq!(roll.state, RollState::Failed);
    assert_eq!(
        failure(&roll),
        (ErrorCode::InstallFailed, format!("dpkg: lock held; {NOTE}"))
    );
    assert_eq!(
        roll.hosts[0].detail.as_deref(),
        Some(&*format!("dpkg: lock held; {NOTE}"))
    );
    assert_eq!(dm_events(&rig).last().map(String::as_str), Some("install"));
    assert!(!dm_events(&rig).contains(&"undrain".to_owned()));
    assert!(rig.log.touching("st2").is_empty());
}

/// Catches a host that never runs the target being given back to placement: the health wait
/// ends in `health_timeout`, naming the node, with no undrain.
#[test]
fn a_host_that_never_runs_the_target_is_left_drained() {
    let rig = Rig::new("never-healthy");
    rig.health.script_held(
        "dm",
        vec![],
        Ok(Health::Waiting(
            "dm runs build abc, not the target's".into(),
        )),
    );
    let roll = roll_dm(&rig);
    let (code, detail) = failure(&roll);
    assert_eq!(code, ErrorCode::HealthTimeout);
    assert!(
        detail.contains("not the target's") && detail.ends_with(NOTE),
        "{detail}"
    );
    assert!(!dm_events(&rig).contains(&"undrain".to_owned()));
    assert!(rig.log.touching("st2").is_empty());
}

/// Catches a failed undrain passing for a finished host: the roll stops with
/// `undrain_failed` naming the node, the host is not `done` and the next is not touched.
#[test]
fn a_failed_undrain_halts_the_roll_and_names_the_node() {
    let rig = Rig::new("undrain-fails");
    rig.drain.script_undrain(
        "dm",
        vec![],
        Err(PortError("the operator API answered 500".into())),
    );
    let roll = roll_dm(&rig);
    let (code, detail) = failure(&roll);
    assert_eq!(code, ErrorCode::UndrainFailed);
    assert!(
        detail.contains("the undrain failed") && detail.ends_with(NOTE),
        "{detail}"
    );
    assert_eq!(dm_phase(&roll), Phase::Failed);
    assert!(rig.log.touching("st2").is_empty());
}

/// Catches a drain that fails being installed over: a refusal of the drain, and a failure to
/// read the node's leases, each stop the roll with `drain_failed` before any install, naming
/// the node, which may be left cordoned.
#[test]
fn a_drain_that_fails_stops_the_roll_before_the_install() {
    for (name, drain_fails) in [("drain-refused", true), ("leases-unreadable", false)] {
        let rig = Rig::new(name);
        let down = PortError::unreachable("the operator API answered 503");
        if drain_fails {
            rig.drain.script_drain("dm", vec![], Err(down));
        } else {
            rig.drain.script_leases("dm", vec![], Err(down));
        }
        let roll = roll_dm(&rig);
        let (code, detail) = failure(&roll);
        assert_eq!(code, ErrorCode::DrainFailed, "{name}");
        assert!(
            detail.contains("answered 503") && detail.ends_with(NOTE),
            "{detail}"
        );
        assert!(!dm_events(&rig).contains(&"install".to_owned()), "{name}");
        assert!(!dm_events(&rig).contains(&"undrain".to_owned()), "{name}");
        assert!(rig.log.touching("st2").is_empty());
    }
}

/// Catches an older version being refused or skipped: every host reports a newer version
/// than the roll's target, and the roll installs on every one of them and ends `done`. A
/// rollback is an install.
#[test]
fn a_roll_to_an_older_version_installs_it() {
    let rig = Rig::new("older");
    for alias in ["fe", "st1", "dm", "st2"] {
        rig.inst
            .set_installed(alias, Installed::Version("9.9".to_owned()));
    }
    let roll = rig.roll(&rig.open());
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(rig.log.installs(), ["fe", "st1", "dm", "st2"]);
}

/// Catches a restart that forgets a drained node: the updater stopped with `dm` waiting for
/// health, so its node is still drained; the new process waits for health and gives the node
/// back, without draining it again or installing.
#[test]
fn a_restart_while_waiting_gives_the_drained_node_back() {
    let rig = Rig::new("restart-held");
    rig.inst
        .set_installed("dm", Installed::Version(TARGET.into()));
    let u = restart(
        &rig,
        &record(
            [
                (Phase::Done, 1),
                (Phase::Done, 1),
                (Phase::WaitingHealthy, 1),
                (Phase::Pending, 0),
            ],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(dm_events(&rig), ["health", "undrain", "health"]);
}

/// Catches a restart after the install call: `dm` is found `installing`, still on the old
/// version, so it is installed once more with its node still held, and a failure of that
/// names the node.
#[test]
fn a_restart_while_installing_keeps_the_node_held() {
    let rig = Rig::new("restart-installing");
    rig.inst.set_installed("dm", Installed::Version(OLD.into()));
    rig.inst
        .script_install("dm", vec![], Err(PortError("dpkg: lock held".into())));
    let u = restart(
        &rig,
        &record(
            [
                (Phase::Done, 1),
                (Phase::Done, 1),
                (Phase::Installing, 1),
                (Phase::Pending, 0),
            ],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(failure(&roll).1, format!("dpkg: lock held; {NOTE}"));
    assert_eq!(dm_events(&rig), ["query", "install"]);
}

/// Catches a silent host losing the node it holds from the report: `dm` is found `installing`
/// and does not answer a query for the whole bound, the roll is interrupted (not failed), and
/// the host's detail names the node that is left drained.
#[test]
fn an_interrupted_host_names_the_node_it_leaves_drained() {
    let rig = Rig::new("restart-silent");
    rig.inst.script_query(
        "dm",
        (0..1000)
            .map(|_| Err(PortError::unreachable("timeout")))
            .collect(),
    );
    let u = restart(
        &rig,
        &record(
            [
                (Phase::Done, 1),
                (Phase::Done, 1),
                (Phase::Installing, 1),
                (Phase::Pending, 0),
            ],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Interrupted);
    let detail = roll.hosts[2].detail.clone().expect("detail");
    assert!(
        detail.contains("did not answer") && detail.ends_with(NOTE),
        "{detail}"
    );
    assert!(!dm_events(&rig).contains(&"undrain".to_owned()));
}
