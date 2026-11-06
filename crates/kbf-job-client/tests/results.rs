//! Results and logs are kept for the retention window after a job ends (design sections
//! 3.2, 5.3 and 11), and the tail of each stream is its last 8 KiB.

mod support;

use std::time::Duration;

use js_proto::v1::{JobState, JobTimes, ListJobsRequest, ResultReason, StartFailureCode};
use kbf_job_client::Error;
use support::{HOST, fake, spec, status_code, to_running};
use tonic::Code;

const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);
const MS: Duration = Duration::from_millis(1);
const DAY: Duration = Duration::from_secs(24 * 3600);

/// Asserts both edges of the retention window: with `set_retention(7 days)`, a finished job
/// answers `get_result`, `get_logs` and `get_job` 1 ms before the window closes, and each is
/// `Status{NOT_FOUND}` (not `Unreachable`, not a transport error) once it has; its id
/// conflicts with another job inside the window and is free after it. Catches a window off
/// by a millisecond at either edge, and an expired job answered as unreachable, which a
/// caller would retry for ever.
#[tokio::test]
async fn a_result_is_kept_for_the_window_then_not_found() {
    let (server, client) = fake().await;
    server.set_retention(WEEK);
    client.submit_job(spec("kept")).await.unwrap();
    to_running(&server, "kept");
    assert!(server.write_logs("kept", b"ok\n", b""));
    assert!(server.finish("kept", 0));

    server.advance(WEEK - MS);
    let result = client.get_result("kept").await.unwrap();
    assert_eq!(
        (result.state(), result.exit_code, result.reason()),
        (JobState::Succeeded, Some(0), ResultReason::Exit)
    );
    assert_eq!(result.stdout_tail, b"ok\n");
    assert_eq!(client.get_logs("kept").await.unwrap().stdout_tail, b"ok\n");
    assert_eq!(
        client.get_job("kept").await.unwrap().state(),
        JobState::Succeeded
    );
    let mut other = spec("kept");
    other.command = "/bin/other".to_owned();
    let conflict = client.submit_job(other.clone()).await.unwrap_err();
    assert!(
        conflict.is(js_proto::v1::RefusalCode::JobIdConflict),
        "{conflict:?}"
    );

    server.advance(MS);
    let errors = [
        client.get_result("kept").await.unwrap_err(),
        client.get_logs("kept").await.unwrap_err(),
        client.get_job("kept").await.unwrap_err(),
    ];
    for error in errors {
        assert_eq!(status_code(&error), Code::NotFound, "{error:?}");
        assert!(!error.may_have_applied());
    }
    let reused = client.submit_job(other).await.unwrap();
    assert_eq!(
        (reused.id.as_str(), reused.state()),
        ("kept", JobState::Queued)
    );
}

/// Asserts what a live job answers: `get_result` of a queued and of a running job is a
/// definite `FAILED_PRECONDITION`, not `NOT_FOUND`; `get_logs` of a running job answers the tail so far, each
/// stream cut to its last 8 KiB and marked truncated once cut. Catches a running job's logs
/// refused, a tail that keeps the head, and a missing result taken for a missing job.
#[tokio::test]
async fn a_live_job_has_logs_and_no_result() {
    let (server, client) = fake().await;
    client.submit_job(spec("live")).await.unwrap();
    let queued = client.get_result("live").await.unwrap_err();
    assert_eq!(status_code(&queued), Code::FailedPrecondition);
    to_running(&server, "live");

    let early = client.get_result("live").await.unwrap_err();
    assert!(
        matches!(
            early,
            Error::Status {
                code: Code::FailedPrecondition,
                ..
            }
        ),
        "{early:?}"
    );
    assert!(!early.may_have_applied());

    assert!(server.write_logs("live", b"line 1\n", b"warn\n"));
    let logs = client.get_logs("live").await.unwrap();
    assert_eq!(
        (
            logs.state(),
            logs.stdout_tail.as_slice(),
            logs.stderr_tail.as_slice(),
            logs.truncated
        ),
        (JobState::Running, &b"line 1\n"[..], &b"warn\n"[..], false)
    );

    let mut long = vec![b'x'; 8192];
    long.extend_from_slice(b"END");
    assert!(server.write_logs("live", &long, b""));
    let logs = client.get_logs("live").await.unwrap();
    assert_eq!(logs.stdout_tail.len(), 8192);
    assert!(logs.stdout_tail.ends_with(b"xxEND"));
    assert!(logs.truncated);
    assert_eq!(logs.stderr_tail, b"warn\n");
}

/// Asserts a job that fails to start: from queued, placed and starting it ends failed to
/// start with the code the fake was given, its result has no exit code and no reason, and a
/// running job cannot fail to start. Catches a start failure that loses its code or reads as
/// an exit.
#[tokio::test]
async fn a_start_failure_keeps_its_code() {
    let (server, client) = fake().await;
    for id in ["queued", "placed", "starting", "running"] {
        client.submit_job(spec(id)).await.unwrap();
    }
    assert!(server.place("placed", support::HOST));
    assert!(server.place("starting", support::HOST) && server.starting("starting"));
    to_running(&server, "running");
    let code = js_proto::v1::StartFailureCode::StartTimeout;
    for id in ["queued", "placed", "starting"] {
        assert!(server.fail_to_start(id, code), "{id}");
        let job = client.get_job(id).await.unwrap();
        assert_eq!(
            (job.state(), job.start_failure()),
            (JobState::FailedToStart, code),
            "{id}"
        );
        let result = client.get_result(id).await.unwrap();
        assert_eq!(
            (result.exit_code, result.reason()),
            (None, ResultReason::Unspecified),
            "{id}"
        );
    }
    assert!(!server.fail_to_start("running", code));
}

/// Asserts a start the host's agent refused: from queued, placed and starting it ends failed to
/// start with `AGENT_REFUSED` and every read of it shows the agent's detail as given, a job
/// failed to start with a code alone shows no detail, and a running job cannot be refused.
/// Catches a refusal that drops the detail, or one detail shown on another job.
#[tokio::test]
async fn a_refused_start_keeps_the_agents_detail() {
    let (server, client) = fake().await;
    for id in ["queued", "placed", "starting", "running", "bare"] {
        client.submit_job(spec(id)).await.unwrap();
    }
    assert!(server.place("placed", support::HOST));
    assert!(server.place("starting", support::HOST) && server.starting("starting"));
    to_running(&server, "running");
    for (id, detail) in [
        ("queued", "egress-unchecked"),
        ("placed", "host-tainted: marker"),
        ("starting", ""),
    ] {
        assert!(server.refuse_start(id, detail), "{id}");
        let job = client.get_job(id).await.unwrap();
        assert_eq!(
            (
                job.state(),
                job.start_failure(),
                job.start_failure_detail.as_str()
            ),
            (
                JobState::FailedToStart,
                StartFailureCode::AgentRefused,
                detail
            ),
            "{id}"
        );
    }
    assert!(server.fail_to_start("bare", StartFailureCode::NoCapacity));
    assert_eq!(
        client.get_job("bare").await.unwrap().start_failure_detail,
        ""
    );
    assert!(!server.refuse_start("running", "egress-unchecked"));
    assert_eq!(
        client
            .get_job("running")
            .await
            .unwrap()
            .start_failure_detail,
        ""
    );
}

/// Asserts the times of a job: with the clock moved on between each step, a job queued at 0,
/// placed 1 s later, starting 2 s after that, running 500 ms after that and finished 4 s
/// later has, in `get_job` and in `get_result`, exactly the times 0, 1000, 3500 and 7500 ms
/// (the `starting` step stamps nothing of its own). Catches a time stamped with the wrong
/// step's clock, and a result whose times differ from the job's.
#[tokio::test]
async fn the_times_of_a_job_are_those_of_its_steps() {
    let (server, client) = fake().await;
    client.submit_job(spec("timed")).await.unwrap();
    server.advance(Duration::from_secs(1));
    assert!(server.place("timed", HOST));
    server.advance(Duration::from_secs(2));
    assert!(server.starting("timed"));
    server.advance(Duration::from_millis(500));
    assert!(server.run("timed"));
    server.advance(Duration::from_secs(4));
    let live = client.get_job("timed").await.unwrap().times.unwrap();
    assert_eq!(live.ended_at_ms, None);
    assert!(server.finish("timed", 0));

    let times = JobTimes {
        queued_at_ms: 0,
        placed_at_ms: Some(1000),
        started_at_ms: Some(3500),
        ended_at_ms: Some(7500),
    };
    assert_eq!(server.now_ms(), 7500);
    assert_eq!(client.get_job("timed").await.unwrap().times, Some(times));
    assert_eq!(client.get_result("timed").await.unwrap().times, Some(times));
}

/// Asserts that the window counts from the end of a job, not from its queueing or its
/// start: a job that sat queued for 3 days and ran for 5 more, with the window at 7 days,
/// still answers `get_job`, `get_result` and `get_logs` 1 ms before it ended 7 days ago, and
/// all three are `NOT_FOUND` at 7 days exactly. Catches a window counted from the queued
/// time (the job would be gone the moment it ended) and one off by a millisecond.
#[tokio::test]
async fn retention_counts_from_the_end_of_the_job() {
    let (server, client) = fake().await;
    server.set_retention(WEEK);
    client.submit_job(spec("slow")).await.unwrap();
    server.advance(3 * DAY);
    to_running(&server, "slow");
    server.advance(5 * DAY);
    assert!(server.finish("slow", 0));

    server.advance(WEEK - MS);
    assert_eq!(
        client.get_job("slow").await.unwrap().state(),
        JobState::Succeeded
    );
    assert_eq!(
        client.get_result("slow").await.unwrap().state(),
        JobState::Succeeded
    );
    assert!(client.get_logs("slow").await.is_ok());

    server.advance(MS);
    let errors = [
        client.get_job("slow").await.unwrap_err(),
        client.get_result("slow").await.unwrap_err(),
        client.get_logs("slow").await.unwrap_err(),
    ];
    for error in errors {
        assert_eq!(status_code(&error), Code::NotFound, "{error:?}");
    }
}

/// Asserts the default window without `set_retention`: 7 days from the end, answered at
/// 7 days less 1 ms and `NOT_FOUND` at 7 days. Catches a default of another length or none.
#[tokio::test]
async fn the_default_retention_is_seven_days() {
    let (server, client) = fake().await;
    client.submit_job(spec("default")).await.unwrap();
    to_running(&server, "default");
    assert!(server.finish("default", 1));

    server.advance(WEEK - MS);
    assert_eq!(
        client.get_job("default").await.unwrap().state(),
        JobState::Failed
    );
    server.advance(MS);
    let error = client.get_job("default").await.unwrap_err();
    assert_eq!(status_code(&error), Code::NotFound);
}

/// Asserts that only an ended job expires: with the window at one hour, a running job and a
/// queued one 30 days old are still answered by `get_job`, and the running one by `get_logs`
/// with its state, and both are listed. Catches a sweep that forgets jobs by age since
/// queueing.
#[tokio::test]
async fn a_live_job_never_expires() {
    let (server, client) = fake().await;
    server.set_retention(Duration::from_secs(3600));
    client.submit_job(spec("running")).await.unwrap();
    client.submit_job(spec("queued")).await.unwrap();
    to_running(&server, "running");
    server.advance(30 * DAY);

    assert_eq!(
        client.get_job("running").await.unwrap().state(),
        JobState::Running
    );
    assert_eq!(
        client.get_job("queued").await.unwrap().state(),
        JobState::Queued
    );
    assert_eq!(
        client.get_logs("running").await.unwrap().state(),
        JobState::Running
    );
    let listed = client
        .list_all_jobs(ListJobsRequest::default())
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
}

/// Asserts that a job of each end expires: succeeded, failed, cancelled, lost and failed to
/// start jobs ended together are all answered, in their state and listed, 1 ms before the
/// window of one hour closes; at the window none is answered by `get_job`, none is listed,
/// `cancel_job` of each is `NOT_FOUND` and the fake holds none. Catches an end that forgets
/// to stamp its time, so that its job is kept for ever.
#[tokio::test]
async fn a_job_of_each_end_expires() {
    let (server, client) = fake().await;
    let hour = Duration::from_secs(3600);
    server.set_retention(hour);
    for id in ["succeeded", "failed", "cancelled", "lost", "no-start"] {
        client.submit_job(spec(id)).await.unwrap();
    }
    for id in ["succeeded", "failed", "lost"] {
        to_running(&server, id);
    }
    assert!(server.finish("succeeded", 0));
    assert!(server.finish("failed", 2));
    assert!(server.lose("lost"));
    client.cancel_job("cancelled").await.unwrap();
    assert!(server.fail_to_start("no-start", StartFailureCode::StartTimeout));
    let ends = [
        ("succeeded", JobState::Succeeded),
        ("failed", JobState::Failed),
        ("cancelled", JobState::Cancelled),
        ("lost", JobState::Lost),
        ("no-start", JobState::FailedToStart),
    ];

    server.advance(hour - MS);
    for (id, state) in ends {
        assert_eq!(client.get_job(id).await.unwrap().state(), state, "{id}");
    }
    let listed = client
        .list_all_jobs(ListJobsRequest::default())
        .await
        .unwrap();
    assert_eq!(listed.len(), 5);

    server.advance(MS);
    for (id, _) in ends {
        let read = client.get_job(id).await.unwrap_err();
        assert_eq!(status_code(&read), Code::NotFound, "{id}");
        let cancel = client.cancel_job(id).await.unwrap_err();
        assert_eq!(status_code(&cancel), Code::NotFound, "{id}");
    }
    let listed = client
        .list_all_jobs(ListJobsRequest::default())
        .await
        .unwrap();
    assert!(listed.is_empty());
    assert!(server.jobs().is_empty());
}
