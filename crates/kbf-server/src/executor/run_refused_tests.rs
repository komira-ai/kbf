//! A job a host would not start now (no room, the host held for an update, or tainted): the
//! operation places it again, spending no attempt, within the action's deadline and the bound
//! of refused starts; and a refusal that says no host can ever run the job is answered at once.
//! The clock is tokio's paused one, moved by the rig.

use std::time::Duration;

use kbf_front::{Dispatch, Finished};
use kbf_proto::google::rpc;
use tonic::Code;

use crate::executor::InputTree;
use crate::executor::outcome::RESUBMITS;
use crate::executor::refused::REFUSED_STARTS;
use crate::executor::tests::{Caller, Rig};

fn status_of(finished: Finished) -> rpc::Status {
    match finished {
        Finished::Failed(status) => status,
        other => panic!("not a failure: {other:?}"),
    }
}

/// Runs until `caller` is answered or the job of `attempt` is submitted: whether it was.
async fn next_or_done(rig: &Rig, caller: &Caller, attempt: u32) -> bool {
    let job = rig.job_of(caller, attempt);
    rig.until("the next job or the answer", || {
        caller.finished().is_some() || rig.fake.submitted(&job).is_some()
    })
    .await;
    caller.finished().is_none()
}

/// Asserts that a start refused once for capacity is placed again and the action's result is
/// its callers': the second job, of the next attempt, runs it, and the refusal is counted as a
/// run that ended `refused`.
#[tokio::test(start_paused = true)]
async fn a_start_refused_once_for_capacity_is_placed_again_and_the_action_runs() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let first = rig.submitted(&caller, 0).await;
    assert!(rig.fake.refuse_start(&first, "no-capacity"));
    let second = rig.submitted(&caller, 1).await;
    rig.host_finishes(&second, 0);

    let finished = rig.until_done(&caller).await;
    assert!(matches!(finished, Finished::Ran(ref result) if result.exit_code == 0));
    assert_eq!(rig.fake.jobs().len(), 2);
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"refused\"} "),
        1
    );
    assert_eq!(rig.counted("kbf_executor_runs_total{outcome=\"ran\"} "), 1);
    assert_eq!(rig.counted("kbf_executor_attempts_resubmitted_total "), 1);
}

/// Asserts that refused starts spend none of the operation's attempts: two lost jobs use up the
/// [`RESUBMITS`] it has, and three refusals between them (the agent's `host-held`,
/// `no-capacity` and `host-tainted`) still leave it the run that answers.
#[tokio::test(start_paused = true)]
async fn refused_starts_spend_no_attempt() {
    assert_eq!(RESUBMITS, 2);
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let lose = |job: &str| {
        rig.host_runs(job);
        assert!(rig.fake.lose(job));
    };
    lose(&rig.submitted(&caller, 0).await);
    assert!(
        rig.fake
            .refuse_start(&rig.submitted(&caller, 1).await, "host-held")
    );
    assert!(
        rig.fake
            .refuse_start(&rig.submitted(&caller, 2).await, "no-capacity")
    );
    lose(&rig.submitted(&caller, 3).await);
    assert!(
        rig.fake
            .refuse_start(&rig.submitted(&caller, 4).await, "host-tainted")
    );
    rig.host_finishes(&rig.submitted(&caller, 5).await, 0);

    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(rig.fake.jobs().len(), 6);
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"refused\"} "),
        3
    );
    assert_eq!(rig.counted("kbf_executor_runs_total{outcome=\"lost\"} "), 2);
}

/// Asserts that a job refused at every start is answered once the action's deadline (its
/// timeout of 60 s, from when it was asked for) comes before the next placement:
/// `UNAVAILABLE`, naming each refusal and how many times it was given, in full; and that no job
/// is submitted after the answer.
#[tokio::test(start_paused = true)]
async fn refused_until_the_deadline_fails_naming_the_refusals() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    rig.submitted(&caller, 0).await;
    let (mut capacity, mut held) = (0_u32, 0_u32);
    let mut attempt = 0;
    loop {
        let job = rig.job_of(&caller, attempt);
        let code = if attempt % 2 == 0 {
            capacity += 1;
            "no-capacity"
        } else {
            held += 1;
            "host-held"
        };
        assert!(rig.fake.refuse_start(&job, code));
        attempt += 1;
        if !next_or_done(&rig, &caller, attempt).await {
            break;
        }
    }
    let status = status_of(rig.until_done(&caller).await);

    assert!(attempt < REFUSED_STARTS, "the deadline, not the bound");
    assert_eq!(status.code, Code::Unavailable as i32);
    assert_eq!(
        status.message,
        format!(
            "the farm's hosts refused to start the job {attempt} times and the next placement \
             would come after the action's deadline (60s from when it was asked for): \
             no-capacity {capacity}, host-held {held} (not the action's own failure)"
        )
    );
    rig.advance(Duration::from_secs(300)).await;
    assert_eq!(
        rig.fake.jobs().len(),
        attempt as usize,
        "no job after the answer"
    );
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"refused\"} "),
        u64::from(attempt)
    );
    assert_eq!(
        rig.counted("kbf_executor_attempts_resubmitted_total "),
        u64::from(attempt - 1)
    );
}

/// Asserts the bound on refused starts when the action's deadline is far: the job is placed
/// again [`REFUSED_STARTS`] times, and the refusal after those is answered `UNAVAILABLE` naming
/// the bound and the refusals.
#[tokio::test(start_paused = true)]
async fn refused_past_the_bound_fails_naming_the_bound() {
    let three_hours = Duration::from_secs(3 * 3600);
    let rig = Rig::with(|cfg| cfg.max_run = three_hours).await;
    if let Ok(facts) = rig.inputs.facts.lock().unwrap().as_mut() {
        facts.timeout = Some(three_hours);
    }
    let caller = rig.submit(1);
    rig.submitted(&caller, 0).await;
    let mut attempt = 0;
    loop {
        assert!(
            rig.fake
                .refuse_start(&rig.job_of(&caller, attempt), "no-capacity")
        );
        attempt += 1;
        if !next_or_done(&rig, &caller, attempt).await {
            break;
        }
    }
    let status = status_of(rig.until_done(&caller).await);

    assert_eq!(attempt, REFUSED_STARTS + 1);
    assert_eq!(status.code, Code::Unavailable as i32);
    assert_eq!(
        status.message,
        "the farm's hosts refused to start the job 101 times and the job was placed again 100 \
         times, the most an operation places it: no-capacity 101 (not the action's own failure)"
    );
}

/// Asserts that a refusal that says no host can ever run the job, the agent's
/// `platform-mismatch`, is answered at once by name: `INVALID_ARGUMENT`, one job, none placed
/// again.
#[tokio::test(start_paused = true)]
async fn a_start_no_host_can_ever_make_fails_at_once_by_name() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    assert!(rig.fake.refuse_start(&job, "platform-mismatch"));

    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(status.code, Code::InvalidArgument as i32);
    assert_eq!(
        status.message,
        "the host refused to start the job: platform-mismatch (the job's image is not built \
         for the platform the job names, so no host can ever run it)"
    );
    rig.advance(Duration::from_secs(60)).await;
    assert_eq!(rig.fake.jobs().len(), 1);
    assert_eq!(rig.counted("kbf_executor_attempts_resubmitted_total "), 0);
}

/// Asserts that a submit the job service refuses because no host could ever run the job
/// (`no-capable-host`: here it holds only x86_64 hosts and the job asks for arm64) is answered
/// at once by name, with one submit sent.
#[tokio::test(start_paused = true)]
async fn a_submit_no_host_can_ever_run_fails_at_once_by_name() {
    let rig = Rig::with(|cfg| cfg.arch = "aarch64".to_owned()).await;
    let caller = rig.submit(1);

    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(status.code, Code::InvalidArgument as i32);
    assert_eq!(
        status.message,
        "the job service refused the job: no-capable-host"
    );
    rig.advance(Duration::from_secs(60)).await;
    assert_eq!(rig.calls_of("SubmitJob"), 1);
    assert!(rig.fake.jobs().is_empty());
}

/// Asserts that a caller's cancel is heard while a refused start waits to be placed again: the
/// operation is answered `CANCELLED` and no further job is submitted.
#[tokio::test(start_paused = true)]
async fn a_cancel_while_a_refused_start_waits_ends_the_operation() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    assert!(rig.fake.refuse_start(&job, "host-held"));
    rig.until("the refusal is seen", || {
        rig.counted("kbf_executor_runs_total{outcome=\"refused\"} ") == 1
    })
    .await;
    assert!(rig.exec.cancel(&caller.name));

    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(status.code, Code::Cancelled as i32);
    rig.advance(Duration::from_secs(60)).await;
    assert_eq!(rig.fake.jobs().len(), 1, "no job after the cancel");
}

/// Asserts that refused starts and refused tree lists, which spend no attempt, do not give the
/// operation more than its own: after one of each, its [`RESUBMITS`] attempts and the first run
/// are three lost jobs, and the third answers its callers `UNAVAILABLE` with no sixth job.
#[tokio::test(start_paused = true)]
async fn the_attempts_still_run_out_after_refused_starts() {
    let rig = Rig::with(|cfg| cfg.input_tree = InputTree::Provider).await;
    let caller = rig.submit(1);
    assert!(
        rig.fake
            .refuse_start(&rig.submitted(&caller, 0).await, "no-capacity")
    );
    let listed = rig.submitted(&caller, 1).await;
    assert!(
        rig.fake
            .refuse_start(&listed, "tree list not supported: missing_sidecar")
    );
    for attempt in 2..=(2 + RESUBMITS) {
        let job = rig.submitted(&caller, attempt).await;
        rig.host_runs(&job);
        assert!(rig.fake.lose(&job));
    }

    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(status.code, Code::Unavailable as i32);
    assert_eq!(status.message, "the job was lost with its host");
    rig.advance(Duration::from_secs(60)).await;
    assert_eq!(rig.fake.jobs().len(), 5, "no job past the attempts");
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"refused\"} "),
        1
    );
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"list_refused\"} "),
        1
    );
    assert_eq!(rig.counted("kbf_executor_runs_total{outcome=\"lost\"} "), 3);
}

/// Asserts that a start the job service placed again itself until it gave up (`no-capacity`
/// followed by the hosts that refused it) spends one of the operation's attempts like any
/// infrastructure ending: after it, two lost jobs end the operation `UNAVAILABLE`, with no
/// fourth job and no run counted `refused`.
#[tokio::test(start_paused = true)]
async fn a_start_the_job_service_already_retried_spends_an_attempt() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let first = rig.submitted(&caller, 0).await;
    assert!(rig.fake.refuse_start(&first, "no-capacity: host-a, host-b"));
    for attempt in 1..=RESUBMITS {
        let job = rig.submitted(&caller, attempt).await;
        rig.host_runs(&job);
        assert!(rig.fake.lose(&job));
    }

    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(status.code, Code::Unavailable as i32);
    assert_eq!(status.message, "the job was lost with its host");
    rig.advance(Duration::from_secs(60)).await;
    assert_eq!(rig.fake.jobs().len(), 3);
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"refused\"} "),
        0
    );
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"infra\"} "),
        1
    );
}
