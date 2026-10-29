//! What a `Start` becomes: a run of the task tool, posted inside the window the farm gave it.

use std::time::Duration;

use kbf_front::Finished;
use kbf_lease_client::Outcome;
use kbf_server::executor::{InputsError, PresignError};
use tonic::Code;

use crate::rig::{Rig, count, count_tail, hex};

fn invalid(finished: &Finished) -> Code {
    match finished {
        Finished::Failed(status) => Code::from_i32(status.code),
        other => panic!("not a failure: {other:?}"),
    }
}

/// Catches an action that does not run end to end: placed on the leased host's worker, posted
/// as a run of the task tool, read back at its exit, ingested, reported, acknowledged and
/// answered to the caller, with the task given what it needs (the image without its scheme,
/// the tool, the secrets as arguments, the pack under `cas/b`, the host's own store proxy, a
/// url good for the run's wall time) and the lease server asked for one lease and one run.
#[tokio::test(start_paused = true)]
async fn an_action_runs_end_to_end() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    assert_eq!(rig.exec.hosts().len(), 1);
    assert_eq!(rig.exec.hosts()[0].worker.to_string(), "js:node1");
    let caller = rig.submit(7);
    rig.pump().await;
    let runs = rig.runs().await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = &runs[0];
    assert_eq!(run.arg("--action"), Some(format!("{}/7", hex(7)).as_str()));
    assert_eq!(run.args[0], "/mnt/kbf-tools/kbf-task");
    assert_eq!(run.arg("--grant-token"), Some("read-token-0001"));
    assert_eq!(run.arg("--grant-url"), Some("https://10.0.0.1:9400"));
    assert!(run.args.contains(&"--insecure-argv-secrets".to_owned()));
    let url = run.arg("--pack-url").expect("a pack url");
    assert!(url.starts_with("http://store.test/cas/b/front-1/"), "{url}");
    let minted = rig.presign.minted.lock().expect("minted").clone();
    assert_eq!(minted.len(), 1);
    assert_eq!(minted[0].host, "node1");
    assert_eq!(minted[0].expires, Duration::from_secs(60 + 300));
    assert_eq!(rig.fake.run_wall_ms(&run.run), Some(360_000));
    rig.host_finishes(&run.run, Outcome::Succeeded, Some(0));
    let finished = rig.until_done(&caller, 4).await;
    let Finished::Ran(result) = finished else {
        panic!("not ran: {finished:?}");
    };
    assert_eq!(result.exit_code, 0);
    let calls = rig.ingest.calls.lock().expect("calls").clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, minted[0].key);
    assert_eq!(calls[0].2.max_pack_bytes, 128 << 20);
    let requests = rig.requests();
    assert_eq!(count(&requests, "POST /v1/leases"), 1);
    assert_eq!(count_tail(&requests, "POST", "/runs"), 1);
    assert!(rig.exec.hosts()[0].runs == 0, "the acknowledged result is forgotten");
}

/// Catches a `Start` that the lease server cannot take yet (its budget comes back only when a
/// finished run has ended, two or three heartbeats late) being failed or lost instead of
/// retried: it is posted as soon as room is made, inside its window.
#[tokio::test(start_paused = true)]
async fn a_start_waits_for_room_inside_its_window() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let (a, _b) = (rig.submit(1), rig.submit(2));
    rig.pump().await;
    let first = rig.runs_of(1).await;
    rig.host_finishes(&first[0].run, Outcome::Succeeded, Some(0));
    rig.pump().await;
    assert!(a.finished().is_some(), "the farm has the result: its core is free");
    let c = rig.submit(3);
    rig.advance(Duration::from_secs(1)).await;
    rig.pump().await;
    assert!(rig.runs_of(3).await.is_empty(), "the host's budget is still booked");
    rig.host_ends(&first[0].run);
    rig.pump().await;
    let third = rig.runs_of(3).await;
    assert_eq!(third.len(), 1, "posted once room was made");
    rig.host_finishes(&third[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&c, 4).await, Finished::Ran(_)));
}

/// Catches a `Start` posted after its window (the late-Start defect): the farm gave up the
/// lease when its heartbeats left it out, and the action runs again on a new lease, so a run
/// posted late would run beside its retry. The late `Start` is dropped without a post and
/// without a result, and the action runs exactly once, after the farm requeues it.
#[tokio::test(start_paused = true)]
async fn a_late_start_never_runs_twice() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let (a, _b) = (rig.submit(1), rig.submit(2));
    rig.pump().await;
    let first = rig.runs_of(1).await;
    rig.host_finishes(&first[0].run, Outcome::Succeeded, Some(0));
    rig.pump().await;
    assert!(a.finished().is_some());
    let c = rig.submit(3);
    rig.pump().await;
    // The window of a Start is 14 s from the heartbeat it names; the host frees its budget
    // after it.
    rig.advance(Duration::from_secs(15)).await;
    rig.pump().await;
    rig.host_ends(&first[0].run);
    rig.pump().await;
    assert!(
        rig.runs_of(3).await.is_empty(),
        "a Start outside its window was posted"
    );
    // The farm, whose heartbeats leave the lease out, gives it up after the grace and
    // places the action again, as a new lease.
    rig.elapse(70).await;
    let runs = rig.runs_of(3).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&c, 6).await, Finished::Ran(_)));
    assert_eq!(rig.runs_of(3).await.len(), 1, "the action ran once");
}

/// Catches an action the executor cannot run being passed to the host or rerun: an image that
/// is not pinned by digest, or a timeout over the executor's limit, is the caller's to fix and
/// is answered INVALID_ARGUMENT; no run is posted.
#[tokio::test(start_paused = true)]
async fn an_action_that_cannot_run_here_is_refused_to_its_caller() {
    for (tag, image, timeout) in [
        (1, "registry.test/app:latest", Some(60)),
        (2, crate::rig::IMAGE, Some(2 * 3600)),
    ] {
        let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
        rig.pump().await;
        {
            let mut facts = rig.inputs.facts.lock().expect("facts");
            let facts = facts.as_mut().expect("facts are ok");
            facts.image = image.to_owned();
            facts.timeout = timeout.map(Duration::from_secs);
        }
        let caller = rig.submit(tag);
        let finished = rig.until_done(&caller, 3).await;
        assert_eq!(invalid(&finished), Code::InvalidArgument, "{finished:?}");
        assert!(rig.runs().await.is_empty());
    }
}

/// Catches what the farm could not get (the CAS, the store) being the caller's error: it is
/// infrastructure, so the farm runs the action again, three times in all, and answers INTERNAL
/// only when its budget is spent. No run is posted without a task to post.
#[tokio::test(start_paused = true)]
async fn what_the_farm_could_not_read_is_rerun_not_refused() {
    for (what, fail) in [("cas", true), ("store", false)] {
        let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
        rig.pump().await;
        match fail {
            true => {
                *rig.inputs.facts.lock().expect("facts") =
                    Err(InputsError::Unavailable("cas down".to_owned()));
            }
            false => {
                *rig.presign.fail.lock().expect("fail") =
                    Some(PresignError::Refused("over the ceiling".to_owned()));
            }
        }
        let caller = rig.submit(5);
        let finished = rig.until_done(&caller, 3).await;
        assert_eq!(invalid(&finished), Code::Internal, "{what}: {finished:?}");
        assert!(rig.runs().await.is_empty(), "{what}: a run was posted");
        let tries = match fail {
            true => rig.inputs.looked_up.lock().expect("looked up").len(),
            false => rig.presign.minted.lock().expect("minted").len() + 3,
        };
        assert_eq!(tries, 3, "{what}: the action was tried once and rerun twice");
    }
}

/// Catches a read grant the service refuses for the action (it cannot be read by this
/// farm at all) being rerun: it is answered INVALID_ARGUMENT at once.
#[tokio::test(start_paused = true)]
async fn a_grant_that_cannot_be_given_is_the_callers_to_fix() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    *rig.inputs.grant.lock().expect("grant") = Err(InputsError::Invalid("no grant".to_owned()));
    let caller = rig.submit(5);
    let finished = rig.until_done(&caller, 3).await;
    assert_eq!(invalid(&finished), Code::InvalidArgument);
    assert_eq!(rig.inputs.looked_up.lock().expect("looked up").len(), 1);
}
