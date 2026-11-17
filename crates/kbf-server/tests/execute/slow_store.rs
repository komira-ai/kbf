//! A slow object store, end to end in one process: reading a job's pack writes its blobs to the
//! store, and on a store whose writes take many seconds that must hold up only the operation
//! whose pack is being taken in. Another operation's job goes on to its answer.
//!
//! A module of the `execute` test binary, not a binary of its own.

use crate::support::{Cell, Job, Ran, done, response};

/// Asserts that an operation whose pack waits on the store does not hold up another: with the
/// store's next write held, the first operation's job ends and its pack is read (the write
/// blocks); a second operation then runs to its answer; and once the write goes through the
/// first is answered too.
#[tokio::test]
async fn an_operation_waiting_on_a_slow_store_does_not_hold_up_another() {
    let cell = Cell::start().await;
    let slow = Job::new("behind a slow store", &[]);
    let quick = Job::new("beside it", &[]);
    cell.upload(&slow.blobs()).await;
    cell.upload(&quick.blobs()).await;
    cell.direct
        .will_run(&slow.action, Ran::writes("out", "written slowly"));
    cell.direct.will_run(&quick.action, Ran::exits(0));
    let mut slow_ops = cell.execute(&slow.action).await;
    let mut quick_ops = cell.execute(&quick.action).await;

    let put = &cell.store.put;
    put.close_next();
    cell.direct.complete(&slow.action).await;
    put.held().await;
    cell.direct.complete(&quick.action).await;
    let quick_answer = response(&done(&mut quick_ops).await);
    assert_eq!(quick_answer.status.map(|s| s.code), Some(0));

    put.open();
    let result = response(&done(&mut slow_ops).await)
        .result
        .expect("a result");
    assert_eq!(result.output_files[0].path, "out");
}
