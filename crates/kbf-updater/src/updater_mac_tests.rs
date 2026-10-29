//! A `mac-daemon` host's turn: identity and drain as for a daemon, an install that only starts
//! the apply, a poll of `query` until the apply is done for the target, then the daemon's
//! health hold, the undrain and the full health. A failed apply and a deadline leave the node
//! drained; a restart mid-poll polls again and never installs again.

use std::time::Duration;

use crate::api::{ErrorCode, HostRoll, Hosts, Phase, Roll, RollState};
use crate::fakes::Event;
use crate::inventory::Inventory;
use crate::ports::{Apply, InstallAnswer, Installed, PortError};
use crate::record::{FORMAT, Record, roll_id};
use crate::updater_tests::{KEY, Rig, request};
use crate::{Timing, Updater};

const NOTE: &str = "node mac-node (mac) is left drained";

fn inventory() -> Inventory {
    Inventory::parse(&format!(
        "dm  dm.farm.example daemon {KEY} dm-node\n\
         mac mac.farm.example mac-daemon {KEY} mac-node\n\
         st2 st2.farm.example storage {KEY}\n"
    ))
    .expect("inventory")
}

fn open(rig: &Rig) -> Updater {
    Updater::open(inventory(), rig.store(), rig.ports(), Timing::default()).expect("open")
}

fn roll_hosts(rig: &Rig, version: &str, hosts: &[&str]) -> Roll {
    let u = open(rig);
    let hosts = Hosts::Aliases(hosts.iter().map(|h| (*h).to_owned()).collect());
    let started = u.start(&request(version, hosts, "k")).expect("start");
    u.drive(&started.roll.id).expect("drive")
}

/// The roll's events for `alias`, as short words.
fn words(rig: &Rig, alias: &str) -> Vec<String> {
    rig.log
        .touching(alias)
        .into_iter()
        .map(|e| e.word())
        .collect()
}

fn failure(roll: &Roll) -> (ErrorCode, String) {
    let f = roll.failure.as_ref().expect("a failure");
    (f.code, f.detail.clone())
}

fn phase_of(roll: &Roll, alias: &str) -> Phase {
    roll.hosts
        .iter()
        .find(|h| h.alias == alias)
        .expect("host")
        .phase
}

/// A running roll over `mac` alone with the host in `phase` after `attempts` installs, as a
/// process that stopped there left it.
fn stopped_at(rig: &Rig, phase: Phase, attempts: u32) -> Updater {
    let rec = Record {
        format: FORMAT,
        seq: 1,
        request: request("2.0", Hosts::Aliases(vec!["mac".to_owned()]), "k"),
        roll: Roll {
            id: roll_id(1),
            state: RollState::Running,
            hosts: vec![HostRoll {
                alias: "mac".to_owned(),
                phase,
                attempts,
                detail: None,
            }],
            failure: None,
        },
        resumes: 0,
        abort_requested: false,
    };
    rig.store().save(&rec).expect("save");
    open(rig)
}

fn running_then(n: usize, end: Option<Apply>) -> (Vec<Option<Apply>>, Option<Apply>) {
    (vec![Some(Apply::Running); n], end)
}

/// Proves the order of a Mac host's turn: its identity is read, it is checked and drained, the
/// install is sent, `query` is polled while the apply runs (two answers of `running`, then
/// `done` for the target, one poll apart), the held health is asked, the node is undrained,
/// and the full health is the last call.
#[test]
fn a_mac_host_goes_drain_install_poll_health_undrain() {
    let rig = Rig::new("mac-order");
    let (first, end) = running_then(2, Some(Apply::Done));
    rig.inst.script_apply("mac", first, end);
    let roll = roll_hosts(&rig, "2.0", &["mac"]);
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert_eq!(
        words(&rig, "mac"),
        [
            "query",
            "check",
            "drain none",
            "leases",
            "install 2.0",
            "query",
            "query",
            "query",
            "health",
            "undrain",
            "health"
        ]
    );
    assert_eq!(rig.elapsed(), Duration::from_secs(10));
    assert_eq!(phase_of(&roll, "mac"), Phase::Done);
}

/// Proves a failed apply stops the roll at the Mac: the reason the host gave is in the
/// failure, the node is named as left drained, the node is not undrained, health is not
/// asked, and the host after it is not touched.
#[test]
fn a_failed_apply_leaves_the_node_drained_and_names_the_reason() {
    let rig = Rig::new("mac-failed");
    rig.inst.script_apply(
        "mac",
        vec![Some(Apply::Running)],
        Some(Apply::Failed("health_timeout:rolled_back".to_owned())),
    );
    let roll = roll_hosts(&rig, "2.0", &["mac", "st2"]);
    assert_eq!(roll.state, RollState::Failed);
    let (code, detail) = failure(&roll);
    assert_eq!(code, ErrorCode::InstallFailed);
    assert_eq!(
        detail,
        format!("the apply on mac failed: health_timeout:rolled_back; {NOTE}")
    );
    let events = words(&rig, "mac");
    assert!(!events.contains(&"undrain".to_owned()), "{events:?}");
    assert!(!events.contains(&"health".to_owned()), "{events:?}");
    assert_eq!(phase_of(&roll, "mac"), Phase::Failed);
    assert!(rig.log.touching("st2").is_empty());
}

/// Proves the poll is bounded: an apply that is `running` for ever ends the roll after exactly
/// the deadline (600 s by default), with the code for a host that did not get there, the last
/// thing the host said, and the node left drained and not undrained.
#[test]
fn an_apply_that_never_gets_done_times_out_and_stays_drained() {
    let rig = Rig::new("mac-timeout");
    rig.inst.script_apply("mac", vec![], Some(Apply::Running));
    let roll = roll_hosts(&rig, "2.0", &["mac"]);
    assert_eq!(roll.state, RollState::Failed);
    let (code, detail) = failure(&roll);
    assert_eq!(code, ErrorCode::HealthTimeout);
    assert_eq!(
        detail,
        format!("the apply on mac was not done for 2.0 after 600 s: the apply is running; {NOTE}")
    );
    assert_eq!(rig.elapsed(), Duration::from_secs(600));
    let events = words(&rig, "mac");
    assert!(!events.contains(&"undrain".to_owned()), "{events:?}");
    assert!(!events.contains(&"health".to_owned()), "{events:?}");
}

/// Proves the deadline is the timing's, not a constant: a deadline of 12 s and a poll of 4 s
/// ask `query` at 0, 4, 8 and 12 s and then give up.
#[test]
fn the_apply_poll_and_deadline_come_from_the_timing() {
    let rig = Rig::new("mac-timing");
    rig.inst.script_apply("mac", vec![], None);
    let timing = Timing {
        apply_poll: Duration::from_secs(4),
        apply_deadline: Duration::from_secs(12),
        ..Timing::default()
    };
    let u = Updater::open(inventory(), rig.store(), rig.ports(), timing).expect("open");
    let started = u
        .start(&request("2.0", Hosts::Aliases(vec!["mac".into()]), "k"))
        .expect("start");
    let roll = u.drive(&started.roll.id).expect("drive");
    let (code, detail) = failure(&roll);
    assert_eq!(code, ErrorCode::HealthTimeout);
    assert!(
        detail.contains("after 12 s: mac reports no apply"),
        "{detail}"
    );
    assert_eq!(rig.elapsed(), Duration::from_secs(12));
    let polls = words(&rig, "mac").iter().filter(|w| *w == "query").count();
    assert_eq!(polls, 1 + 4, "the identity and the four polls");
}

/// Proves `done` alone is not enough: the host must say the target is what is installed. A
/// `done` left by an earlier apply (the host still reports the old version, an install in
/// progress or none) is waited out until the version is the target's, and a host that does not
/// answer a poll is asked again.
#[test]
fn done_for_another_version_and_a_silent_host_are_waited_for() {
    let rig = Rig::new("mac-stale-done");
    rig.inst.script_query(
        "mac",
        vec![
            Ok(Installed::Version("3.0".into())),
            Ok(Installed::Version("3.0".into())),
            Err(PortError::unreachable("timeout")),
            Ok(Installed::InProgress),
            Ok(Installed::NotInstalled),
        ],
    );
    let roll = roll_hosts(&rig, "2.0", &["mac"]);
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    let polls = words(&rig, "mac").iter().filter(|w| *w == "query").count();
    assert_eq!(
        polls, 6,
        "the identity, four polls that are not it, and the answer"
    );
    assert_eq!(rig.elapsed(), Duration::from_secs(20));
}

/// Proves a rollback takes the same path as an update: the Mac runs 3.0, the roll asks for
/// 1.0, and the host is drained, installed with 1.0, polled until it reports 1.0 and done,
/// and given back. Nothing refuses the older version.
#[test]
fn an_older_version_goes_through_the_same_path() {
    let rig = Rig::new("mac-older");
    rig.inst
        .set_installed("mac", Installed::Version("3.0".into()));
    let (first, end) = running_then(1, Some(Apply::Done));
    rig.inst.script_apply("mac", first, end);
    let roll = roll_hosts(&rig, "1.0", &["mac"]);
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert_eq!(
        words(&rig, "mac"),
        [
            "query",
            "check",
            "drain none",
            "leases",
            "install 1.0",
            "query",
            "query",
            "health",
            "undrain",
            "health"
        ]
    );
}

/// Proves a restart in `waiting_healthy` (the install was answered, the apply is going on)
/// polls and does not install again: no install, no new drain, the poll sees `running` and
/// then `done`, and the node is given back.
#[test]
fn a_resumed_roll_mid_poll_does_not_install_again() {
    let rig = Rig::new("mac-resume-poll");
    rig.inst
        .set_installed("mac", Installed::Version("2.0".into()));
    rig.inst.begin_apply("mac");
    rig.inst
        .script_apply("mac", vec![Some(Apply::Running)], Some(Apply::Done));
    let u = stopped_at(&rig, Phase::WaitingHealthy, 1);
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert_eq!(
        words(&rig, "mac"),
        ["query", "query", "health", "undrain", "health"]
    );
    assert_eq!(roll.hosts[0].attempts, 1);
}

/// Proves a resumed poll still halts on a failed apply: the apply failed while the updater was
/// down, so the resumed roll fails with the reason, names the node, and installs nothing.
#[test]
fn a_resumed_roll_finds_the_apply_failed_and_halts() {
    let rig = Rig::new("mac-resume-failed");
    rig.inst.begin_apply("mac");
    rig.inst
        .script_apply("mac", vec![], Some(Apply::Failed("apply_lost".to_owned())));
    let u = stopped_at(&rig, Phase::WaitingHealthy, 1);
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Failed);
    assert_eq!(
        failure(&roll).1,
        format!("the apply on mac failed: apply_lost; {NOTE}")
    );
    assert_eq!(words(&rig, "mac"), ["query"]);
}

/// Proves a restart between the install call and its answer asks the host first: the host is
/// found `installing` with its apply running, so the roll polls (no second install), and when
/// the apply was never started (the old version, nothing running) it installs once more.
#[test]
fn a_resumed_install_is_not_repeated_when_the_apply_is_running() {
    let rig = Rig::new("mac-resume-installing");
    rig.inst
        .set_installed("mac", Installed::Version("1.0".into()));
    rig.inst.begin_apply("mac");
    rig.inst
        .script_apply("mac", vec![Some(Apply::Running)], Some(Apply::Done));
    rig.inst.script_query(
        "mac",
        vec![
            Ok(Installed::InProgress),
            Ok(Installed::Version("2.0".into())),
        ],
    );
    let u = stopped_at(&rig, Phase::Installing, 1);
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert!(rig.log.installs().is_empty());

    let rig = Rig::new("mac-resume-never-started");
    rig.inst
        .set_installed("mac", Installed::Version("1.0".into()));
    rig.inst.begin_apply("mac");
    let u = stopped_at(&rig, Phase::Installing, 1);
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert_eq!(rig.log.installs(), ["mac"]);
    assert_eq!(roll.hosts[0].attempts, 2);
}

/// Proves a host found `installing` with the target already installed is still waited for as an
/// apply: when its apply failed the roll halts with the reason (no install, health never asked),
/// and when the apply is done the roll goes on to the health hold and gives the node back.
#[test]
fn a_resumed_install_with_the_target_present_still_waits_for_the_apply() {
    let rig = Rig::new("mac-resume-installing-failed");
    rig.inst
        .set_installed("mac", Installed::Version("2.0".into()));
    rig.inst.begin_apply("mac");
    rig.inst.script_apply(
        "mac",
        vec![],
        Some(Apply::Failed("health_timeout".to_owned())),
    );
    let u = stopped_at(&rig, Phase::Installing, 1);
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Failed);
    assert_eq!(
        failure(&roll).1,
        format!("the apply on mac failed: health_timeout; {NOTE}")
    );
    assert!(rig.log.installs().is_empty());
    assert!(!words(&rig, "mac").contains(&"health".to_owned()));

    let rig = Rig::new("mac-resume-installing-done");
    rig.inst
        .set_installed("mac", Installed::Version("2.0".into()));
    rig.inst.begin_apply("mac");
    rig.inst.script_apply("mac", vec![], Some(Apply::Done));
    let u = stopped_at(&rig, Phase::Installing, 1);
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert!(rig.log.installs().is_empty());
    assert_eq!(
        words(&rig, "mac"),
        ["query", "query", "health", "undrain", "health"]
    );
}

/// Proves an install whose answer was lost does not become a second install while the apply
/// runs: the call gets no answer, the roll asks the host, finds the apply running, and polls.
#[test]
fn an_install_with_no_answer_is_polled_not_repeated() {
    let rig = Rig::new("mac-lost-answer");
    rig.inst
        .script_install("mac", vec![Err(PortError::unreachable("timeout"))], Ok(()));
    rig.inst
        .set_installed("mac", Installed::Version("1.0".into()));
    rig.inst.begin_apply("mac");
    rig.inst
        .script_apply("mac", vec![None, Some(Apply::Running)], Some(Apply::Done));
    // The identity check, the question after the lost answer, and the poll.
    rig.inst.script_query(
        "mac",
        vec![
            Ok(Installed::Version("1.0".into())),
            Ok(Installed::InProgress),
            Ok(Installed::Version("2.0".into())),
        ],
    );
    let roll = roll_hosts(&rig, "2.0", &["mac"]);
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert_eq!(rig.log.installs(), ["mac"]);
}

/// Proves the two roles do not interfere in one roll: the Linux daemon is installed
/// synchronously and is never polled (its only query is the identity), the Mac is polled, the
/// Linux host finishes before the Mac starts, and a storage host after both is rolled as
/// before.
#[test]
fn mac_and_linux_hosts_of_one_inventory_do_not_interfere() {
    let rig = Rig::new("mac-and-linux");
    let (first, end) = running_then(1, Some(Apply::Done));
    rig.inst.script_apply("mac", first, end);
    let roll = roll_hosts(&rig, "2.0", &["dm", "mac", "st2"]);
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert_eq!(
        words(&rig, "dm"),
        [
            "query",
            "check",
            "drain none",
            "leases",
            "install 2.0",
            "health",
            "undrain",
            "health"
        ]
    );
    assert_eq!(
        words(&rig, "mac"),
        [
            "query",
            "check",
            "drain none",
            "leases",
            "install 2.0",
            "query",
            "query",
            "health",
            "undrain",
            "health"
        ]
    );
    assert_eq!(words(&rig, "st2"), ["check", "install 2.0", "health"]);
    let events = rig.log.events();
    let last_dm = events
        .iter()
        .rposition(|e| matches!(e, Event::Health(a) if a == "dm"));
    let first_mac = events
        .iter()
        .position(|e| matches!(e, Event::Check(a) if a == "mac"));
    assert!(last_dm < first_mac);
    assert_eq!(rig.elapsed(), Duration::from_secs(5));
}

/// Proves a Mac that answers an install as a Linux host does (finished at once) fails the roll
/// as the wrong kind of host: the node it holds is named as left drained, and it is not
/// polled, undrained or checked.
#[test]
fn a_synchronous_answer_from_a_mac_host_is_an_error() {
    let rig = Rig::new("mac-sync");
    rig.inst
        .script_install_answers("mac", vec![], Ok(InstallAnswer::Installed));
    let roll = roll_hosts(&rig, "2.0", &["mac"]);
    assert_eq!(roll.state, RollState::Failed);
    let (code, detail) = failure(&roll);
    assert_eq!(code, ErrorCode::InstallFailed);
    assert_eq!(
        detail,
        format!("mac is listed as a mac-daemon host but finished the install at once; {NOTE}")
    );
    assert_eq!(
        words(&rig, "mac"),
        ["query", "check", "drain none", "leases", "install 2.0"]
    );
}

/// Proves a Linux daemon that answers `apply_started` fails the roll the same way, naming its
/// node as left drained, and a storage host that does so fails with no node to name.
#[test]
fn apply_started_from_a_linux_host_is_an_error() {
    let rig = Rig::new("linux-apply-started");
    rig.inst
        .script_install_answers("dm", vec![], Ok(InstallAnswer::ApplyStarted));
    let roll = roll_hosts(&rig, "2.0", &["dm", "mac"]);
    assert_eq!(roll.state, RollState::Failed);
    let (code, detail) = failure(&roll);
    assert_eq!(code, ErrorCode::InstallFailed);
    assert_eq!(
        detail,
        "dm is listed as a non-Mac host but started an apply; \
         node dm-node (dm) is left drained"
    );
    assert!(rig.log.touching("mac").is_empty());

    let rig = Rig::new("storage-apply-started");
    rig.inst
        .script_install_answers("st2", vec![], Ok(InstallAnswer::ApplyStarted));
    let roll = roll_hosts(&rig, "2.0", &["st2"]);
    let (code, detail) = failure(&roll);
    assert_eq!(code, ErrorCode::InstallFailed);
    assert_eq!(
        detail,
        "st2 is listed as a non-Mac host but started an apply"
    );
}

/// Proves a Mac that already runs the target (`already_installed`) is not polled for an apply
/// that never started: it goes straight to the health hold, even when its last apply was one
/// that failed, and its node is given back.
#[test]
fn a_mac_that_already_runs_the_target_is_not_polled() {
    let rig = Rig::new("mac-already");
    rig.inst
        .script_install_answers("mac", vec![], Ok(InstallAnswer::AlreadyInstalled));
    rig.inst.script_apply(
        "mac",
        vec![],
        Some(Apply::Failed("health_timeout:rolled_back".to_owned())),
    );
    let roll = roll_hosts(&rig, "2.0", &["mac"]);
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert_eq!(
        words(&rig, "mac"),
        [
            "query",
            "check",
            "drain none",
            "leases",
            "install 2.0",
            "health",
            "undrain",
            "health"
        ]
    );
}

/// Proves a Mac is identified like a daemon: listed without a node id it is refused before any
/// call to the host, and reporting another node's id it is refused before the drain.
#[test]
fn a_mac_host_is_checked_for_its_node_id_like_a_daemon() {
    let rig = Rig::new("mac-identity");
    let inventory =
        Inventory::parse(&format!("mac mac.farm.example mac-daemon {KEY}\n")).expect("inventory");
    let u = Updater::open(inventory, rig.store(), rig.ports(), Timing::default()).expect("open");
    let started = u
        .start(&request("2.0", Hosts::Aliases(vec!["mac".into()]), "k"))
        .expect("start");
    let roll = u.drive(&started.roll.id).expect("drive");
    assert_eq!(failure(&roll).0, ErrorCode::NodeIdentity);
    assert!(rig.log.events().is_empty());

    let rig = Rig::new("mac-identity-other");
    rig.inst.set_identity("mac", Some("another-node"), None);
    let roll = roll_hosts(&rig, "2.0", &["mac"]);
    let (code, detail) = failure(&roll);
    assert_eq!(code, ErrorCode::NodeIdentity);
    assert!(detail.contains("reports node id another-node"), "{detail}");
    assert_eq!(words(&rig, "mac"), ["query"]);
}

/// Proves the health hold after the apply is the daemon's: a Mac whose held health waits is
/// asked again, and a failure there names the node as left drained.
#[test]
fn a_mac_that_never_runs_the_target_commit_leaves_the_node_drained() {
    let rig = Rig::new("mac-health");
    rig.health.script_held(
        "mac",
        vec![],
        Ok(crate::ports::Health::Waiting(
            "mac runs build abc, not the target's".to_owned(),
        )),
    );
    let roll = roll_hosts(&rig, "2.0", &["mac"]);
    let (code, detail) = failure(&roll);
    assert_eq!(code, ErrorCode::HealthTimeout);
    assert!(detail.contains("780 s"), "{detail}");
    assert!(detail.ends_with(NOTE), "{detail}");
    assert!(!words(&rig, "mac").contains(&"undrain".to_owned()));
}
