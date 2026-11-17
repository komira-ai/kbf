//! A node host's turn: the node id the inventory lists for it is checked against what the host
//! reports before anything is installed, and a rollback is an install.

use crate::api::{ErrorCode, Phase, Roll, RollState};
use crate::fakes::Event;
use crate::inventory::Inventory;
use crate::ports::{Installed, PortError};
use crate::updater_tests::{KEY, Rig, all};
use crate::{Timing, Updater};

fn failure(roll: &Roll) -> (ErrorCode, String) {
    let f = roll.failure.as_ref().expect("a failure");
    (f.code, f.detail.clone())
}

/// A roll over `st3` (which lists the node id `st3-node`) and `st2` in an inventory of both.
fn roll_st3(rig: &Rig) -> Roll {
    let inventory = Inventory::parse(&format!(
        "st3 st3.farm.example storage {KEY} st3-node\nst2 st2.farm.example storage {KEY}\n"
    ))
    .expect("inventory");
    let u = Updater::open(inventory, rig.store(), rig.ports(), Timing::default()).expect("open");
    let started = u.start(&all("2.0", "k")).expect("start");
    u.drive(&started.roll.id).expect("drive")
}

/// Catches a host that is installed on although it is not the node the inventory lists for it:
/// the host reports another node id, none, or cannot be asked. The roll stops at it with
/// `node_identity`, naming both ids, and the host's only call is the query: it is not checked
/// for safe-to-stop or installed on, and the host after it is not touched.
#[test]
fn an_identity_mismatch_installs_nothing() {
    let cases: [(Option<&str>, Result<(), PortError>, &str); 3] = [
        (
            Some("other-node"),
            Ok(()),
            "reports node id other-node, not the st3-node",
        ),
        (None, Ok(()), "reports no node id, not the st3-node"),
        (
            Some("st3-node"),
            Err(PortError::unreachable("timeout")),
            "could not be asked its node id",
        ),
    ];
    for (reported, ask, want) in cases {
        let rig = Rig::new("id-mismatch");
        rig.inst.set_identity("st3", reported, None);
        if ask.is_err() {
            rig.inst
                .script_query("st3", vec![Err(PortError::unreachable("timeout"))]);
        }
        let roll = roll_st3(&rig);
        assert_eq!(roll.state, RollState::Failed, "{reported:?}");
        let (code, detail) = failure(&roll);
        assert_eq!(code, ErrorCode::NodeIdentity);
        assert!(detail.contains(want), "{detail}");
        assert_eq!(
            rig.log.touching("st3"),
            [Event::Query("st3".into())],
            "{reported:?}"
        );
        assert!(rig.log.touching("st2").is_empty());
        let phase = roll.hosts.iter().find(|h| h.alias == "st3").expect("st3");
        assert_eq!(phase.phase, Phase::Failed);
    }
}

/// Catches a host that lists a node id being trusted without a look: a storage host whose
/// inventory line has an id is checked against its report, and one without is not asked.
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

/// Catches a host that lists no node id being asked for one: it is checked for safe-to-stop and
/// installed on without a query before the install.
#[test]
fn a_host_that_lists_no_node_id_is_not_asked_for_one() {
    let rig = Rig::new("id-none");
    let inventory =
        Inventory::parse(&format!("st2 st2.farm.example storage {KEY}\n")).expect("inv");
    let u = Updater::open(inventory, rig.store(), rig.ports(), Timing::default()).expect("open");
    let started = u.start(&all("2.0", "k")).expect("start");
    let roll = u.drive(&started.roll.id).expect("drive");
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(
        rig.log.touching("st2")[..2],
        [
            Event::Check("st2".into()),
            Event::Install("st2".into(), "2.0".into())
        ]
    );
}

/// Catches an older version being refused or skipped: every host reports a newer version
/// than the roll's target, and the roll installs on every one of them and ends `done`. A
/// rollback is an install.
#[test]
fn a_roll_to_an_older_version_installs_it() {
    let rig = Rig::new("older");
    for alias in ["fe", "st1", "st3", "st2"] {
        rig.inst
            .set_installed(alias, Installed::Version("9.9".to_owned()));
    }
    let roll = rig.roll(&rig.open());
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(rig.log.installs(), ["fe", "st1", "st3", "st2"]);
}
