//! Frontends that share one state store: operations are named without clashes, a client that
//! waits through another frontend gets the result, a finished operation is answered by name by
//! any frontend for as long as the store keeps it, and a frontend that comes after one that is
//! gone takes up its unfinished operations.
//!
//! Two frontends here are two servers over one [`FakeState`] and one job service
//! ([`Cell::restart`] leaves the old server running beside the new one). The store's clock is
//! moved by hand.

use std::time::Duration;

use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::WaitExecutionRequest;
use kbf_server::executor::LIVE;
use kbf_server::executor::rows::DONE_FLOOR;
use tonic::{Code, Streaming};

use super::support::{Cell, Client, FakeState, Job, Ran, StateHandle, done, response};

/// The name of the first operation `ops` streams.
async fn first_name(ops: &mut Streaming<Operation>) -> String {
    ops.message()
        .await
        .expect("a healthy stream")
        .expect("an update")
        .name
}

/// WaitExecution on `name`: the stream of its operation, or the error code.
async fn wait(client: &Client, name: &str) -> Result<Streaming<Operation>, Code> {
    client
        .exec()
        .wait_execution(WaitExecutionRequest {
            name: name.to_owned(),
        })
        .await
        .map(tonic::Response::into_inner)
        .map_err(|status| status.code())
}

/// Asserts that two frontends over one store never give one operation name twice: each takes
/// its ids from a block it reserved in the store, so the names of the actions the two are
/// running at once differ.
#[tokio::test]
async fn two_frontends_over_one_store_never_give_one_operation_name_twice() {
    let state = FakeState::new();
    let a = Cell::start_with_state(state.handle()).await;
    let b = a.restart().await;
    let (x, y) = (Job::new("/bin/x", &[]), Job::new("/bin/y", &[]));
    a.upload(&x.blobs()).await;
    a.upload(&y.blobs()).await;

    let mut x_ops = a.execute(&x.action).await;
    let mut y_ops = b.execute(&y.action).await;
    let (x_name, y_name) = (first_name(&mut x_ops).await, first_name(&mut y_ops).await);

    assert_ne!(x_name, y_name, "one name for two operations");
    // An operation's row is written after its first update is streamed and before its job is
    // submitted: both jobs submitted means both rows are in the store.
    a.direct.job(&x.action, 0).await;
    a.direct.job(&y.action, 0).await;
    assert_eq!(state.keys("exec/op/").len(), 2);
}

/// Asserts that a client that submits through one frontend and waits through another gets the
/// result: the client, cut from the first frontend, asks the second for the operation by its
/// name, is shown the operation, and gets the answer the first frontend writes to the store when
/// the job ends. The first frontend's own caller gets it too.
#[tokio::test]
async fn a_client_waiting_through_another_frontend_gets_the_result() {
    let state = FakeState::new();
    let a = Cell::start_with_state(state.handle()).await;
    let b = a.restart().await;
    let job = Job::new("/bin/run", &[]);
    a.upload(&job.blobs()).await;
    a.direct
        .will_run(&job.action, Ran::writes("out", "through b"));
    let mut own = a.execute(&job.action).await;
    let name = first_name(&mut own).await;
    let id = a.direct.run(&job.action).await;

    let mut waiting = wait(&b, &name).await.expect("the operation is known to b");
    assert!(not_done_yet(&mut waiting).await);
    assert!(a.direct.fake.finish(&id, 0));

    let through_b = response(&done(&mut waiting).await)
        .result
        .expect("a result");
    let through_a = response(&done(&mut own).await).result.expect("a result");
    assert_eq!(through_b.output_files[0].path, "out");
    assert_eq!(through_b, through_a);
    assert_eq!(a.direct.jobs_of(&job.action).len(), 1, "one job ran");
}

/// Whether a stream shows no done operation yet: its next update, if any, is not done.
async fn not_done_yet(ops: &mut Streaming<Operation>) -> bool {
    match tokio::time::timeout(super::support::QUIET, ops.message()).await {
        Err(_) => true,
        Ok(update) => !update.expect("a healthy stream").expect("an update").done,
    }
}

/// Asserts that a finished operation is answered by name by a frontend that never held it, for
/// as long as the store keeps the answer, and not after: the `done/` and `doneof/` rows live at
/// least [`DONE_FLOOR`] on the store's clock; past it the name is NOT_FOUND.
#[tokio::test]
async fn a_finished_operation_is_answered_by_name_by_another_frontend_until_the_store_forgets_it() {
    let state = FakeState::new();
    let a = Cell::start_with_state(state.handle()).await;
    let b = a.restart().await;
    let job = Job::new("/bin/run", &[]);
    a.upload(&job.blobs()).await;
    a.direct
        .will_run(&job.action, Ran::writes("out", "kept for b"));
    let mut own = a.execute(&job.action).await;
    let name = first_name(&mut own).await;
    a.direct.complete(&job.action).await;
    let sent = response(&done(&mut own).await).result.expect("a result");

    let mut again = wait(&b, &name).await.expect("b answers by name");
    let asked = response(&done(&mut again).await).result.expect("a result");
    assert_eq!(asked, sent);

    state.advance(DONE_FLOOR + Duration::from_secs(1));
    assert_eq!(wait(&b, &name).await.err(), Some(Code::NotFound));
}

/// Asserts that a frontend started after another is gone takes up the operation the first left
/// unfinished, and a client that reattaches by name gets the result. The row names the put
/// directory the job commits into, so the new frontend reads the job the first one submitted
/// and, when it ends, its pack from that directory, and answers the client: one job, no second
/// attempt. The old frontend's liveness row has expired on the store's clock, and its operation
/// row names it as the owner.
#[tokio::test]
async fn a_restarted_frontend_takes_up_the_unfinished_operation_and_the_client_reattaches() {
    let state = FakeState::new();
    let before = Cell::start_with_state(state.handle()).await;
    let job = Job::new("/bin/run", &[]);
    before.upload(&job.blobs()).await;
    before
        .direct
        .will_run(&job.action, Ran::writes("out", "after the restart"));
    let mut ops = before.execute(&job.action).await;
    let name = first_name(&mut ops).await;
    let first = before.direct.run(&job.action).await;
    assert_eq!(state.keys("exec/op/").len(), 1);

    state.advance(LIVE + Duration::from_secs(1));
    let after = before.cold_restart().await;
    drop(ops);
    let mut reattached = wait(&after, &name)
        .await
        .expect("the new frontend knows it");
    assert!(after.direct.fake.finish(&first, 0));

    let result = response(&done(&mut reattached).await)
        .result
        .expect("a result");
    assert_eq!(result.output_files[0].path, "out");
    assert_eq!(after.direct.jobs_of(&job.action).len(), 1, "no second job");
    assert!(state.keys("exec/op/").is_empty(), "the operation finished");
    assert_eq!(state.keys("done/").len(), 1);
}

/// The families of rows an earlier release's scheduler wrote that this release does not read.
const RETIRED: [&str; 10] = [
    "op/",
    "queue/",
    "lease/",
    "loss/",
    "fate/",
    "cordon/",
    "session/",
    "meta/epoch",
    "meta/generation",
    "meta/next_seq",
];

/// Every row of the retired families, with its version.
fn retired_rows(store: &dyn kbf_state::Store) -> Vec<(String, Vec<u8>, u64)> {
    RETIRED
        .iter()
        .flat_map(|prefix| store.scan_prefix(prefix).expect("a scan"))
        .map(|(key, row)| (key, row.value, row.version.0))
        .collect()
}

/// Asserts that a server of this release over the store an earlier release wrote (the shipped
/// `tests/golden/state/v1.db`, copied) opens it and ignores the scheduler's rows: the operation
/// the old release finished is answered by name with the answer it stored (its `done/` row, and
/// the `doneof/` row of the action beside it, as the release wrote them), the operations it left
/// queued or leased are not known (nothing runs them, and nothing answers for them), the ids this
/// release gives continue from the counter the old release left, and no row of the retired
/// families is changed or removed.
///
/// This is the cross-version test of rule B8 for the state store: the previous release's data,
/// read by this release.
#[tokio::test]
async fn the_store_an_earlier_release_wrote_is_read_by_name_and_its_scheduler_rows_are_ignored() {
    use kbf_state::sqlite::{Options, SqliteStore};
    use kbf_state::{Cond, Store as _, Txn};

    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/state/v1.db");
    let scratch = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-state-v1");
    let copy = scratch.join("state.db");
    std::fs::copy(&golden, &copy).expect("a copy of the golden");
    let store = std::sync::Arc::new(
        SqliteStore::open(
            &copy,
            std::sync::Arc::new(kbf_state::ManualClock::default()),
            Options::default(),
        )
        .expect("the golden opens"),
    );
    // The golden predates the `doneof/` row; an earlier release's is a version byte, the
    // action's size, its hash and the instance name.
    let mut doneof = vec![1_u8];
    doneof.extend(2_u64.to_be_bytes());
    doneof.extend([2_u8; 32]);
    doneof.extend(b"main");
    store
        .txn(Txn::new().put_ttl(
            "doneof/0000000000000001",
            doneof,
            Duration::from_secs(600),
            Cond::Absent,
        ))
        .expect("a write")
        .expect("no conflict");
    let old = retired_rows(&*store);
    assert!(old.len() > 8, "the golden holds the scheduler's rows");
    let next_op = store
        .get("meta/next_op")
        .expect("a read")
        .expect("a counter")
        .value;
    let next_op = u64::from_be_bytes(next_op.as_slice().try_into().expect("eight bytes"));

    let handle = std::sync::Arc::new(StateHandle {
        store: store.clone(),
    });
    let cell = Cell::start_with_state(handle).await;

    let mut finished = wait(&cell, "operations/1").await.expect("answered by name");
    let answer = response(&done(&mut finished).await);
    let status = answer.status.expect("a status");
    assert_eq!(status.code, Code::InvalidArgument as i32);
    assert_eq!(status.message, "golden");
    for unknown in ["operations/0", "operations/2"] {
        assert_eq!(
            wait(&cell, unknown).await.err(),
            Some(Code::NotFound),
            "{unknown}"
        );
    }

    let job = Job::new("after the upgrade", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let name = first_name(&mut ops).await;
    let number: u64 = name
        .strip_prefix("operations/")
        .and_then(|n| n.parse().ok())
        .expect("a stored name");
    assert!(
        number >= next_op,
        "{name} may be an id the old release gave ({next_op})"
    );
    cell.direct.complete(&job.action).await;
    done(&mut ops).await;

    assert_eq!(
        retired_rows(&*store),
        old,
        "rows of the retired families changed"
    );
}
