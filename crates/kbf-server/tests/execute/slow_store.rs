//! A slow object store, end to end in one process: settling a daemon's `Result`
//! writes its action-cache record to the store, and on a store whose writes take many
//! seconds that must not hold up the daemon's heartbeats. A heartbeat the server
//! leaves unacknowledged for the fence time makes the daemon fence its leases.
//!
//! A module of the `execute` test binary, not a binary of its own: `Farm` is generic,
//! and coverage counts each generic function by its best-covered instantiation.

use kbf_proto::worker::daemon_message;
use kbf_proto::worker::server_message::Message;

use crate::support::{Cell, Job, done, output, ran, response};

/// Catches: the server settling a `Result` on the task that reads the worker
/// stream, so that every heartbeat behind it waits for the store's write. With the
/// write held, ten heartbeats in a row must each be acknowledged within the quiet
/// time, the lease they list must stay held (no `Cancel`), and once the write goes
/// through the `Result` is accepted and the caller answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeats_are_acknowledged_while_a_result_waits_on_the_store() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("behind a slow store", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let lease = start.lease_id.expect("a lease");
    let result = output(&cell, "written slowly", 0).await;

    let put = &cell.store.put;
    put.close_next();
    daemon.send(daemon_message::Message::Result(ran(
        start.lease_id,
        &result,
    )));
    put.held().await;
    for n in 0..10 {
        assert!(
            daemon.heartbeat(&[lease]).await,
            "heartbeat {n} waited for the store's write"
        );
    }
    assert_eq!(daemon.cancelled().await, None, "the lease was given up");
    put.open();
    let ack = daemon
        .expect("ResultAck", |m| match m {
            Message::ResultAck(a) => Some(*a),
            _ => None,
        })
        .await;
    assert!(ack.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));
}
