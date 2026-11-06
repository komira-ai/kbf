//! What the executor does when the world breaks: the job service stops answering, the farm ends
//! a stream or gives a lease up, an answer is lost, a worker falls silent.

use std::time::Duration;

use js_proto::v1::JobState;
use kbf_front::Finished;
use kbf_job_client::fake::Fault;
use kbf_sched::fence::{LEADER_LEASE_MARGIN, LEASE_GRACE};
use kbf_server::executor::{Config, ConfigError};
use tonic::{Code, Status};

use crate::rig::{Caller, GIB, Posted, Rig, cancels, submits};

const HOST: &str = "node1";

/// A rig with one action running: its caller and its job as it was submitted.
async fn running(mut rig: Rig) -> (Rig, Caller, Posted) {
    rig.pump().await;
    let caller = rig.submit(1);
    rig.pump().await;
    let job = rig.jobs_of(1).remove(0);
    rig.host_runs(job.id(), HOST);
    rig.pump().await;
    (rig, caller, job)
}

/// Runs the rig until the job service holds `n` jobs of the action of `tag`, or panics after
/// `steps` of 5 s.
pub async fn until_jobs(rig: &mut Rig, tag: u8, n: usize, steps: usize) -> Vec<Posted> {
    for _ in 0..steps {
        let jobs = rig.jobs_of(tag);
        if jobs.len() >= n {
            return jobs;
        }
        rig.advance(Duration::from_secs(5)).await;
        rig.pump().await;
    }
    panic!(
        "{n} jobs of action {tag} not made: {:?}\nworkers {:?}\ncalls {:?}",
        rig.jobs_of(tag),
        rig.exec.workers(),
        rig.methods()
    );
}

/// The job service says `UNAVAILABLE` to the next `n` calls of `method` (any call, if empty).
fn unavailable(rig: &Rig, method: &str, n: usize) {
    for _ in 0..n {
        rig.fake.inject_on(
            method,
            Fault::Status {
                code: Code::Unavailable,
                message: "down".to_owned(),
            },
        );
    }
}

/// Catches a worker that keeps heartbeating a farm while it cannot reach the job service, so
/// its jobs may be lost for good with nobody asking: after the fence time (T, under the
/// scheduler's grace G) without an answer the worker cancels its job (the call is made, even
/// if it is not answered) and goes silent, so the farm runs its leases again after G; the job
/// that could not be cancelled goes on.
#[tokio::test(start_paused = true)]
async fn an_unreachable_job_service_fences_the_worker_at_the_fence_time() {
    let (mut rig, caller, first) = running(Rig::start(&[HOST], 1, |_| {}).await).await;
    unavailable(&rig, "", 200);
    let mut seconds = 0;
    while !rig.exec.workers()[0].silent {
        assert!(seconds <= 60, "not fenced after {seconds} s");
        rig.advance(Duration::from_secs(5)).await;
        seconds += 5;
        rig.pump().await;
    }
    assert!(
        seconds + 5 > rig.cfg.fence_after.as_secs(),
        "fenced after {seconds} s: before T"
    );
    assert!(
        seconds < LEASE_GRACE.as_secs(),
        "fenced after {seconds} s: not before G"
    );
    assert!(caller.finished().is_none());
    assert_eq!(cancels(&rig.fake.calls()), [first.id()]);
    let held = rig.jobs();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].state(), JobState::Running, "the cancel was applied");
}

/// Catches a fence that cancels some of a worker's jobs, or none, or goes on talking: with
/// two jobs running and the job service silent past the fence time, the worker asks to
/// cancel each of them once, makes no call while it is silent, and the farm, which heard
/// nothing, runs both actions again as new farm leases, which are new jobs, once the service
/// answers again and the silence is over; the old jobs are cancelled for good.
#[tokio::test(start_paused = true)]
async fn the_fence_cancels_every_job_and_falls_silent() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let callers = [rig.submit(1), rig.submit(2)];
    rig.pump().await;
    let mut old: Vec<String> = rig.jobs().iter().map(|p| p.id().to_owned()).collect();
    assert_eq!(old.len(), 2, "{:?}", rig.jobs());
    old.sort();
    for id in &old {
        rig.host_runs(id, HOST);
    }
    rig.pump().await;
    assert_eq!(rig.exec.workers()[0].runs, 2);

    // Every read of a job fails from here on; the cancels the fence sends are answered.
    unavailable(&rig, "GetJob", 200);
    let mut seconds = 0;
    while !rig.exec.workers()[0].silent {
        assert!(seconds <= 60, "not fenced after {seconds} s");
        rig.advance(Duration::from_secs(5)).await;
        seconds += 5;
        rig.pump().await;
    }
    assert_eq!(
        rig.exec.workers()[0].runs,
        0,
        "the fenced worker kept tasks"
    );
    let mut cancelled = cancels(&rig.fake.calls());
    cancelled.sort();
    assert_eq!(cancelled, old, "each job is cancelled once");
    assert!(
        rig.jobs().iter().all(|p| p.job.cancel_requested),
        "{:?}",
        rig.jobs()
    );
    assert!(callers.iter().all(|c| c.finished().is_none()));

    // Silent: nothing is asked of the job service, and nothing is submitted, while the
    // worker waits for the farm to give its leases up.
    let calls = rig.fake.calls().len();
    rig.elapse(35).await;
    assert!(rig.exec.workers()[0].silent);
    assert_eq!(rig.fake.calls().len(), calls, "{:?}", rig.methods());
    assert_eq!(rig.jobs().len(), 2);

    // The service answers again: the rest of the failures are used up.
    let mut answered = false;
    for _ in 0..256 {
        if rig.api.get_job(&old[0]).await.is_ok() {
            answered = true;
            break;
        }
    }
    assert!(answered, "the injected failures were never used up");
    for tag in [1, 2] {
        let jobs = until_jobs(&mut rig, tag, 2, 12).await;
        assert!(!old.contains(&jobs[1].id().to_owned()), "{jobs:?}");
    }
    for id in &old {
        assert!(rig.fake.host_stopped(id), "{id} was not cancelled");
    }
    let mut cancelled = cancels(&rig.fake.calls());
    cancelled.sort();
    assert_eq!(cancelled, old, "a job that was not fenced was cancelled");
    for (tag, caller) in [1_u8, 2].into_iter().zip(&callers) {
        let job = rig.jobs_of(tag).remove(1);
        rig.host_finishes(job.id(), HOST, 0);
        assert!(matches!(rig.until_done(caller, 6).await, Finished::Ran(_)));
    }
    assert_eq!(rig.ingest.calls.lock().expect("calls").len(), 2);
}

/// Catches a fence time that leaves the scheduler no margin before it requeues: a worker that
/// is silent for T cancels its jobs, and the farm requeues them after G, so T plus the
/// scheduler's margin and one step stays under G, and the config that breaks it is refused.
#[test]
fn a_fence_time_that_leaves_no_margin_under_the_farms_grace_is_refused() {
    let mut cfg = Config::new("front", 1, 2_000, 16 * GIB);
    cfg.insecure_argv_secrets = true;
    cfg.fence_after = LEASE_GRACE - LEADER_LEASE_MARGIN - cfg.poll;
    assert_eq!(cfg.validate(), Err(ConfigError::Fence));
    cfg.fence_after = LEASE_GRACE;
    assert_eq!(cfg.validate(), Err(ConfigError::Fence));
}

/// Catches a stream the farm ended (a handover sends a status in place of a message) being
/// treated as an error to retry, or ignored: the jobs the farm gave up are cancelled, the
/// worker registers again as a new process naming the old one, and the farm runs the action
/// again at once as a new farm lease, which is a new job.
#[tokio::test(start_paused = true)]
async fn a_stream_the_farm_ended_has_its_runs_stopped_and_is_registered_again() {
    let (mut rig, caller, first) = running(Rig::start(&[HOST], 1, |_| {}).await).await;
    assert_eq!(
        rig.farm
            .end_worker_streams(&Status::unavailable("handover")),
        1
    );
    rig.pump().await;
    assert_eq!(cancels(&rig.fake.calls()), [first.id()]);
    let jobs = until_jobs(&mut rig, 1, 2, 3).await;
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert_ne!(jobs[1].id(), first.id());
    assert!(rig.fake.host_stopped(first.id()), "the old job runs on");
    rig.host_finishes(jobs[1].id(), HOST, 0);
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches the lease the farm gave up being run to its end beside its retry: a worker that is
/// not stepped for longer than the scheduler's grace is one the farm has given up, and its
/// next pass finds the farm last acknowledged it too long ago, so it cancels its job and
/// falls silent; the job's late result is never reported, and the action runs again as a new
/// job.
#[tokio::test(start_paused = true)]
async fn a_lease_the_farm_gave_up_is_cancelled_and_its_run_stopped() {
    let (mut rig, caller, first) = running(Rig::start(&[HOST], 1, |_| {}).await).await;
    // The executor is not stepped for longer than the grace: the farm gives the lease up
    // and places the action again.
    rig.advance(Duration::from_secs(65)).await;
    rig.farm.tick().await;
    rig.pump().await;
    assert_eq!(cancels(&rig.fake.calls()), [first.id()]);
    // The first job's late result, if its host still sends one, is not reported.
    assert!(rig.fake.finish(first.id(), 0));
    let jobs = until_jobs(&mut rig, 1, 2, 16).await;
    let newest = jobs.last().expect("a job").id().to_owned();
    assert_ne!(newest, first.id());
    rig.host_finishes(&newest, HOST, 0);
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
    assert_eq!(rig.ingest.calls.lock().expect("calls").len(), 1);
}

/// Catches a submit whose answer was lost being given up on, or making a second job: the job
/// service made the job, and the worker cannot know, so it sends the submit again with the same
/// job id; the service answers from the job it has, there is one job of the action, and it is
/// counted as submitted once and sent again once.
#[tokio::test(start_paused = true)]
async fn a_post_whose_answer_was_lost_is_submitted_again_with_the_same_id_and_makes_one_job() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
    rig.fake.inject_on("SubmitJob", Fault::LoseAnswer);
    rig.pump().await;
    let sent = submits(&rig.fake.calls());
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0].job_id, sent[1].job_id);
    let jobs = rig.jobs();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].id(), sent[0].job_id);
    assert_eq!(rig.counter("kbf_executor_jobs_submitted_total"), 1);
    assert_eq!(
        rig.counter("kbf_executor_jobs_resubmitted_after_lost_answer_total"),
        1
    );
    assert!(caller.finished().is_none());
    rig.host_finishes(jobs[0].id(), HOST, 0);
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
    assert_eq!(rig.ingest.calls.lock().expect("calls").len(), 1);
}

/// Catches a stream the farm dropped without a word (another stream registered as the same
/// worker) being taken for a quiet one: the job is cancelled, the worker registers again,
/// and the action, whose lease the farm gives up, runs again as a new job.
#[tokio::test(start_paused = true)]
async fn a_worker_registered_again_by_another_stream_registers_again_in_turn() {
    use kbf_sched::DaemonInstance;
    use kbf_server::farm::Reported;
    let (mut rig, caller, first) = running(Rig::start(&[HOST], 1, |_| {}).await).await;
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, _inbox) = tokio::sync::mpsc::unbounded_channel();
    let worker = rig.exec.workers()[0].worker.clone();
    rig.farm.register(
        &worker,
        DaemonInstance::new("another-process"),
        DaemonInstance::new(""),
        Reported::serving(kbf_types::Resources::new(2_000, 4 << 30), caps),
        outbound,
        kbf_proto::worker::ServerMessage::default(),
    );
    rig.pump().await;
    assert_eq!(cancels(&rig.fake.calls()), [first.id()]);
    rig.elapse(75).await;
    let jobs = rig.jobs_of(1);
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    rig.host_finishes(jobs[1].id(), HOST, 0);
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a submit the job service refuses for its own sake (it did not take the job, and
/// says nothing about the job being wrong) being the action's failure or being sent again
/// unchanged: it is infrastructure, so the farm runs the action again, and the refused submit
/// made no job.
#[tokio::test(start_paused = true)]
async fn a_refused_submit_is_rerun() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
    rig.fake.inject_on(
        "SubmitJob",
        Fault::Status {
            code: Code::ResourceExhausted,
            message: "busy".to_owned(),
        },
    );
    rig.pump().await;
    assert!(caller.finished().is_none());
    assert_eq!(rig.counter("kbf_executor_runs_total{outcome=\"infra\"}"), 1);
    let jobs = until_jobs(&mut rig, 1, 1, 12).await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    rig.host_finishes(jobs[0].id(), HOST, 0);
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches an ingest that takes long (a slow store) stopping the worker's heartbeats: the
/// farm then gives its lease up and runs the action again beside the one being read. The
/// read runs on its own, the worker goes on heartbeating, and the result is reported when
/// the read ends.
#[tokio::test(start_paused = true)]
async fn a_slow_ingest_does_not_stop_the_heartbeats() {
    let (mut rig, caller, job) = running(Rig::start(&[HOST], 1, |_| {}).await).await;
    rig.ingest
        .hold
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(rig.fake.finish(job.id(), 0));
    rig.pump().await;
    rig.elapse(150).await;
    assert!(caller.finished().is_none());
    assert_eq!(rig.exec.workers()[0].runs, 1);
    assert!(!rig.exec.workers()[0].silent);
    assert_eq!(rig.jobs_of(1).len(), 1, "the farm ran the action again");
    rig.ingest.release.notify_one();
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a read left running for a worker that has let its tasks go: the farm ended the
/// stream while the pack of a finished job was being read, the worker forgot the task, and
/// the read is abandoned with it. One release of the ingest then lets the read of the
/// action's next job end (a read left behind would take it), and the job that had ended is
/// never cancelled.
#[tokio::test(start_paused = true)]
async fn the_read_of_a_worker_that_goes_is_abandoned() {
    let (mut rig, caller, job) = running(Rig::start(&[HOST], 1, |_| {}).await).await;
    rig.ingest
        .hold
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(rig.fake.finish(job.id(), 0));
    rig.pump().await;
    assert_eq!(rig.ingest.calls.lock().expect("calls").len(), 1);
    assert_eq!(
        rig.farm
            .end_worker_streams(&Status::unavailable("handover")),
        1
    );
    rig.pump().await;
    // The worker registered again, and the farm may already have placed the action's next
    // job on it in the same pump: what shows the old task is gone is the ingest below.
    assert!(!rig.exec.workers()[0].silent);
    assert!(caller.finished().is_none());
    assert!(cancels(&rig.fake.calls()).is_empty());
    let jobs = until_jobs(&mut rig, 1, 2, 12).await;
    rig.host_finishes(jobs[1].id(), HOST, 0);
    rig.pump().await;
    assert_eq!(rig.ingest.calls.lock().expect("calls").len(), 2);
    rig.ingest.release.notify_one();
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a job service that accepts a request and never answers holding the executor's pass
/// for the client's own 30 s a call: a pass makes several, one after another, with the
/// heartbeats to the farm among them, and the farm requeues a worker it has not heard from
/// after a minute. Each call gives up after the call timeout, the pass ends, and the job goes
/// on being tracked.
#[tokio::test(start_paused = true)]
async fn a_job_service_that_never_answers_does_not_hold_the_pass() {
    let (mut rig, caller, job) = running(Rig::start(&[HOST], 1, |_| {}).await).await;
    rig.fake.inject_on("GetJob", Fault::Hang);
    let in_time = rig.step_through(Duration::from_secs(6)).await;
    assert!(in_time, "the pass waited for the client's own timeout");
    assert_eq!(rig.exec.workers().len(), 1);
    assert!(!rig.exec.workers()[0].silent);
    assert_eq!(rig.exec.workers()[0].runs, 1);
    assert!(rig.fake.finish(job.id(), 0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches the fence being judged by the clock at the start of the pass: a pass that spent
/// its time waiting for the job service would then fence a pass late, with the job service
/// silent for longer than the fence time. The clock is read where the silence is counted.
#[tokio::test(start_paused = true)]
async fn the_fence_counts_the_time_the_pass_itself_took() {
    let (mut rig, _caller, job) = running(
        Rig::start(&[HOST], 1, |cfg| {
            cfg.fence_after = Duration::from_secs(10);
        })
        .await,
    )
    .await;
    assert!(!rig.exec.workers()[0].silent);
    rig.advance(Duration::from_secs(9)).await;
    rig.fake.inject_on("GetJob", Fault::Hang);
    // The pass starts 9 s after the job service last answered, and waits 5 s for it.
    let in_time = rig.step_through(Duration::from_secs(6)).await;
    assert!(in_time);
    assert!(rig.exec.workers()[0].silent, "the worker was not fenced");
    assert_eq!(cancels(&rig.fake.calls()), [job.id()]);
}
