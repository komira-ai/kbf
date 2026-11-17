//! Operation names and results across a server restart (issues #137 and #154).
//!
//! A server with no state store keeps its operations in its process. A client can still hold an
//! operation name of the process before a restart and call `WaitExecution` with it on the next
//! one; the job service can still run a job of the process before a restart, whose process is
//! still answering its callers. If the new process gave the same name to another operation, the
//! client would be handed that operation's result as its own.
//!
//! With the Raft metadata log ([`Cell::start_raft`], restarted with [`Cell::cold_restart`]) the
//! log and the action cache are kept across the restart; the names are not.

mod support;

use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::{ExecuteOperationMetadata, WaitExecutionRequest};
use prost::Message;
use support::{Cell, Client, Job, Ran, done, done_within_quiet, response, unstamped};
use tonic::{Code, Streaming};

/// The name of the first operation `ops` streams.
async fn first_name(ops: &mut Streaming<Operation>) -> String {
    ops.message()
        .await
        .expect("a healthy stream")
        .expect("an update")
        .name
}

/// WaitExecution on `name`: the first operation it streams, or the error code.
async fn wait_first(client: &Client, name: &str) -> Result<Operation, Code> {
    let mut ops = client
        .exec()
        .wait_execution(WaitExecutionRequest {
            name: name.to_owned(),
        })
        .await
        .map_err(|s| s.code())?
        .into_inner();
    Ok(ops
        .message()
        .await
        .expect("a healthy stream")
        .expect("an update"))
}

/// Asserts WaitExecution on `name` is NOT_FOUND on `after`, the restarted server, and
/// says which operation and action it attached to if it is not. `x` and `y` are the
/// action before the restart and the one running after it.
async fn assert_unknown_after_restart(after: &Client, name: &str, x: &Job, y: &Job) {
    match wait_first(after, name).await {
        Err(code) => assert_eq!(code, Code::NotFound, "WaitExecution({name:?})"),
        Ok(op) => {
            let action = op.metadata.as_ref().and_then(|any| {
                ExecuteOperationMetadata::decode(any.value.as_slice())
                    .ok()?
                    .action_digest
            });
            panic!(
                "WaitExecution({name:?}) on the restarted server attached to operation \
                 {:?} of action {action:?}; X is {:?}, Y is {:?}",
                op.name, x.action.proto, y.action.proto
            );
        }
    }
}

/// Executes `y` on `after` and runs it on the host, so it stays unfinished. Returns its operation
/// name; the stream is returned to keep it open.
async fn run_y(after: &Cell, y: &Job) -> (String, Streaming<Operation>) {
    after.upload(&y.blobs()).await;
    let mut y_ops = after.execute(&y.action).await;
    let y_name = first_name(&mut y_ops).await;
    after.direct.run(&y.action).await;
    (y_name, y_ops)
}

/// Catches issue #137: a restarted server whose predecessor's job ending answers the new
/// process's operation. The old process (left running beside the new one) has X running; the new
/// process runs Y; X's job ends and the old process answers X's caller and caches X's result. Y's
/// caller must not be answered by it and Y must stay out of the action cache until Y's own job
/// ends, whose result then answers Y.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_server_does_not_take_its_predecessor_s_result_for_its_own() {
    let before = Cell::start().await;
    let x = Job::new("x, before the restart", &[]);
    before.upload(&x.blobs()).await;
    before
        .direct
        .will_run(&x.action, Ran::writes("x.out", "from the old process"));
    let mut x_ops = before.execute(&x.action).await;
    let x_job = before.direct.run(&x.action).await;

    let after = before.restart().await;
    let y = Job::new("y, after the restart", &[]);
    after
        .direct
        .will_run(&y.action, Ran::writes("y.out", "from the new process"));
    let (_, mut y_ops) = run_y(&after, &y).await;

    assert!(before.direct.fake.finish(&x_job, 0));
    let x_result = response(&done(&mut x_ops).await)
        .result
        .expect("X's result");
    assert_eq!(x_result.output_files[0].path, "x.out");
    assert!(
        !done_within_quiet(&mut y_ops).await,
        "the old process's result answered the new action"
    );
    assert_eq!(after.cached(&y.action).await, Err(Code::NotFound));
    assert_eq!(
        after.cached(&x.action).await.map(unstamped),
        Ok(unstamped(x_result))
    );

    let y_job = after.direct.job(&y.action, 0).await;
    assert!(after.direct.fake.finish(&y_job, 0));
    let y_result = response(&done(&mut y_ops).await)
        .result
        .expect("Y's result");
    assert_eq!(y_result.output_files[0].path, "y.out");
}

/// Catches issue #154: operation names that restart at `operations/0` in every server
/// process. A client holds the name of action X's operation from the process before a
/// restart; the new process is running action Y. WaitExecution with X's name must answer
/// NOT_FOUND, never attach the client to Y's operation (it would be handed Y's result as
/// X's). Also catches a fix that refuses every name (Y's own name still finds Y's operation,
/// and X's name found X before the restart).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_server_does_not_answer_its_predecessor_s_operation_names() {
    let before = Cell::start().await;
    let x = Job::new("x, before the restart", &[]);
    before.upload(&x.blobs()).await;
    let mut x_ops = before.execute(&x.action).await;
    let x_name = first_name(&mut x_ops).await;
    before.direct.run(&x.action).await;
    let found = wait_first(&before, &x_name).await.map(|op| op.name);
    assert_eq!(found, Ok(x_name.clone()), "X's name on its own process");

    let after = before.restart().await;
    let y = Job::new("y, after the restart", &[]);
    let (y_name, _y_ops) = run_y(&after, &y).await;

    assert_unknown_after_restart(&after, &x_name, &x, &y).await;
    assert_ne!(
        x_name, y_name,
        "a restarted server reused an operation name"
    );
    let waited = wait_first(&after, &y_name).await.map(|op| op.name);
    assert_eq!(waited, Ok(y_name), "Y's own name");
}

/// Catches issue #154 for an operation that finished before the restart (its waiter
/// is gone from the old process, as most names a client still holds are: the old
/// process here keeps no finished operation): X's name is NOT_FOUND on the old process
/// once X is done, and stays NOT_FOUND on the new one while Y runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_operation_s_name_stays_unknown_after_a_restart() {
    let before = Cell::start_with_retention(std::time::Duration::ZERO).await;
    let x = Job::new("x, finished before the restart", &[]);
    before.upload(&x.blobs()).await;
    let mut x_ops = before.execute(&x.action).await;
    let x_name = first_name(&mut x_ops).await;
    before.direct.complete(&x.action).await;
    assert!(response(&done(&mut x_ops).await).result.is_some());
    let found = wait_first(&before, &x_name).await.map(|op| op.name);
    assert_eq!(found, Err(Code::NotFound), "X's name once X is done");

    let after = before.restart().await;
    let y = Job::new("y, after the restart (X finished)", &[]);
    let (y_name, _y_ops) = run_y(&after, &y).await;

    assert_unknown_after_restart(&after, &x_name, &x, &y).await;
    let waited = wait_first(&after, &y_name).await.map(|op| op.name);
    assert_eq!(waited, Ok(y_name), "Y's own name");
}

/// Catches issue #154 under the Raft log: the log and the action cache are kept across a
/// restart over one raft directory, the operation names are not. A client holds X's name from
/// before the restart; the new process runs Y. WaitExecution with X's name must answer
/// NOT_FOUND, and Y's own name must find Y.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_over_the_raft_log_does_not_answer_the_old_operation_names() {
    let before = Cell::start_raft().await;
    let x = Job::new("x, before the raft restart (names)", &[]);
    before.upload(&x.blobs()).await;
    let mut x_ops = before.execute(&x.action).await;
    let x_name = first_name(&mut x_ops).await;
    before.direct.run(&x.action).await;

    let after = before.cold_restart().await;
    drop(x_ops);
    let y = Job::new("y, after the raft restart (names)", &[]);
    let (y_name, _y_ops) = run_y(&after, &y).await;

    assert_unknown_after_restart(&after, &x_name, &x, &y).await;
    let waited = wait_first(&after, &y_name).await.map(|op| op.name);
    assert_eq!(waited, Ok(y_name), "Y's own name");
}
