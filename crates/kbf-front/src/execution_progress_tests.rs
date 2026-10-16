//! The progress tick on an open stream, on a paused clock: no socket is involved, so
//! the clock moves only when every task is waiting on a timer.

use std::time::Duration;

use futures::StreamExt as _;
use kbf_proto::reapi::ExecuteOperationMetadata;
use kbf_proto::reapi::execution_stage::Value as ExecStage;
use kbf_segments::sha256;
use prost::Message as _;
use tokio::sync::watch;

use super::{Progress, Stage, Ticket, closing, operations, progress};
use crate::metrics::CacheMetrics;

const EVERY: Duration = Duration::from_secs(30);

/// A ticket whose stage the test holds; the sender must outlive the stream.
fn ticket(stage: Stage) -> (watch::Sender<Stage>, Ticket) {
    let (tx, rx) = watch::channel(stage);
    let ticket = Ticket {
        name: "operations/0".to_owned(),
        instance: "main".to_owned(),
        action: sha256(b"action"),
        stage: rx,
    };
    (tx, ticket)
}

/// Catches (komira-build-farm#791): an open stream that sends nothing while its action
/// runs, so a load balancer's idle timeout cuts it and Buck2 (which never reconnects)
/// fails the action. An action that runs for ten intervals gets ten progress
/// operations after the first one, each the same operation, not done, at its current
/// stage. Mutant: no ticker (`Progress::off`, or a stream that does not select the
/// tick): the read times out on the paused clock and the test fails.
#[tokio::test(start_paused = true)]
async fn an_idle_stream_gets_a_progress_operation_every_tick() {
    let (_stage, ticket) = ticket(Stage::Executing);
    let (_closer, closing) = closing();
    let (ticker, progress) = progress(EVERY);
    let ticking = tokio::spawn(ticker.run());
    let metrics = CacheMetrics::default();
    let mut ops = operations(ticket, closing, progress, metrics.execute.wait());

    let first = ops.next().await.expect("an operation").expect("healthy");
    assert!(!first.done);
    for _ in 1..=10 {
        let update = tokio::time::timeout(10 * EVERY, ops.next())
            .await
            .expect("a progress operation (is the ticker running?)")
            .expect("the stream is open")
            .expect("healthy");
        assert_eq!(update.name, "operations/0");
        assert!(!update.done, "{update:?}");
        let meta = update.metadata.as_ref().expect("metadata");
        let meta = ExecuteOperationMetadata::decode(meta.value.as_slice()).expect("decodes");
        assert_eq!(meta.stage, ExecStage::Executing as i32);
    }
    ticking.abort();
}

/// Catches a stream that ignores its own stage change while ticks are also arriving:
/// the done operation is sent as the stream's last, and ends it.
#[tokio::test(start_paused = true)]
async fn a_progress_stream_still_ends_with_the_done_operation() {
    let (stage, ticket) = ticket(Stage::Queued);
    let (_closer, closing) = closing();
    let (ticker, progress) = progress(EVERY);
    let ticking = tokio::spawn(ticker.run());
    let metrics = CacheMetrics::default();
    let mut ops = operations(ticket, closing, progress, metrics.execute.wait());
    ops.next().await.expect("queued").expect("healthy");
    ops.next()
        .await
        .expect("a progress operation")
        .expect("healthy");
    stage.send_replace(Stage::Done(super::Finished::Ran(Box::default())));
    let done = ops.next().await.expect("done").expect("healthy");
    assert!(done.done, "{done:?}");
    assert!(ops.next().await.is_none());
    ticking.abort();
}

/// Without a ticker a stream sends only its stage changes.
#[tokio::test(start_paused = true)]
async fn a_stream_without_progress_sends_only_stage_changes() {
    let (stage, ticket) = ticket(Stage::Queued);
    let (_closer, closing) = closing();
    let metrics = CacheMetrics::default();
    let mut ops = operations(ticket, closing, Progress::off(), metrics.execute.wait());
    ops.next().await.expect("queued").expect("healthy");
    let quiet = tokio::time::timeout(100 * EVERY, ops.next()).await;
    assert!(quiet.is_err(), "an update with no change and no ticker");
    stage.send_replace(Stage::Executing);
    ops.next().await.expect("executing").expect("healthy");
}

/// Catches a drain that undoes a close: `drain` after `close` (a second signal, or a
/// deadline that ran out before the drain was noticed) must leave the streams closed,
/// not reopen them to wait for their action again. Draining twice changes nothing.
#[tokio::test(start_paused = true)]
async fn a_drain_after_a_close_leaves_it_closed() {
    let (closer, mut closing) = closing();
    assert!(!closing.draining());
    closer.drain();
    closer.drain();
    assert!(closing.draining());
    closer.close();
    closer.drain();
    assert!(closing.draining());
    tokio::time::timeout(Duration::from_secs(1), closing.closed())
        .await
        .expect("still closed after a drain");
}
