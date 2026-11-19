//! A frontend updated in the middle of a build, over a real Postgres state store (the server
//! `KBF_TEST_POSTGRES_URL` names; these tests fail without it): two frontends serve together on
//! one database and one job service, as the old and the new task of an update do, each with a
//! handle of its own on the database.
//!
//! - The old one is stopped in the middle of an Execute, as a stop that cannot wait does: its
//!   stream ends and the process goes. The client asks the new one for the operation by name
//!   (`WaitExecution`), and the new one, which takes the operation up from the database, answers
//!   it with the result of the job that was already running: one job, no rebuild.
//! - While the old one runs an action, an Execute of the same action on the new one follows the
//!   old one's operation instead of submitting a job of its own.
//!
//! Everything asserted is counted (jobs at the job service, `SubmitJob` calls, packs read, rows
//! in the database); every wait is for an event, bounded only against a hang.

use std::sync::Arc;
use std::time::Duration;

use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::WaitExecutionRequest;
use kbf_state::Store;
use kbf_state::postgres::{PostgresConfig, PostgresStore, TestDatabase, test_database};
use tonic::{Code, Streaming};

use tokio::time::timeout;

use super::support::{Cell, Client, Direct, Job, PROMPT, Ran, StateHandle, done, response};

/// The new frontend's liveness (`--executor-live-secs`): it looks for the operations of gone
/// frontends every half of it. The old one deletes its own liveness row when it stops, so its
/// operation is taken up at the new one's next look, well within a wait of the support's.
const LIVE: Duration = Duration::from_secs(2);

/// Runs `f` on a thread in no runtime: the test server's client makes and drops a database on a
/// runtime of its own, which tokio refuses on a thread that runs tasks.
fn off_runtime<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| match scope.spawn(f).join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    })
}

/// A database of the test's own with the state schema, dropped when the test ends.
struct Database(Option<TestDatabase>);

impl Database {
    fn new() -> Self {
        Self(Some(off_runtime(test_database)))
    }

    /// A handle of its own on the database, as each frontend process opens one.
    fn handle(&self) -> Arc<StateHandle> {
        let url = self.0.as_ref().expect("held until dropped").url();
        let store = off_runtime(move || {
            let config = PostgresConfig::parse(&url).expect("a config");
            PostgresStore::open(&config).expect("a connection")
        });
        Arc::new(StateHandle {
            store: Arc::new(store),
        })
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if let Some(database) = self.0.take() {
            off_runtime(move || drop(database));
        }
    }
}

/// The keys under `prefix`, read with `handle`.
fn keys(handle: &StateHandle, prefix: &str) -> Vec<String> {
    let store = Arc::clone(&handle.store);
    let prefix = prefix.to_owned();
    let rows = off_runtime(move || store.scan_prefix(&prefix).expect("a scan"));
    rows.into_iter().map(|(key, _)| key).collect()
}

/// The name of the first operation `ops` streams.
async fn first_name(ops: &mut Streaming<Operation>) -> String {
    timeout(PROMPT, ops.message())
        .await
        .expect("an update in time")
        .expect("a healthy stream")
        .expect("an update")
        .name
}

/// The status a stream ends with.
async fn ended(ops: &mut Streaming<Operation>) -> tonic::Status {
    let end = async {
        loop {
            match ops.message().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("the stream ended with no status"),
                Err(status) => return status,
            }
        }
    };
    timeout(PROMPT, end).await.expect("the stream ends in time")
}

/// Waits, bounded, until the executors of `direct` have followed `count` operations of other
/// frontends (`kbf_executor_operations_joined_total`).
async fn joined(direct: &Direct, count: u64) {
    let line = format!("kbf_executor_operations_joined_total {count}");
    let seen = async {
        loop {
            let mut page = String::new();
            direct.metrics.render(&mut page);
            if page.lines().any(|l| l == line) {
                return;
            }
            tokio::task::yield_now().await;
        }
    };
    timeout(PROMPT, seen)
        .await
        .expect("the operation is joined in time");
}

/// WaitExecution on `name`.
async fn wait(client: &Client, name: &str) -> Streaming<Operation> {
    let mut exec = client.exec();
    let asked = exec.wait_execution(WaitExecutionRequest {
        name: name.to_owned(),
    });
    timeout(PROMPT, asked)
        .await
        .expect("answered in time")
        .expect("the operation is known")
        .into_inner()
}

/// Catches an update that loses the build of a client whose Execute was open on the old
/// frontend: the operation must be answerable by the new frontend from the database, with the
/// result of the job already running and no second job. The old frontend is stopped while the
/// job runs (its stream ends `UNAVAILABLE`, `shutting down`); `WaitExecution` of the same name on
/// the new frontend is answered with the job's result; one job ran and one pack was read, and
/// the database holds the operation's answer and no row of it still open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_execute_open_on_a_stopped_frontend_is_answered_by_the_other_with_one_job() {
    let database = Database::new();
    let old = Cell::start_with_state(database.handle()).await;
    let new = old.restart_over(database.handle(), LIVE).await;
    let job = Job::new("/bin/build --mid-update", &[]);
    old.upload(&job.blobs()).await;
    old.direct
        .will_run(&job.action, Ran::writes("out", "built once"));

    let mut open = old.execute(&job.action).await;
    let name = first_name(&mut open).await;
    let running = old.direct.run(&job.action).await;
    old.drain.close();
    let cut = ended(&mut open).await;
    assert_eq!(cut.code(), Code::Unavailable, "{cut:?}");
    assert!(cut.message().contains("shutting down"), "{cut:?}");
    let direct = old.direct.clone();
    old.stop().await;

    let mut waited = wait(&new, &name).await;
    assert!(direct.fake.finish(&running, 0), "the job ends");
    let answered = done(&mut waited).await;
    assert_eq!(answered.name, name);
    let result = response(&answered).result.expect("a result");
    assert_eq!(result.exit_code, 0);
    let paths: Vec<&str> = result
        .output_files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(paths, ["out"]);

    assert_eq!(direct.jobs_of(&job.action).len(), 1, "a second job");
    let submits = direct
        .methods()
        .iter()
        .filter(|m| **m == "SubmitJob")
        .count();
    assert_eq!(submits, 1, "a second submit");
    assert_eq!(direct.ingestions().len(), 1, "the pack was read twice");
    let raw = database.handle();
    assert_eq!(keys(&raw, "exec/op/"), Vec::<String>::new(), "an open row");
    assert_eq!(
        keys(&raw, "exec/act/"),
        Vec::<String>::new(),
        "an action row"
    );
    assert_eq!(keys(&raw, "done/").len(), 1);
}

/// Catches a rebuild while two frontends serve together: an Execute on the new frontend of an
/// action the old one is running follows the old one's operation instead of submitting a job of
/// its own (the two Executes have different names, and the join is counted); when the one job ends both callers get its
/// result, one job ran and one pack was read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_execute_on_the_other_frontend_of_an_action_one_runs_follows_it() {
    let database = Database::new();
    let old = Cell::start_with_state(database.handle()).await;
    let new = old.restart_over(database.handle(), LIVE).await;
    let job = Job::new("/bin/build --twice", &[]);
    old.upload(&job.blobs()).await;
    old.direct
        .will_run(&job.action, Ran::writes("out", "built once"));

    let mut first = old.execute(&job.action).await;
    let first_op = first_name(&mut first).await;
    old.direct.job(&job.action, 0).await;
    let mut second = new.execute(&job.action).await;
    let second_op = first_name(&mut second).await;
    assert_ne!(first_op, second_op);
    joined(&new.direct, 1).await;

    let running = old.direct.run(&job.action).await;
    assert!(old.direct.fake.finish(&running, 0));
    let (a, b) = (done(&mut first).await, done(&mut second).await);
    assert_eq!(response(&a).result, response(&b).result);
    assert_eq!(response(&a).result.expect("a result").exit_code, 0);
    assert_eq!(old.direct.jobs_of(&job.action).len(), 1, "a second job");
    assert_eq!(old.direct.ingestions().len(), 1, "the pack was read twice");
    let raw = database.handle();
    assert_eq!(
        keys(&raw, "exec/act/"),
        Vec::<String>::new(),
        "an action row"
    );
    assert_eq!(keys(&raw, "done/").len(), 2, "each name is answered");
}
