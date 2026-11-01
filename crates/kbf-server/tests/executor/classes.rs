//! How each ending of a run reaches the farm: which are the farm's fault and run again, which
//! are the caller's, and what the caller is answered.

use std::time::Duration;

use kbf_front::Finished;
use kbf_lease_client::Outcome;
use kbf_server::executor::{Ingested, RejectReason};
use tonic::Code;

use crate::rig::{FakeIngest, Rig};

/// Posts the action of `tag` and returns its caller and the run.
async fn posted(rig: &mut Rig, tag: u8) -> (crate::rig::Caller, kbf_lease_client::RunId) {
    rig.pump().await;
    let caller = rig.submit(tag);
    rig.pump().await;
    let runs = rig.runs_of(tag).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    (caller, runs[0].run.clone())
}

fn exit_code(finished: &Finished) -> i32 {
    match finished {
        Finished::Ran(result) => result.exit_code,
        other => panic!("did not run: {other:?}"),
    }
}

/// Catches a run killed at its memory limit being answered as the action's failure: it is the
/// farm's, so the action runs again with more memory (the farm records the kill), and the
/// caller sees only the second run's result.
#[tokio::test(start_paused = true)]
async fn an_out_of_memory_run_is_rerun_with_more_memory() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    let (caller, run) = posted(&mut rig, 1).await;
    rig.host_finishes(&run, Outcome::OutOfMemory, None);
    rig.pump().await;
    assert!(caller.finished().is_none(), "answered at the first kill");
    assert_eq!(
        rig.farm.memory_runs().len(),
        1,
        "the kill is on the farm's record"
    );
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 2, "no rerun: {runs:?}");
    rig.host_finishes(&runs[1].run, Outcome::Succeeded, Some(0));
    assert_eq!(exit_code(&rig.until_done(&caller, 4).await), 0);
    let counted = rig.counted();
    for row in [
        "kbf_executor_runs_total{outcome=\"out_of_memory\"} 1\n",
        "kbf_executor_runs_total{outcome=\"ran\"} 1\n",
    ] {
        assert!(counted.contains(row), "{row} in {counted}");
    }
}

/// Catches a run that passed its wall time being rerun or called the farm's fault: the
/// action's timeout is the caller's, answered DEADLINE_EXCEEDED, and the action is not run
/// again.
#[tokio::test(start_paused = true)]
async fn a_timeout_is_answered_to_the_caller_and_not_rerun() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    let (caller, run) = posted(&mut rig, 1).await;
    rig.host_finishes(&run, Outcome::TimedOut, None);
    let finished = rig.until_done(&caller, 4).await;
    match finished {
        Finished::TimedOut { status, .. } | Finished::Failed(status) => {
            assert_eq!(Code::from_i32(status.code), Code::DeadlineExceeded);
        }
        other => panic!("not a timeout: {other:?}"),
    }
    assert_eq!(rig.runs_of(1).await.len(), 1);
}

/// Catches a timeout at a wall time the lease cut short being called the caller's: the lease
/// server grants at most its maximum ttl, and a run is cut to what the lease has left when it
/// is posted, so an action whose wall time is longer than that is ended by the lease, not by its
/// own timeout. That is the farm's fault: the action is rerun, and the caller is not told it
/// timed out. (The run is cut to 3600 s of the 3800 s asked; a run not cut is the caller's
/// timeout, above.)
#[tokio::test(start_paused = true)]
async fn a_timeout_at_a_wall_the_lease_cut_short_is_rerun() {
    let mut rig = Rig::start(&["node1"], 1, crate::rig::long_runs).await;
    rig.pump().await;
    rig.set_timeout(Duration::from_secs(3_500));
    let caller = rig.submit(1);
    rig.pump().await;
    let first = rig.runs_of(1).await.remove(0);
    let cut = rig.fake.run_wall_ms(&first.run).expect("a wall");
    assert!(cut < 3_800_000, "the lease did not cut the run: {cut} ms");
    rig.host_finishes(&first.run, Outcome::TimedOut, None);
    rig.pump().await;
    assert!(
        caller.finished().is_none(),
        "answered as the caller's timeout"
    );
    assert_eq!(rig.runs_of(1).await.len(), 2, "not run again");
}

/// Catches an ending that says the infrastructure failed (the task tool exited nonzero, the
/// run was killed unasked, the container did not start, an outcome this server does not know,
/// a success with a nonzero exit) being answered as the action's failure. Each is rerun on a
/// new run, and the second run's OK is the caller's answer: the failed attempts leave no
/// result of the action behind, so one digest never shows a pass and a failure (a flake)
/// for an infrastructure fault.
#[tokio::test(start_paused = true)]
async fn an_infrastructure_ending_is_rerun_and_leaves_no_result_behind() {
    for (outcome, code) in [
        (Outcome::Failed, Some(12)),
        (Outcome::Killed, None),
        (Outcome::FailedToStart, None),
        (Outcome::Unknown, None),
        (Outcome::Succeeded, Some(3)),
    ] {
        let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
        let (caller, run) = posted(&mut rig, 1).await;
        rig.host_finishes(&run, outcome, code);
        rig.pump().await;
        assert!(
            caller.finished().is_none(),
            "{outcome:?} answered the caller"
        );
        let runs = rig.runs_of(1).await;
        assert_eq!(runs.len(), 2, "{outcome:?} was not rerun: {runs:?}");
        rig.host_finishes(&runs[1].run, Outcome::Succeeded, Some(0));
        let finished = rig.until_done(&caller, 4).await;
        assert_eq!(exit_code(&finished), 0, "{outcome:?}");
        assert!(rig.farm.memory_runs().is_empty(), "{outcome:?}");
        assert_eq!(
            rig.ingest.calls.lock().expect("calls").len(),
            1,
            "{outcome:?}"
        );
    }
}

/// Catches an action that ran and failed (the task tool exited 0, the pack says the action
/// exited 3) being rerun or called an infrastructure failure: the farm answers the caller with
/// the action's own exit code, from the pack the frontend verified.
#[tokio::test(start_paused = true)]
async fn an_action_that_failed_is_answered_with_its_own_exit_code() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.ingest
        .script
        .lock()
        .expect("script")
        .push_back(FakeIngest::ran(3));
    let (caller, run) = posted(&mut rig, 1).await;
    rig.host_finishes(&run, Outcome::Succeeded, Some(0));
    assert_eq!(exit_code(&rig.until_done(&caller, 4).await), 3);
    assert_eq!(rig.runs_of(1).await.len(), 1);
}

/// Catches a pack the frontend refuses, or cannot read, being the action's failure or being
/// asked for again under the same key (the store is create-only, so a second write to it
/// would be refused): the action runs again as a new run with a key of its own.
#[tokio::test(start_paused = true)]
async fn a_refused_pack_is_rerun_under_a_new_key() {
    for refusal in [
        Ingested::Reject(RejectReason::HashMismatch),
        Ingested::Unavailable("the store is down".to_owned()),
    ] {
        let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
        rig.ingest.script.lock().expect("script").push_back(refusal);
        let (caller, run) = posted(&mut rig, 1).await;
        rig.host_finishes(&run, Outcome::Succeeded, Some(0));
        rig.pump().await;
        assert!(caller.finished().is_none());
        let runs = rig.runs_of(1).await;
        assert_eq!(runs.len(), 2, "{runs:?}");
        rig.host_finishes(&runs[1].run, Outcome::Succeeded, Some(0));
        assert_eq!(exit_code(&rig.until_done(&caller, 4).await), 0);
        let keys: Vec<String> = rig
            .ingest
            .calls
            .lock()
            .expect("calls")
            .iter()
            .map(|call| call.1.clone())
            .collect();
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1], "the second read was of the first key");
    }
}

/// Catches an ingest that panics taking the executor down, or leaving the action hanging
/// (its result never formed): the run is an infrastructure failure, and the action runs again.
#[tokio::test(start_paused = true)]
async fn an_ingest_that_stops_is_an_infrastructure_failure() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.ingest
        .panics
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let (caller, run) = posted(&mut rig, 1).await;
    rig.host_finishes(&run, Outcome::Succeeded, Some(0));
    rig.pump().await;
    assert!(caller.finished().is_none());
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 2, "{runs:?}");
    rig.host_finishes(&runs[1].run, Outcome::Succeeded, Some(0));
    assert_eq!(exit_code(&rig.until_done(&caller, 4).await), 0);
}
