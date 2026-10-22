//! Step 9 of the swap, the order for the nodes, is retired: no call is made for the nodes and
//! no `nodes` phase is entered. What is left is the reading of an `update.json` that an older
//! build wrote, which may be at that phase or may have failed at that step. Against the fakes of
//! [`super::fakes`].

use super::fakes::*;
use super::model::*;
use super::record::FileRecorder;
use super::recover_tests::{crash_when, draining_world, failed_at, restart, run_to_crash};
use crate::testkit::{A, B};

/// An update stopped after the confirm landed (the drain was about to start), and its record
/// rewritten the way a build that told the nodes would have left it: at the `nodes` phase, with
/// `failure` as that build recorded it.
fn left_at_nodes_by_an_older_build(failure: Option<Failure>) -> World {
    let (w, old) = draining_world();
    crash_when(&w, |i| i.phase == Phase::Draining);
    run_to_crash(&w, Mode::Frontend, old);
    {
        let mut records = w.records.borrow_mut();
        let last = records.last_mut().expect("a record was written");
        last.phase = Phase::Nodes;
        last.failure = failure;
    }
    w
}

/// The phases the records written after the first `written` ones are at.
fn phases_after(w: &World, written: usize) -> Vec<Phase> {
    w.records.borrow()[written..]
        .iter()
        .map(|record| record.phase)
        .collect()
}

/// Catches a record at the `nodes` phase, which a build that told the nodes left when it died
/// there, being refused, being run again from an earlier phase, or being waited on: the
/// confirm has landed, so the restart goes straight to the drain, enters no phase for the nodes
/// and ends the update `done` once the old child is idle and retired.
///
/// Mutants it catches: the `nodes` phase entered again; a restart that does nothing for a record
/// at `nodes`; the drain waiting for the nodes to be told.
#[test]
fn a_record_at_the_retired_nodes_phase_goes_on_to_the_drain() {
    let w = left_at_nodes_by_an_older_build(None);
    let written = w.records.borrow().len();
    assert_eq!(restart(&w).0, Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(phases_after(&w, written), [Phase::Draining, Phase::Done]);
    assert!(!w.last_intent().nodes_posted);
    assert_eq!(w.last_intent().failure, None);
    assert!(w.when("terminate pid 100").expect("old retired") >= secs(600));
    assert_eq!(w.count("order revert"), 0);
    assert_eq!(w.holders(), vec![Slot::B]);
}

/// Catches the failure an older build recorded at step `nodes` (the child refused the order for
/// good, or never answered) being lost at a restart: the new proxy does not send anything, goes
/// on to drain and retire, and the update ends `failed` at `nodes` as it would have. The frontend
/// was swapped and stays so: the staged release is kept and the new child serves.
///
/// Mutants it catches: the end of the update ignoring a recorded failure at `nodes` (it ends
/// `done`); the failure dropped at the restart.
#[test]
fn a_failure_an_older_build_recorded_at_nodes_still_ends_the_update_failed() {
    let failure = Failure {
        step: Step::Nodes,
        reason: "not_own: refused for good".to_owned(),
    };
    let w = left_at_nodes_by_an_older_build(Some(failure));
    let written = w.records.borrow().len();
    assert_eq!(restart(&w).0, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failed_at(&w), Some(Step::Nodes));
    assert_eq!(phases_after(&w, written), [Phase::Draining, Phase::Failed]);
    assert_eq!(w.count("order revert"), 0, "the staged release is kept");
    assert_eq!(w.count("kill"), 0);
    assert!(w.when("terminate pid 100").is_some(), "old child retired");
    assert_eq!(w.holders(), vec![Slot::B]);
}

/// Catches an `update.json` of a build that told the nodes no longer parsing: the phase word
/// `nodes`, the step word `nodes` and the flag `nodes_posted` are all still read, so a proxy
/// that comes up on such a file resumes the update instead of refusing every start.
///
/// Mutants it catches: the `nodes` variant of the phase or of the step removed.
#[test]
fn an_update_json_of_a_build_that_told_the_nodes_is_still_read() {
    let dir = kbf_host::testkit::scratch("proxy-nodes-record");
    let text = serde_json::json!({
        "update_id": "u1",
        "request_id": "r1",
        "mode": "frontend",
        "from": A,
        "to": B,
        "nodes": true,
        "rollback": false,
        "phase": "nodes",
        "phase_started": 1_700_000_000_u64,
        "bake_started": 1_700_000_000_u64,
        "nodes_posted": true,
        "old_slot": "a",
        "new_slot": "b",
        "old_proc": null,
        "new_proc": null,
        "respawned": false,
        "retire": null,
        "rolling_back": false,
        "failure": {"step": "nodes", "reason": "timeout: no answer"},
    });
    std::fs::write(dir.join("update.json"), text.to_string()).expect("seed");
    let intent = FileRecorder::load(&dir).expect("read").expect("present");
    assert_eq!(intent.phase, Phase::Nodes);
    assert!(intent.nodes_posted);
    assert_eq!(intent.failure.map(|f| f.step), Some(Step::Nodes));
}
