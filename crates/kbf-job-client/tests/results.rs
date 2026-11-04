//! Results and logs are kept for the retention window after a job ends (design sections
//! 3.2, 5.3 and 11), and the tail of each stream is its last 8 KiB.

mod support;

use std::time::Duration;

use js_proto::v1::{JobState, ResultReason};
use kbf_job_client::Error;
use support::{fake, spec, status_code, to_running};
use tonic::Code;

const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);
const MS: Duration = Duration::from_millis(1);

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
