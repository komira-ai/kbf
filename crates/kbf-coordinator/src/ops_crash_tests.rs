//! A coordinator that crashed in the middle of a change to its ops store, restarted on the files
//! the crash left: the ops file, the halts file and the staged halts file, put together from
//! the files before the change and the files after it. A crash before the ops file was replaced
//! restarts as before the change, one after it as after the change, the halts file of the
//! change included; and the first heartbeat after the restart is answered as that state says.

use std::fs;
use std::path::PathBuf;

use kbf_types::HostRole;
use kbf_updater::wire::{Heartbeat, UpdateResult};
use serde_json::json;

use crate::desired::KBF;
use crate::identity::Identity;
use crate::ops::{HaltAck, ServiceOps};
use crate::ops_file::{halts_path, staged_path};
use crate::service::FEATURES;
use crate::testutil::{OPERATOR, Rig, beat, id};

fn halted_update() -> UpdateResult {
    serde_json::from_value(json!({
        "target": "2.0", "outcome": "failed", "phase": "halt", "reason": "disk_tainted"
    }))
    .expect("a report")
}

/// The heartbeat of a host at 1.0 whose update to 2.0 halted, announcing `halt-v1`.
fn halting() -> Heartbeat {
    let mut b = calm();
    b.last_update = Some(halted_update());
    b
}

/// The heartbeat of a host at 1.0 with no update to report, announcing `halt-v1`.
fn calm() -> Heartbeat {
    let mut b = beat("1.0");
    b.agent_features = FEATURES.iter().map(|f| (*f).to_owned()).collect();
    b
}

fn storage(name: &str) -> Identity {
    Identity {
        node_id: id(name),
        role: HostRole::STORAGE,
    }
}

fn ops_path(rig: &Rig) -> PathBuf {
    rig.dir.path().join("desired.json.ops")
}

/// The ops file and the halts file (none where there is none) as they are now.
struct Files {
    ops: Option<Vec<u8>>,
    halts: Option<Vec<u8>>,
}

fn files(rig: &Rig) -> Files {
    let path = ops_path(rig);
    Files {
        ops: fs::read(&path).ok(),
        halts: fs::read(halts_path(&path)).ok(),
    }
}

/// Puts back what a crash in the change from `before` to `after` leaves (the ops file of
/// `after` when the crash came after it was replaced, of `before` if not; the halts file of
/// `before`; the halts file of `after`, staged) and restarts on it.
fn crash(rig: &mut Rig, before: &Files, after: &Files, committed: bool) {
    let path = ops_path(rig);
    let ops = if committed { &after.ops } else { &before.ops };
    match ops {
        Some(ops) => fs::write(&path, ops).expect("the ops file"),
        None => fs::remove_file(&path).expect("no ops file"),
    }
    match &before.halts {
        Some(halts) => fs::write(halts_path(&path), halts).expect("the halts file"),
        None => fs::remove_file(halts_path(&path)).expect("no halts file"),
    }
    let staged = after.halts.as_ref().expect("the change wrote a halts file");
    fs::write(staged_path(&path), staged).expect("the staged file");
    rig.restart();
    assert!(
        !staged_path(&path).exists(),
        "the start settles the staged file"
    );
    let halts = if committed {
        &after.halts
    } else {
        &before.halts
    };
    assert_eq!(
        &fs::read(halts_path(&path)).ok(),
        halts,
        "the halts file is the one of the side of the change the ops file is on"
    );
}

fn kbf(rig: &Rig) -> ServiceOps {
    rig.coordinator.ops().marks(KBF)
}

fn clear(rig: &Rig) {
    rig.coordinator
        .operate(
            Some(OPERATOR),
            br#"{"action":"clear_halt","service":"kbf","reason":"disk replaced"}"#,
        )
        .expect("cleared");
}

/// The generation an answer to `node` tells for kbf.
fn told(rig: &Rig, node: &str, beat: Heartbeat) -> Option<u64> {
    rig.coordinator
        .heartbeat(storage(node), beat)
        .halts
        .get(KBF)
        .copied()
}

/// A rig past the quiet start whose kbf `s1` halted.
fn halted_rig(name: &str) -> Rig {
    let rig = Rig::new(name);
    rig.at(1_100);
    rig.coordinator.heartbeat(storage("s1"), halting());
    assert!(kbf(&rig).halt.is_some());
    rig
}

/// Catches: a halt raised in memory and in the ops file lost at a restart, or its report: a
/// crash after the ops file held the halt restarts halted, the halt's report included, and
/// another host's halting report is kept for the clear.
#[test]
fn a_crash_after_a_halt_reached_the_ops_file_restarts_halted() {
    let mut rig = Rig::new("ops-crash-raise-after");
    rig.at(1_100);
    rig.coordinator.heartbeat(storage("s9"), calm());
    let before = files(&rig);
    rig.coordinator.heartbeat(storage("s1"), halting());
    let after = files(&rig);
    crash(&mut rig, &before, &after, true);
    let halt = kbf(&rig).halt.expect("halted");
    assert_eq!(
        (halt.node_id.as_str(), halt.report),
        ("s1", Some(halted_update()))
    );
    rig.coordinator.heartbeat(storage("s2"), halting());
    assert_eq!(kbf(&rig).halt_seen.len(), 1);
}

/// Pins: a crash before the ops file held a halt restarts unhalted, with nothing of it joined
/// from its staged halts file; the host's next report raises it again.
#[test]
fn a_crash_before_a_halt_reached_the_ops_file_restarts_unhalted_and_the_next_report_halts() {
    let mut rig = Rig::new("ops-crash-raise-before");
    rig.at(1_100);
    rig.coordinator.heartbeat(storage("s9"), calm());
    let before = files(&rig);
    rig.coordinator.heartbeat(storage("s1"), halting());
    let after = files(&rig);
    crash(&mut rig, &before, &after, false);
    assert_eq!(kbf(&rig).halt, None);
    rig.coordinator.heartbeat(storage("s1"), halting());
    assert!(kbf(&rig).halt.is_some());
}

/// Catches: a clear committed to the ops file restarted with the halts file before it: the
/// acknowledgement without its report and the old generation, so agents are not told to lift
/// and the halted host's next report is weighed against an acknowledgement with no report. The
/// restart reads the clear whole: its acknowledgement with the report, the new generation told
/// to an agent, and the halted host's next report does not halt the service again.
#[test]
fn a_crash_after_a_clear_reached_the_ops_file_restarts_cleared_with_its_generation() {
    let mut rig = halted_rig("ops-crash-clear-after");
    let before = files(&rig);
    clear(&rig);
    let after = files(&rig);
    let marks = kbf(&rig);
    crash(&mut rig, &before, &after, true);
    assert_eq!(kbf(&rig), marks);
    assert_eq!(marks.halt_generation, 1);
    assert_eq!(
        marks.halt_acked,
        [HaltAck {
            node_id: "s1".to_owned(),
            target: crate::testutil::version("2.0"),
            report: Some(halted_update()),
        }]
    );
    assert_eq!(told(&rig, "s1", halting()), Some(1));
    assert_eq!(kbf(&rig).halt, None, "the cleared report halted again");
}

/// Catches: a clear that never reached the ops file published by its staged halts file (the
/// generation told with the halt still standing, so agents lift a halt no operator cleared):
/// the restart is halted, the generation is the old one, and the next report keeps it halted.
#[test]
fn a_crash_before_a_clear_reached_the_ops_file_restarts_halted_with_the_old_generation() {
    let mut rig = halted_rig("ops-crash-clear-before");
    let before = files(&rig);
    let marks = kbf(&rig);
    clear(&rig);
    let after = files(&rig);
    crash(&mut rig, &before, &after, false);
    assert_eq!(kbf(&rig), marks);
    assert_eq!(told(&rig, "s1", halting()), None);
    assert!(kbf(&rig).halt.is_some());
}

/// Catches: the end of an acknowledgement (the cleared host reported something else) undone
/// or half kept by a crash: after the ops file held it, the restart has no acknowledgement and
/// the host's old report, sent again, halts the service again; before, the acknowledgement and
/// its report are back and the same report does not.
#[test]
fn a_crash_around_the_end_of_an_acknowledgement_restarts_on_one_side_of_it() {
    for committed in [true, false] {
        let mut rig = halted_rig(&format!("ops-crash-ack-{committed}"));
        clear(&rig);
        let before = files(&rig);
        let acked = kbf(&rig).halt_acked;
        rig.coordinator.heartbeat(storage("s1"), calm());
        assert_eq!(kbf(&rig).halt_acked, []);
        let after = files(&rig);
        crash(&mut rig, &before, &after, committed);
        let expected = if committed { Vec::new() } else { acked };
        assert_eq!(kbf(&rig).halt_acked, expected, "committed: {committed}");
        rig.coordinator.heartbeat(storage("s1"), halting());
        assert_eq!(
            kbf(&rig).halt.is_some(),
            committed,
            "committed: {committed}"
        );
    }
}
