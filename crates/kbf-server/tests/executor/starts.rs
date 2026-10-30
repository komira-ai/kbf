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
/// the secrets as arguments, the pack under `cas/b`, the host's own store proxy, a
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
    // The kbf image keeps the task tool as its entrypoint: the arguments are its flags, with
    // no path in front, and the run is launched from that image with nothing mounted.
    assert_eq!(run.args[0], "--action");
    assert_eq!(
        rig.fake.run_image(&run.run).as_deref(),
        Some(crate::rig::IMAGE.trim_start_matches("docker://"))
    );
    assert_eq!(rig.fake.run_mounts(&run.run), Some(Vec::new()));
    assert_eq!(run.arg("--grant-token"), Some("read-token-0001"));
    assert_eq!(run.arg("--grant-url"), Some("https://grant.test:9400"));
    assert!(run.args.contains(&"--insecure-argv-secrets".to_owned()));
    let url = run.arg("--pack-url").expect("a pack url");
    let lease = run.arg("--lease").expect("a lease");
    assert!(
        url.starts_with(&format!("https://store.test/cas/b/{lease}/")),
        "{url}"
    );
    assert!(!lease.contains('.'), "{lease}");
    let minted = rig.presign.minted.lock().expect("minted").clone();
    assert_eq!(minted.len(), 1);
    assert_eq!(minted[0].host, "node1");
    assert_eq!(minted[0].max_bytes, 128 << 20);
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
    assert!(
        rig.exec.hosts()[0].runs == 0,
        "the acknowledged result is forgotten"
    );
    let counted = rig.counted();
    for row in [
        "kbf_executor_leases_held 1\n",
        "kbf_executor_runs_total{outcome=\"ran\"} 1\n",
        "kbf_executor_ingests_total{result=\"ok\"} 1\n",
        "kbf_executor_late_starts_total 0\n",
    ] {
        assert!(counted.contains(row), "{row} in {counted}");
    }
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
    assert!(
        a.finished().is_some(),
        "the farm has the result: its core is free"
    );
    let c = rig.submit(3);
    rig.advance(Duration::from_secs(1)).await;
    rig.pump().await;
    assert!(
        rig.runs_of(3).await.is_empty(),
        "the host's budget is still booked"
    );
    rig.host_ends(&first[0].run);
    rig.pump().await;
    let third = rig.runs_of(3).await;
    assert_eq!(third.len(), 1, "posted once room was made");
    // Read once, however many passes it waited: the CAS is not read, and no url or grant is
    // made, on each pass of the window.
    let reads = rig.inputs.looked_up.lock().expect("looked up").clone();
    assert_eq!(
        reads.iter().filter(|h| **h == hex(3)).count(),
        1,
        "{reads:?}"
    );
    let waited = rig.presign.keys().len();
    assert_eq!(waited, 3, "a url was minted again while the Start waited");
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
    let counted = rig.counted();
    assert!(
        counted.contains("kbf_executor_late_starts_total 1\n"),
        "the dropped Start is counted once: {counted}"
    );
}

/// Catches a `Start` posted past its window because the window was judged once, at the start
/// of the pass, and the read of the action then took longer than the window left (a slow
/// CAS): the farm has given the lease up and runs it again, so the late post would run
/// beside its retry. The clock is read again just before the post, the `Start` is dropped, and
/// the action runs once, after the farm requeues it.
#[tokio::test(start_paused = true)]
async fn a_start_whose_inputs_were_slow_is_not_posted_past_its_window() {
    use crate::rig::settle;
    use std::sync::atomic::Ordering;
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
    rig.farm.tick().await;
    settle().await;
    rig.inputs.hold.store(true, Ordering::SeqCst);
    let (exec, fake, inputs) = (&mut rig.exec, &rig.fake, &rig.inputs);
    let slow = async {
        settle().await;
        tokio::time::advance(Duration::from_secs(20)).await;
        fake.advance(Duration::from_secs(20));
        inputs.release.notify_one();
    };
    tokio::join!(exec.step(), slow);
    rig.inputs.hold.store(false, Ordering::SeqCst);
    assert!(
        rig.runs().await.is_empty(),
        "a Start was posted 20 s after its 14 s window"
    );
    assert_eq!(count_tail(&rig.requests(), "POST", "/runs"), 0);
    rig.elapse(70).await;
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 6).await, Finished::Ran(_)));
    assert_eq!(rig.runs_of(1).await.len(), 1, "the action ran once");
    assert!(rig.counted().contains("kbf_executor_late_starts_total 1\n"));
}

/// Catches a lease found gone only when the run is to be posted: the renewal that comes before
/// a post (the lease would end before the run does) is refused, the worker is taken down with
/// its lease, and the `Start` is given back to the farm, which runs the action again on
/// another lease. Nothing is posted to a lease that is not there.
#[tokio::test(start_paused = true)]
async fn a_lease_gone_before_the_post_is_given_up_and_the_action_rerun() {
    use std::sync::atomic::Ordering;
    let mut rig = Rig::start(&["node1", "node2"], 1, crate::rig::long_runs).await;
    rig.pump().await;
    rig.set_timeout(Duration::from_secs(3_500));
    let caller = rig.submit(1);
    rig.farm.tick().await;
    crate::rig::settle().await;
    rig.inputs.hold.store(true, Ordering::SeqCst);
    let lease = rig.exec.hosts()[0].lease;
    let (exec, api, inputs) = (&mut rig.exec, &rig.api, &rig.inputs);
    let gone = async {
        crate::rig::settle().await;
        api.release(lease).await.expect("released");
        inputs.hold.store(false, Ordering::SeqCst);
        inputs.release.notify_one();
    };
    tokio::join!(exec.step(), gone);
    assert!(rig.exec.hosts().is_empty(), "the worker kept a gone lease");
    assert!(rig.runs().await.is_empty());
    let posts = count_tail(&rig.requests(), "POST", "/runs");
    assert_eq!(posts, 0, "a run was posted to a lease that is gone");
    rig.elapse(15).await;
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 6).await, Finished::Ran(_)));
}

/// Catches a `Start` posted after its window because the renewal that comes first took the
/// time: the lease server answers a renewal slowly (here, not at all until the call gives up),
/// the window ends meanwhile, and the post is not made.
#[tokio::test(start_paused = true)]
async fn a_start_whose_renewal_outlasted_its_window_is_not_posted() {
    use kbf_lease_client::fake::Fault;
    let mut rig = Rig::start(&["node1"], 1, crate::rig::long_runs).await;
    rig.pump().await;
    rig.set_timeout(Duration::from_secs(3_500));
    let caller = rig.submit(1);
    rig.farm.tick().await;
    crate::rig::settle().await;
    rig.advance(Duration::from_secs(10)).await;
    rig.fake.inject_on("POST /v1/leases", Fault::Hang);
    let in_time = rig.step_through(Duration::from_secs(6)).await;
    assert!(in_time);
    assert!(rig.runs().await.is_empty(), "posted after its window");
    rig.elapse(70).await;
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 6).await, Finished::Ran(_)));
    assert!(rig.counted().contains("kbf_executor_late_starts_total 1\n"));
}

/// Catches a read of the CAS that never answers holding the pass, and with it the heartbeats
/// and the fence: the read is given the `Start`'s window and no more, after which the
/// `Start` is dropped, as one that arrived late is.
#[tokio::test(start_paused = true)]
async fn a_cas_that_never_answers_is_given_the_window_and_no_more() {
    use std::sync::atomic::Ordering;
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
    rig.farm.tick().await;
    crate::rig::settle().await;
    rig.inputs.hold.store(true, Ordering::SeqCst);
    let in_time = rig.step_through(Duration::from_secs(15)).await;
    rig.inputs.hold.store(false, Ordering::SeqCst);
    assert!(in_time, "the pass waited for the CAS past the window");
    assert!(rig.runs().await.is_empty());
    rig.elapse(70).await;
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 6).await, Finished::Ran(_)));
}

/// Catches a pass that starts every waiting task however long the first ones take: the
/// heartbeats and the fence come after them. Once the pass budget is spent no more is begun,
/// and the tasks left are taken in the next pass, still inside their windows.
#[tokio::test(start_paused = true)]
async fn a_pass_begins_no_task_once_its_budget_is_spent() {
    use std::sync::atomic::Ordering;
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let (_a, _b) = (rig.submit(1), rig.submit(2));
    rig.farm.tick().await;
    crate::rig::settle().await;
    rig.inputs.hold.store(true, Ordering::SeqCst);
    let (exec, fake, inputs) = (&mut rig.exec, &rig.fake, &rig.inputs);
    let slow = async {
        crate::rig::settle().await;
        tokio::time::advance(Duration::from_secs(11)).await;
        fake.advance(Duration::from_secs(11));
        inputs.hold.store(false, Ordering::SeqCst);
        inputs.release.notify_one();
    };
    tokio::join!(exec.step(), slow);
    assert_eq!(
        rig.runs().await.len(),
        1,
        "the second task began past the budget"
    );
    rig.exec.step().await;
    assert_eq!(
        rig.runs().await.len(),
        2,
        "the second task was not taken up"
    );
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
        assert_eq!(
            tries, 3,
            "{what}: the action was tried once and rerun twice"
        );
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
