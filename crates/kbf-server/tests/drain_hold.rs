//! A hold on the plain drain (`Drain::hold`): what `kbf all-in-one` takes so that its
//! daemon half's leases keep reaching their results through the server half until the
//! daemon half has drained. The drain's own behaviour is in `frontend_drain.rs` and
//! `shutdown.rs`.

mod support;

use std::time::Duration;

use futures::FutureExt as _;
use kbf_proto::reapi::execution_stage::Value as ExecStage;
use support::{Cell, Job, PROMPT, done, output, ran, stage};

/// An execution running on a node's lease, the node's daemon, and the lease's result.
async fn running(
    cell: &Cell,
    text: &str,
) -> (
    support::FakeDaemon,
    kbf_proto::worker::Result,
    tonic::Streaming<kbf_proto::google::longrunning::Operation>,
) {
    let job = Job::new(text, &[]);
    cell.upload(&job.blobs()).await;
    // Everything the daemon will report is uploaded now: the drain closes the REAPI
    // listener to new connections.
    let result = output(cell, "the build output", 0).await;
    let mut ops = cell.execute(&job.action).await;
    ops.message().await.expect("healthy").expect("QUEUED");
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let start = daemon.start().await;
    loop {
        let op = ops.message().await.expect("healthy").expect("EXECUTING");
        if stage(&op) == ExecStage::Executing as i32 {
            break;
        }
    }
    (daemon, ran(start.lease_id, &result), ops)
}

/// Whether the worker listener still takes connections.
async fn accepts(cell: &Cell) -> bool {
    tokio::net::TcpStream::connect(cell.worker_addr)
        .await
        .is_ok()
}

/// Catches a server that stands down while a daemon beside it has not drained, which cuts
/// the leases that daemon still runs (their Results and acknowledgements need the worker
/// listener): with a hold, a drain that began with an execution running lets the
/// execution finish (its Result is accepted and the stream is answered), settles once the
/// REAPI listener is done, and then goes on serving the worker listener: the daemon's
/// session is still acknowledged. Only when the hold is dropped does the server stop, and
/// the daemon's session ends. Catches: the serving future returning as soon as the REAPI
/// listener is done (the wait on the hold removed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hold_keeps_the_worker_listener_up_after_the_executions_are_done() {
    let cell = Cell::start().await;
    let (mut daemon, result, mut ops) = running(&cell, "held").await;
    let hold = cell.drain.hold();
    assert!(cell.drain.is_held());

    assert!(!cell.drain.is_unready());
    cell.drain.begin();
    assert!(cell.drain.is_unready(), "a drain left /readyz ready");
    assert!(
        cell.drain.settled().now_or_never().is_none(),
        "settled with an execution open"
    );
    let ack = daemon.report(result).await;
    assert!(ack.accepted, "{ack:?}");
    let last = done(&mut ops).await;
    assert_eq!(stage(&last), ExecStage::Completed as i32);
    drop(ops);

    tokio::time::timeout(PROMPT, cell.drain.settled())
        .await
        .expect("the drain settles once the execution is done");
    // The REAPI side is done; the held server answers the daemon still.
    daemon.beat_acked(&[], None).await;
    assert!(
        accepts(&cell).await,
        "the worker listener stopped under a hold"
    );

    drop(hold);
    daemon.closed().await;
    let deadline = tokio::time::Instant::now() + PROMPT;
    while accepts(&cell).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the server went on serving after the hold was dropped"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Catches a hold that outlives the operator's hard stop: a close (a second signal)
/// stops the server whatever is held, and the hold is still alive when it does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_close_stops_the_server_whatever_is_held() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let _hold = cell.drain.hold();

    cell.drain.begin();
    tokio::time::timeout(PROMPT, cell.drain.settled())
        .await
        .expect("an idle drain settles");
    daemon.beat_acked(&[], None).await;
    cell.drain.close();
    daemon.closed().await;
    assert!(cell.drain.is_held(), "the close dropped the hold");
}

/// Catches a server that stops without a hold waiting for nothing: with no hold, a drain
/// of an idle server settles and the server stops, as it did before holds existed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_hold_an_idle_drain_stops_the_server() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    assert!(!cell.drain.is_held());

    cell.drain.begin();
    daemon.closed().await;
}
