//! What a `Start` becomes: a job of the job service, submitted inside the window the farm gave
//! it.

use std::time::Duration;

use js_proto::v1::JobState;
use kbf_front::Finished;
use kbf_job_client::fake::Fault;
use kbf_server::executor::{InputsError, PresignError};
use tonic::Code;

use crate::rig::{Rig, cancels, hex, settle, submits};

const HOST: &str = "node1";

fn code(finished: &Finished) -> Code {
    match finished {
        Finished::Failed(status) => Code::from_i32(status.code),
        other => panic!("not a failure: {other:?}"),
    }
}

/// Catches an action that does not run end to end: submitted as a job of the kbf image, read
/// back at its exit, ingested, reported, acknowledged and answered to the caller, with the
/// task given what it needs (the image without its scheme, the secrets as arguments, the pack
/// under `cas/b`, a url good for the job's wall time) and the job service asked, in order,
/// for one `SubmitJob`, reads of the job, and one `GetResult` when it had ended.
#[tokio::test(start_paused = true)]
async fn an_action_runs_end_to_end() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    assert_eq!(rig.exec.workers().len(), 1);
    assert_eq!(rig.exec.workers()[0].worker.to_string(), "js:front-1-0");
    let caller = rig.submit(7);
    rig.pump().await;
    let jobs = rig.jobs_of(7);
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let job = &jobs[0];
    assert_eq!(job.state(), JobState::Queued);
    assert_eq!(job.arg("--action"), Some(format!("{}/7", hex(7)).as_str()));
    // The kbf image keeps the task tool as its entrypoint: the arguments are its flags, with
    // no path in front, and the job's command is left empty.
    assert_eq!(job.spec.args[0], "--action");
    assert!(job.spec.command.is_empty());
    assert_eq!(
        job.spec.image,
        crate::rig::IMAGE.trim_start_matches("docker://")
    );
    assert_eq!(job.arg("--grant-token"), Some("read-token-0001"));
    assert_eq!(job.arg("--grant-url"), Some("https://grant.test:9400"));
    assert!(
        job.spec
            .args
            .contains(&"--insecure-argv-secrets".to_owned())
    );
    let url = job.arg("--pack-url").expect("a pack url");
    let lease = job.arg("--lease").expect("a lease");
    assert!(
        url.starts_with(&format!("https://store.test/cas/b/{lease}/")),
        "{url}"
    );
    assert!(!lease.contains('.'), "{lease}");
    assert!(
        job.id().ends_with(&format!("-{lease}-0")),
        "the job's id is made from the farm lease: {}",
        job.id()
    );
    let minted = rig.presign.minted.lock().expect("minted").clone();
    assert_eq!(minted.len(), 1);
    assert_eq!(minted[0].max_bytes, 128 << 20);
    assert_eq!(minted[0].expires, Duration::from_secs(60 + 300));
    assert_eq!(job.spec.wall_limit_seconds, Some(360));
    let id = job.id().to_owned();
    rig.host_finishes(&id, HOST, 0);
    let finished = rig.until_done(&caller, 4).await;
    let Finished::Ran(result) = finished else {
        panic!("not ran: {finished:?}");
    };
    assert_eq!(result.exit_code, 0);
    let calls = rig.ingest.calls.lock().expect("calls").clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, minted[0].key);
    assert_eq!(calls[0].2.max_pack_bytes, 128 << 20);
    let methods: Vec<_> = rig
        .methods()
        .into_iter()
        .filter(|method| *method != "ListJobs")
        .collect();
    assert_eq!(methods.first(), Some(&"SubmitJob"), "{methods:?}");
    assert_eq!(methods.last(), Some(&"GetResult"), "{methods:?}");
    let middle = &methods[1..methods.len() - 1];
    assert!(
        middle.iter().all(|method| *method == "GetJob"),
        "{methods:?}"
    );
    assert_eq!(rig.jobs().len(), 1, "the action made one job");
    assert!(
        rig.exec.workers()[0].runs == 0,
        "the acknowledged result is forgotten"
    );
    let counted = rig.counted();
    for row in [
        "kbf_executor_jobs_submitted_total 1\n",
        "kbf_executor_jobs_cancelled_total 0\n",
        "kbf_executor_runs_total{outcome=\"ran\"} 1\n",
        "kbf_executor_ingests_total{result=\"ok\"} 1\n",
        "kbf_executor_late_starts_total 0\n",
    ] {
        assert!(counted.contains(row), "{row} in {counted}");
    }
}

/// Catches a `Start` whose submit the job service does not take at first (it answers
/// UNAVAILABLE, and then takes the job but loses the answer) being failed or lost instead of
/// retried: the same job is sent again on the next passes, inside the window, and lands as one
/// job. The CAS is read and the url made once, however many passes it waited.
#[tokio::test(start_paused = true)]
async fn a_start_waits_for_room_inside_its_window() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    rig.fake.inject_on(
        "SubmitJob",
        Fault::Status {
            code: Code::Unavailable,
            message: "no route".to_owned(),
        },
    );
    rig.fake.inject_on("SubmitJob", Fault::LoseAnswer);
    let caller = rig.submit(3);
    for _ in 0..3 {
        rig.pump().await;
        rig.advance(Duration::from_secs(1)).await;
    }
    rig.pump().await;
    let jobs = rig.jobs_of(3);
    assert_eq!(jobs.len(), 1, "submitted once the service took it");
    let sent = submits(&rig.fake.calls());
    assert_eq!(sent.len(), 3, "two submits failed and the third landed");
    assert!(
        sent.iter().all(|spec| *spec == sent[0]),
        "each submit sent the same job"
    );
    assert_eq!(rig.counter("kbf_executor_jobs_submitted_total"), 1);
    assert_eq!(
        rig.counter("kbf_executor_jobs_resubmitted_after_lost_answer_total"),
        2
    );
    assert_eq!(rig.counter("kbf_executor_late_starts_total"), 0);
    // Read once, however many passes it waited: the CAS is not read, and no url or grant is
    // made, on each pass of the window.
    let reads = rig.inputs.looked_up.lock().expect("looked up").clone();
    assert_eq!(
        reads.iter().filter(|h| **h == hex(3)).count(),
        1,
        "{reads:?}"
    );
    assert_eq!(rig.presign.keys().len(), 1, "a url was made again");
    let id = jobs[0].id().to_owned();
    rig.host_finishes(&id, HOST, 0);
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a `Start` acted on after its window (the late-Start defect): the farm gives up the
/// lease when its heartbeats leave it out, and the action runs again on a new lease, so a job
/// left running would run beside its retry. The submit of the `Start` is answered by nothing
/// (the job exists), its window ends, and the job is cancelled before it is ever placed; the
/// action's only run is the retry's, after the farm requeues it.
#[tokio::test(start_paused = true)]
async fn a_late_start_never_runs_twice() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    rig.fake.inject_on("SubmitJob", Fault::LoseAnswer);
    let caller = rig.submit(1);
    rig.farm.tick().await;
    settle().await;
    rig.exec.step().await;
    let first = rig.jobs_of(1);
    assert_eq!(first.len(), 1, "the lost answer left a job: {first:?}");
    let late = first[0].id().to_owned();
    // The window of a Start is 14 s from the heartbeat it names.
    rig.advance(Duration::from_secs(15)).await;
    rig.exec.step().await;
    assert_eq!(cancels(&rig.fake.calls()), std::slice::from_ref(&late));
    assert_eq!(rig.jobs_of(1)[0].state(), JobState::Cancelled);
    assert_eq!(rig.counter("kbf_executor_late_starts_total"), 1);
    // The farm, whose heartbeats leave the lease out, gives it up after the grace and
    // places the action again, as a new lease.
    rig.elapse(70).await;
    let jobs = rig.jobs_of(1);
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    let retry = jobs
        .iter()
        .find(|job| job.id() != late)
        .expect("the retry's job")
        .id()
        .to_owned();
    rig.host_finishes(&retry, HOST, 0);
    assert!(matches!(rig.until_done(&caller, 6).await, Finished::Ran(_)));
    let states: Vec<_> = rig.jobs_of(1).iter().map(|job| job.state()).collect();
    assert_eq!(states, [JobState::Cancelled, JobState::Succeeded]);
    assert!(
        rig.jobs_of(1)[0].job.host_id.is_empty(),
        "the late job was placed on a host"
    );
    assert_eq!(rig.counter("kbf_executor_late_starts_total"), 1);
}

/// Catches a `Start` submitted past its window because the window was judged once, at the
/// start of the pass, and the read of the action then took longer than the window left (a slow
/// CAS): the farm has given the lease up and runs it again, so the late job would run beside
/// its retry. The clock is read again just before the submit, the `Start` is dropped, and the
/// action runs once, after the farm requeues it.
#[tokio::test(start_paused = true)]
async fn a_start_whose_inputs_were_slow_is_not_submitted_past_its_window() {
    use std::sync::atomic::Ordering;
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
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
        rig.jobs().is_empty(),
        "a Start was submitted 20 s after its 14 s window"
    );
    assert!(submits(&rig.fake.calls()).is_empty());
    rig.elapse(70).await;
    let jobs = rig.jobs_of(1);
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    rig.host_finishes(jobs[0].id(), HOST, 0);
    assert!(matches!(rig.until_done(&caller, 6).await, Finished::Ran(_)));
    assert_eq!(rig.jobs_of(1).len(), 1, "the action ran once");
    assert_eq!(rig.counter("kbf_executor_late_starts_total"), 1);
}

/// Catches a read of the CAS that never answers holding the pass, and with it the heartbeats
/// and the fence: the read is given the `Start`'s window and no more, after which the
/// `Start` is dropped, as one that arrived late is.
#[tokio::test(start_paused = true)]
async fn a_cas_that_never_answers_is_given_the_window_and_no_more() {
    use std::sync::atomic::Ordering;
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
    rig.farm.tick().await;
    settle().await;
    rig.inputs.hold.store(true, Ordering::SeqCst);
    let in_time = rig.step_through(Duration::from_secs(15)).await;
    rig.inputs.hold.store(false, Ordering::SeqCst);
    assert!(in_time, "the pass waited for the CAS past the window");
    assert!(rig.jobs().is_empty());
    rig.elapse(70).await;
    let jobs = rig.jobs_of(1);
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    rig.host_finishes(jobs[0].id(), HOST, 0);
    assert!(matches!(rig.until_done(&caller, 6).await, Finished::Ran(_)));
}

/// Catches a pass that starts every waiting task however long the first ones take: the
/// heartbeats and the fence come after them. Once the pass budget is spent no more is begun,
/// and the tasks left are taken in the next pass, still inside their windows.
#[tokio::test(start_paused = true)]
async fn a_pass_begins_no_task_once_its_budget_is_spent() {
    use std::sync::atomic::Ordering;
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let (_a, _b) = (rig.submit(1), rig.submit(2));
    rig.farm.tick().await;
    settle().await;
    rig.inputs.hold.store(true, Ordering::SeqCst);
    let (exec, fake, inputs) = (&mut rig.exec, &rig.fake, &rig.inputs);
    let slow = async {
        settle().await;
        tokio::time::advance(Duration::from_secs(11)).await;
        fake.advance(Duration::from_secs(11));
        inputs.hold.store(false, Ordering::SeqCst);
        inputs.release.notify_one();
    };
    tokio::join!(exec.step(), slow);
    assert_eq!(rig.jobs().len(), 1, "the second task began past the budget");
    rig.exec.step().await;
    assert_eq!(rig.jobs().len(), 2, "the second task was not taken up");
}

/// Catches an action the executor cannot run being passed to the job service or rerun: an
/// image that is not pinned by digest, or a timeout over the executor's limit, is the caller's
/// to fix and is answered INVALID_ARGUMENT; no job is submitted.
#[tokio::test(start_paused = true)]
async fn an_action_that_cannot_run_here_is_refused_to_its_caller() {
    for (tag, image, timeout) in [
        (1, "registry.test/app:latest", Some(60)),
        (2, crate::rig::IMAGE, Some(2 * 3600)),
    ] {
        let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
        rig.pump().await;
        {
            let mut facts = rig.inputs.facts.lock().expect("facts");
            let facts = facts.as_mut().expect("facts are ok");
            facts.image = image.to_owned();
            facts.timeout = timeout.map(Duration::from_secs);
        }
        let caller = rig.submit(tag);
        let finished = rig.until_done(&caller, 3).await;
        assert_eq!(code(&finished), Code::InvalidArgument, "{finished:?}");
        assert!(rig.jobs().is_empty());
        assert!(submits(&rig.fake.calls()).is_empty());
    }
}

/// Catches what the farm could not get (the CAS, the store) being the caller's error: it is
/// infrastructure, so the farm runs the action again, three times in all, and answers INTERNAL
/// only when its budget is spent. No job is submitted without a task to run.
#[tokio::test(start_paused = true)]
async fn what_the_farm_could_not_read_is_rerun_not_refused() {
    use std::sync::atomic::Ordering;
    for (what, fail) in [("cas", true), ("store", false)] {
        let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
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
        assert_eq!(code(&finished), Code::Internal, "{what}: {finished:?}");
        assert!(rig.jobs().is_empty(), "{what}: a job was submitted");
        // The CAS is read once for each try; the store's url is asked for after the key is
        // minted, so a try that failed at the store minted a key.
        let tries = match fail {
            true => rig.inputs.looked_up.lock().expect("looked up").len(),
            false => usize::try_from(rig.ingest.minted.load(Ordering::SeqCst)).expect("a count"),
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
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    *rig.inputs.grant.lock().expect("grant") = Err(InputsError::Invalid("no grant".to_owned()));
    let caller = rig.submit(5);
    let finished = rig.until_done(&caller, 3).await;
    assert_eq!(code(&finished), Code::InvalidArgument);
    assert_eq!(rig.inputs.looked_up.lock().expect("looked up").len(), 1);
    assert!(rig.jobs().is_empty());
}
