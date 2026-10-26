//! `kbf_execute_errors_total` by cause: an error status, a client that gave a call up,
//! and a stream dropped under a connection that was lost.

use kbf_auth::{DenyAuthorizer, Policy};
use kbf_front::metrics::{ErrorCause, ExecuteMetrics};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Endpoint;

use super::*;

/// The counts of the three causes, in the order status, client_abort, dropped.
fn causes(metrics: &ExecuteMetrics) -> [u64; 3] {
    [
        ErrorCause::Status,
        ErrorCause::ClientAbort,
        ErrorCause::Dropped,
    ]
    .map(|cause| metrics.errors.of(cause))
}

/// Waits until `n` Executes have ended (counted in `requests`): a stream its client left
/// is dropped by the server some time later. It yields to the runtime between looks and
/// gives up only at the hang deadline, never at a speed bound.
async fn ended(metrics: &ExecuteMetrics, n: u64) {
    tokio::time::timeout(HANG, async {
        while metrics.requests.get() < n {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Executes did not end");
    assert_eq!(metrics.requests.get(), n);
}

/// Asserts that the page has the series of `cause` at `value`.
fn series(farm: &Farm, cause: ErrorCause, value: u64) {
    let line = format!(
        "kbf_execute_errors_total{{cause=\"{}\"}} {value}",
        cause.label()
    );
    let text = farm.cache.metrics().render();
    assert!(text.lines().any(|l| l == line), "{line} not in:\n{text}");
}

/// A loopback relay to `to` for one connection; the handle ends it abruptly, as a
/// network that loses the connection does: both sockets close with no word to either end
/// about the streams on them.
async fn relay(
    to: std::net::SocketAddr,
) -> (std::net::SocketAddr, oneshot::Receiver<JoinHandle<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let at = listener.local_addr().expect("address");
    let (send, cut) = oneshot::channel();
    tokio::spawn(async move {
        let (mut client, _) = listener.accept().await.expect("accept");
        let mut server = TcpStream::connect(to).await.expect("connect");
        let copy = tokio::spawn(async move {
            let _ = copy_bidirectional(&mut client, &mut server).await;
        });
        let _ = send.send(copy);
    });
    (at, cut)
}

/// Asserts that a call that ends in an error status counts under `cause="status"` and
/// under no other cause: an input missing, the scheduler refusing, and a stream the
/// server ends (its operation abandoned).
///
/// Catches: a status counted as a client abort or a drop, and a status that bumps two
/// series. Mutant: classify by the connection alone (every non-ok end `dropped` or
/// `client_abort`): this goes red.
#[tokio::test]
async fn an_error_status_counts_as_status() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let metrics = &farm.cache.metrics().execute;

    let absent = Blob::new("an action nobody uploaded");
    start(&farm, &absent).await.expect_err("missing");
    assert_eq!(causes(metrics), [1, 0, 0]);

    let job = job("refused", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
    script.refuse.store(true, Ordering::SeqCst);
    start(&farm, &job.action).await.expect_err("refused");
    assert_eq!(causes(metrics), [2, 0, 0]);

    script.refuse.store(false, Ordering::SeqCst);
    let mut ops = start(&farm, &job.action).await.expect("Execute");
    next(&mut ops).await.expect("healthy").expect("QUEUED");
    script.abandon(0);
    next(&mut ops).await.expect_err("abandoned");
    assert_eq!(causes(metrics), [3, 0, 0]);
    ended(metrics, 3).await;
    series(&farm, ErrorCause::Status, 3);
    series(&farm, ErrorCause::ClientAbort, 0);
    series(&farm, ErrorCause::Dropped, 0);
}

/// Asserts that an Execute the authorizer refuses counts under `cause="status"` and no
/// other cause.
///
/// Catches: a denied call counted as a client abort, or as a drop when the connection is
/// gone. Mutant: remove the `inspect_err` that fails the guard in `execute`: this goes
/// red.
#[tokio::test]
async fn a_denied_execute_counts_as_status() {
    let mut policy = Policy::allow_all();
    policy.authorizers.execute = Arc::new(DenyAuthorizer);
    let farm = Farm::with_policy(Arc::new(Script::default()), policy).await;
    let metrics = &farm.cache.metrics().execute;
    let job = job("denied", &[], false);

    let denied = start(&farm, &job.action).await.expect_err("denied");
    assert_eq!(denied.code(), Code::PermissionDenied);
    ended(metrics, 1).await;
    assert_eq!(causes(metrics), [1, 0, 0]);
    series(&farm, ErrorCause::Status, 1);
}

/// Asserts that a client that drops its stream while its connection stays (it resets the
/// stream) counts under `cause="client_abort"` only.
///
/// Catches: an abort counted as a drop or an error. Mutant: swap the two causes in
/// `OpenExecute::cause`: this goes red.
#[tokio::test]
async fn a_client_that_gives_up_counts_as_a_client_abort() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let metrics = &farm.cache.metrics().execute;
    let job = job("given up", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;

    let mut ops = start(&farm, &job.action).await.expect("Execute");
    next(&mut ops).await.expect("healthy").expect("QUEUED");
    drop(ops);
    ended(metrics, 1).await;
    assert_eq!(causes(metrics), [0, 1, 0]);
    series(&farm, ErrorCause::ClientAbort, 1);
}

/// Asserts that a stream whose connection is lost under it, with no reset, counts under
/// `cause="dropped"` only.
///
/// Catches: a lost connection counted as the client's choice. Mutant: swap the two causes
/// in `OpenExecute::cause`, or never mark the connection gone: this goes red.
#[tokio::test]
async fn a_lost_connection_counts_as_dropped() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let metrics = &farm.cache.metrics().execute;
    let job = job("lost", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;

    let (through, cut) = relay(farm.addr).await;
    let channel = Endpoint::from_shared(format!("http://{through}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let mut exec = kbf_proto::reapi::execution_client::ExecutionClient::new(channel);
    let mut ops = exec
        .execute(ExecuteRequest {
            instance_name: "main".to_owned(),
            action_digest: Some(job.action.proto.clone()),
            ..Default::default()
        })
        .await
        .expect("Execute")
        .into_inner();
    next(&mut ops).await.expect("healthy").expect("QUEUED");
    cut.await.expect("the relay").abort();
    ended(metrics, 1).await;
    assert_eq!(causes(metrics), [0, 0, 1]);
    series(&farm, ErrorCause::Dropped, 1);
}

/// Asserts that an Execute answered by a run or by the action cache counts under no
/// cause, that every cause is rendered at 0, and that causes and answers add up to the
/// requests.
///
/// Catches: `ok` set too early or late; a cause series missing until it is first
/// bumped. Mutant: set `ok` when the stream is made: the abort and status tests go red;
/// render a cause only when non-zero: this goes red.
#[tokio::test]
async fn an_answered_execute_counts_under_no_cause() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let metrics = &farm.cache.metrics().execute;
    let job = job("answered", &[], false);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
    for cause in [
        ErrorCause::Status,
        ErrorCause::ClientAbort,
        ErrorCause::Dropped,
    ] {
        series(&farm, cause, 0);
    }

    let mut ops = start(&farm, &job.action).await.expect("Execute");
    script.set(0, Stage::Done(Finished::Ran(Box::default())));
    next(&mut ops).await.expect("healthy").expect("an update");
    while next(&mut ops).await.expect("healthy").is_some() {}
    drop(ops);
    ended(metrics, 1).await;

    let hit = self::job("cached", &[], false);
    farm.upload(&hit.blobs.iter().collect::<Vec<_>>()).await;
    farm.cache
        .write_action_result(Role::Daemon, hit.action.digest, &ActionResult::default())
        .await
        .expect("write");
    let mut ops = start(&farm, &hit.action).await.expect("Execute");
    next(&mut ops).await.expect("healthy").expect("done");
    drop(ops);
    ended(metrics, 2).await;
    assert_eq!(causes(metrics), [0, 0, 0]);
    assert_eq!(metrics.errors.get(), 0);
}
