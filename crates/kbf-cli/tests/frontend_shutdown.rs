//! Stopping a `kbf frontend` process while an action runs as a job of the job service (issue
//! #168, and kbf#791): SIGTERM keeps an open Execute or WaitExecution stream until its job is
//! done, because Buck2 does not recover a stream that breaks; when `--shutdown-timeout-secs`
//! runs out the streams end UNAVAILABLE; a second SIGTERM ends the drain; and the drain
//! endpoint drains the process the same way. The job service is in this process
//! (`kbf_job_client::fake`), the frontend and the storage node it serves from are children, and
//! the jobs are moved from state to state by the test.

#![cfg(unix)]

#[path = "frontend_shutdown/rig.rs"]
mod rig;

use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::{ExecuteResponse, WaitExecutionRequest};
use prost::Message as _;
use rig::{PROMPT, Rig, next};
use tonic::Code;

/// Catches issue #168: a server that dies on SIGTERM (no handler), or stops serving at once, so
/// an open Execute or WaitExecution stream fails on the client with UNKNOWN (an h2 error)
/// instead of UNAVAILABLE; and one that does not exit 0 promptly once the streams are ended.
/// The deadline is 1 s here (the default is none). The operation is running as a job that
/// never ends, so the drain keeps both streams until the deadline closes them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_ends_open_execute_streams_unavailable() {
    let mut rig = Rig::start("deadline", &["--shutdown-timeout-secs=1"]).await;
    let (mut executed, name) = rig.run_on_a_host("open across a SIGTERM").await;
    let mut waited = rig
        .exec()
        .wait_execution(WaitExecutionRequest { name })
        .await
        .expect("WaitExecution")
        .into_inner();
    next(&mut waited).await;

    rig.frontend.terminate();
    for (what, stream) in [("Execute", &mut executed), ("WaitExecution", &mut waited)] {
        let ended = tokio::time::timeout(PROMPT, stream.message())
            .await
            .unwrap_or_else(|_| panic!("{what}: no end within {PROMPT:?} of SIGTERM"));
        let status = ended.expect_err(what);
        assert_eq!(
            status.code(),
            Code::Unavailable,
            "{what}: {}: {}",
            status.code(),
            status.message()
        );
        assert!(
            status.message().contains("shutting down"),
            "{what}: {status:?}"
        );
    }
    let status = rig.frontend.exited(PROMPT).await;
    assert!(status.success(), "kbf exited with {status}");
}

/// Catches (kbf#791): a SIGTERM that ends open streams or exits while an action is in flight
/// (Buck2 never reconnects, so the action would fail). With no deadline (the default) the
/// stream of an operation with a running job stays open and the process keeps running after the
/// signal; once the frontend reports the drain begun (`/readyz` 503) and counts the stream as
/// still open, nothing has been sent on it. The defect it catches is the signal handler closing
/// the streams as it drains.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_keeps_a_running_stream_when_there_is_no_deadline() {
    use once::now_or_never;

    let mut rig = Rig::start("no-deadline", &[]).await;
    let (mut executed, _) = rig.run_on_a_host("kept across a SIGTERM").await;

    rig.frontend.terminate();
    rig.not_ready().await;
    let drain = rig.drain().await;
    assert_eq!(drain["draining"], true, "{drain}");
    assert_eq!(drain["open_executions"], 1, "{drain}");
    assert_eq!(drain["queued_operations"], 0, "{drain}");
    let quiet = now_or_never(executed.message());
    assert!(quiet.is_none(), "the stream ended or spoke: {quiet:?}");
    assert!(
        rig.frontend.child.try_wait().expect("try_wait").is_none(),
        "the frontend exited"
    );

    // The client goes away and the frontend, with nothing left to wait for, exits 0.
    drop(executed);
    let status = rig.frontend.exited(PROMPT).await;
    assert!(status.success(), "kbf exited with {status}");
}

/// What a rolling update relies on (`docs/deploy/swarm-frontend.md`): an Execute open when the
/// stop signal comes is followed to its answer while the frontend drains, and the client gets
/// that answer on the same stream. The drain is past its load balancer wait (new executions
/// refused, every listener sent GOAWAY) before the job ends. The job is then lost by its host
/// three times: the executor submits the operation's second and third attempt during the drain,
/// and at the third ending it answers the client with a done operation, UNAVAILABLE, its last
/// attempt spent. Then the frontend, with nothing left to answer, exits 0 by itself while the
/// client still holds its connection. Catches a drain that keeps the stream open but no longer
/// delivers to it (a job's end is not read, an attempt is not submitted again, or the answer is
/// cut by the stand-down or the exit), and one that waits on the client to go away before it
/// exits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_answers_an_open_execute_whose_job_ends_during_the_drain() {
    let mut rig = Rig::start("answer-in-drain", &[]).await;
    let (mut executed, _) = rig.run_on_a_host("answered during the drain").await;
    let first = rig.job_submitted().await;

    rig.frontend.terminate();
    rig.not_ready().await;
    rig.draining().await;
    let drain = rig.drain().await;
    assert_eq!(drain["open_executions"], 1, "{drain}");

    let mut job = first;
    for attempt in 1..=3 {
        assert!(rig.fake.lose(&job), "attempt {attempt} lost");
        if attempt < 3 {
            job = rig.job_after(&job).await;
            assert!(
                rig.fake.place(&job, "host-1"),
                "attempt {} placed",
                attempt + 1
            );
            assert!(rig.fake.starting(&job), "attempt {} starting", attempt + 1);
            assert!(rig.fake.run(&job), "attempt {} running", attempt + 1);
        }
    }
    let last = loop {
        let op = next(&mut executed).await;
        if op.done {
            break op;
        }
    };
    let response = match &last.result {
        Some(operation::Result::Response(any)) => {
            ExecuteResponse::decode(any.value.as_slice()).expect("an ExecuteResponse")
        }
        other => panic!("a done operation without a response: {other:?}"),
    };
    let status = response.status.expect("the answer's status");
    assert_eq!(status.code, Code::Unavailable as i32, "{status:?}");
    assert_eq!(rig.fake.jobs().len(), 3, "one job per attempt");

    // The answer is sent and the stream ended; the client keeps its connection, and the
    // frontend does not wait on it to exit.
    let ended = tokio::time::timeout(PROMPT, executed.message())
        .await
        .expect("the stream ends after its answer");
    assert!(matches!(ended, Ok(None)), "{ended:?}");
    let exit = rig.frontend.exited(PROMPT).await;
    assert!(exit.success(), "kbf exited with {exit}");
    drop(executed);
}

/// Catches: `--progress-interval-secs 0` (and `--http2-keepalive-interval-secs 0`) ignored or
/// breaking the stream: with both off the frontend still starts, takes an action and runs it as
/// a job, and when it is stopped (its deadline is 1 s) the update after the one that says the
/// action is executing is the end of the stream, UNAVAILABLE, with no update that repeats the
/// last one between them (a progress update is the last operation sent again).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_turns_progress_off() {
    let mut rig = Rig::start(
        "progress-off",
        &[
            "--progress-interval-secs=0",
            "--http2-keepalive-interval-secs=0",
            "--shutdown-timeout-secs=1",
        ],
    )
    .await;
    let (mut executed, _) = rig.run_on_a_host("idle without progress").await;
    rig.frontend.terminate();
    let ended = tokio::time::timeout(PROMPT, executed.message())
        .await
        .expect("the stream ends after the signal");
    let status = ended.expect_err("the next thing on the stream is its end");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    drop(executed);
    let status = rig.frontend.exited(PROMPT).await;
    assert!(status.success(), "kbf exited with {status}");
}

/// Catches a second SIGTERM that is swallowed (the handler of the first is all there is):
/// systemd, an operator or the rollout that waited out its drain sends another to end the
/// drain, and with no shutdown timeout (the default) nothing else would. The first SIGTERM
/// drains and the stream is kept (the status names it: one stream open, none queued: the
/// operation runs as a job); the second ends the stream UNAVAILABLE and the process exits 0.
/// The defect it catches is the escalation not being spawned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_sigterm_ends_the_drain() {
    let mut rig = Rig::start("second-signal", &[]).await;
    let (mut executed, _) = rig.run_on_a_host("ended by a second signal").await;

    rig.frontend.terminate();
    rig.not_ready().await;
    let drain = rig.drain().await;
    assert_eq!(drain["draining"], true, "{drain}");
    assert_eq!(drain["open_executions"], 1, "{drain}");
    assert_eq!(drain["queued_operations"], 0, "{drain}");

    rig.frontend.terminate();
    let status = tokio::time::timeout(PROMPT, executed.message())
        .await
        .expect("the stream ends after the second signal")
        .expect_err("it ends with an error");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    assert!(status.message().contains("shutting down"), "{status:?}");
    drop(executed);
    let status = rig.frontend.exited(PROMPT).await;
    assert!(status.success(), "kbf exited with {status}");
}

/// Catches a drain endpoint that does not drain the process: `POST /v1/frontend/drain` takes it
/// out of `/readyz`, refuses a new Execute, and keeps the open stream; the process then exits 0
/// by itself when the stream is gone, with no signal (so the run writes its coverage). The
/// defect it catches is the endpoint answering 200 and doing nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_drain_endpoint_drains_the_process_and_it_exits_when_the_streams_are_done() {
    let mut rig = Rig::start("endpoint", &[]).await;
    let (executed, _) = rig.run_on_a_host("drained in the process").await;
    // The second action's inputs are uploaded before the drain: once the drain has sent the
    // connection GOAWAY, an upload on it fails too, and the test is about the Execute.
    let again = rig.action("a second action during the drain").await;

    let (code, body) = rig.api("POST", "/v1/frontend/drain").await;
    assert_eq!(code, 200, "{body}");
    rig.not_ready().await;
    rig.draining().await;
    // UNAVAILABLE from the service, or a transport error once the connection has been sent
    // GOAWAY, as the answer to the call or as the first message of its stream: either way the
    // call is not taken (the drain below still counts one open execution).
    if let Ok(mut stream) = rig.execute(&again).await {
        let first = tokio::time::timeout(PROMPT, stream.message())
            .await
            .expect("an answer in time");
        first.expect_err("a new Execute during the drain");
    }
    let drain = rig.drain().await;
    assert_eq!(drain["draining"], true, "{drain}");
    assert_eq!(drain["open_executions"], 1, "{drain}");
    assert!(
        rig.frontend.child.try_wait().expect("try_wait").is_none(),
        "the frontend exited with a stream open"
    );

    // The client goes away; the frontend has nothing left to wait for and exits 0.
    drop(executed);
    let status = rig.frontend.exited(PROMPT).await;
    assert!(status.success(), "kbf exited with {status}");
}

/// `now_or_never` without a new dependency: polls a future once.
mod once {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    pub fn now_or_never<F: Future>(future: F) -> Option<F::Output> {
        let mut future = pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(output) => Some(output),
            Poll::Pending => None,
        }
    }
}
