//! How soon a job is read back: at once when it is submitted, at doubling steps while it has
//! not started, and a poll apart once it runs. Time moves only when a test advances it (the
//! rig's paused clock), so every read below is counted at the step of the clock it went out at.

use std::time::Duration;

use js_proto::v1::JobState;
use kbf_front::{Finished, Stage};
use kbf_job_client::fake::Fault;
use tonic::Code;

use crate::executor::tests::{Caller, Rig, job_ended, settle};

/// One step of the rig's clock: the first step of a job that has not started.
const TICK: Duration = Duration::from_millis(10);

/// Lets the tasks run, with the clock where it is, until `caller`'s job is at the job service.
async fn submitted_now(rig: &Rig, caller: &Caller) -> String {
    let job = rig.job_of(caller, 0);
    for _ in 0..8 {
        settle().await;
        if rig.fake.submitted(&job).is_some() {
            return job;
        }
    }
    panic!("{job} was not submitted before the clock moved");
}

/// Moves the clock `ticks` steps of [`TICK`] and lists, in milliseconds since the first, the
/// steps after which the job service had been sent more reads of a job; `at` runs before each
/// step with the milliseconds passed so far.
async fn reads_over(rig: &Rig, ticks: u64, mut at: impl FnMut(u64)) -> Vec<u64> {
    let mut seen = rig.calls_of("GetJob");
    let mut reads = Vec::new();
    for tick in 1..=ticks {
        at((tick - 1) * 10);
        rig.advance(TICK).await;
        let now = rig.calls_of("GetJob");
        assert!(now <= seen + 1, "two reads in one step at {} ms", tick * 10);
        if now > seen {
            reads.push(tick * 10);
        }
        seen = now;
    }
    reads
}

/// Catches the first read of a job sent a poll after its submit: a job that is running at its
/// first read is seen running (the callers are shown it executing, and its queue phase ends)
/// before the clock moves at all, with one read; the next read waits the poll.
#[tokio::test(start_paused = true)]
async fn a_job_running_at_its_first_read_is_seen_before_the_clock_moves() {
    let rig = Rig::start().await;
    rig.fake.inject_on("GetJob", job_ended(JobState::Running));
    let caller = rig.submit(1);
    let job = submitted_now(&rig, &caller).await;
    settle().await;
    assert_eq!(rig.calls_of("GetJob"), 1, "one read, sent at once");
    assert_eq!(caller.seen(), Stage::Executing);
    assert_eq!(
        rig.counted("kbf_executor_phase_seconds_count{phase=\"queue\"} "),
        1
    );
    let reads = reads_over(&rig, 25, |_| {}).await;
    assert_eq!(reads, [250], "the job runs: the next read is a poll later");
    rig.host_finishes(&job, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
}

/// Catches a job that starts while it waits seen only at the next poll: read queued at once and
/// at 10 and 30 ms, it runs at 30 ms and is seen running at the next step, 70 ms, and not
/// before; from then it is read a poll apart (320 ms), not at the steps.
#[tokio::test(start_paused = true)]
async fn a_job_that_starts_after_three_steps_is_seen_at_the_next_step() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let job = submitted_now(&rig, &caller).await;
    settle().await;
    assert_eq!(rig.calls_of("GetJob"), 1, "the first read, sent at once");
    assert_ne!(caller.seen(), Stage::Executing, "read queued");
    let mut stages = Vec::new();
    let reads = reads_over(&rig, 33, |ms| {
        if ms == 30 {
            rig.host_runs(&job);
        }
        stages.push((ms, caller.seen() == Stage::Executing));
    })
    .await;
    assert_eq!(reads, [10, 30, 70, 320]);
    let first_seen = stages.iter().find(|(_, executing)| *executing).map(|s| s.0);
    assert_eq!(first_seen, Some(70), "seen running at the next step");
    assert!(rig.fake.finish(&job, 0));
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
}

/// Catches reads of a job that waits for a host that do not slow down to the poll, which would
/// multiply the job service's reads in a burst of starts: over two seconds queued the job is
/// read 12 times (at once, then 10, 30, 70, 150 and 310 ms, then a poll apart), where reads a
/// poll apart from the submit are 8: the steps add at most 5 reads a job at the default poll.
#[tokio::test(start_paused = true)]
async fn a_job_that_waits_is_read_a_bounded_number_of_times() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let job = submitted_now(&rig, &caller).await;
    settle().await;
    let reads = reads_over(&rig, 200, |_| {}).await;
    assert_eq!(
        reads,
        [10, 30, 70, 150, 310, 560, 810, 1060, 1310, 1560, 1810]
    );
    assert_eq!(
        rig.calls_of("GetJob"),
        12,
        "the first read and the eleven after"
    );
    rig.host_finishes(&job, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
}

/// Catches a failed read of a job that has not started tried again at a step, not the poll: the
/// first read is answered `FAILED_PRECONDITION`, and the next one waits a whole poll; the job,
/// queued then, is read at the steps from there.
#[tokio::test(start_paused = true)]
async fn a_failed_first_read_waits_the_poll_and_the_steps_follow() {
    let rig = Rig::start().await;
    rig.fake.inject_on(
        "GetJob",
        Fault::Status {
            code: Code::FailedPrecondition,
            message: "no".to_owned(),
        },
    );
    let caller = rig.submit(1);
    let job = submitted_now(&rig, &caller).await;
    settle().await;
    assert_eq!(rig.calls_of("GetJob"), 1, "the first read, sent at once");
    let reads = reads_over(&rig, 30, |_| {}).await;
    assert_eq!(reads, [250, 260, 280]);
    rig.host_finishes(&job, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
}
