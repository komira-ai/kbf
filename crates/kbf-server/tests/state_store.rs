//! The executor over a state store: the rows it writes (`exec/op/`, `done/`, `doneof/`, the id
//! counter), what a restart over the same store keeps, and what a store that is down does to an
//! Execute.
//!
//! The store is the in-memory fake, which keeps a durable copy a restart reopens. The tests that
//! restart over the rows run a second time over a real SQLite file, closed and reopened, and a
//! third time over a real Postgres schema (`KBF_TEST_POSTGRES_URL` names the server; those tests
//! fail without it). The tests of the server run over the fake, whose clock a test moves.
//! `state_store/frontends.rs` holds the tests of several frontends over one store.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_front::Finished;
use kbf_proto::google::rpc;
use kbf_server::StateError;
use kbf_server::StateStore;
use kbf_server::executor::rows::{DONE_FLOOR, ID_BLOCK, OP_PREFIX, OpRow, Rows, RowsError};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::postgres::{Flaw as PgFlaw, PostgresBackend, PostgresStore};
use kbf_state::sqlite::{Options, SqliteStore};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::{ActionKey, Digest, DigestFunction, OperationId};
use tonic::Code;

#[path = "state_store/frontends.rs"]
mod frontends;
mod support;

use support::{Cell, FakeState, Job, Ran, StateHandle, done, response};

/// A store whose writes can be made to fail, over the fake (which a restart reopens from what
/// was synced), a SQLite file or a Postgres schema.
struct Flaky {
    inner: Inner,
    down: AtomicBool,
    /// Every write applies, then fails `Failed`: the store took it and said it did not.
    lie: AtomicBool,
}

/// What the store is kept in.
#[derive(Clone, Copy)]
enum Backing {
    /// The in-memory fake.
    Fake,
    /// A SQLite file in a directory of the test's own.
    Sqlite,
    /// A schema of its own in the Postgres database `KBF_TEST_POSTGRES_URL` names.
    Postgres,
}

enum Inner {
    Fake(Fake),
    Sqlite(Tracked<SqliteStore>),
    Postgres(Box<Tracked<PostgresStore>>),
}

/// A store that records when it is closed, so a test can tell a restart that closed the store
/// and opened it again from one that kept the same object.
struct Tracked<S> {
    store: S,
    closed: Arc<AtomicBool>,
}

impl<S> Tracked<S> {
    fn new(store: S) -> Self {
        Self {
            store,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl<S> Drop for Tracked<S> {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

impl Inner {
    fn store(&self) -> &dyn Store {
        match self {
            Inner::Fake(store) => store,
            Inner::Sqlite(tracked) => &tracked.store,
            Inner::Postgres(tracked) => &tracked.store,
        }
    }
}

/// A directory removed when dropped.
struct Scratch(kbf_testkit::dir::TestDir);

impl Scratch {
    fn new() -> Self {
        Self(kbf_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            "kbf-state-store",
        ))
    }
}

fn open_sqlite(path: &std::path::Path) -> SqliteStore {
    SqliteStore::open(
        path,
        Arc::new(kbf_state::SystemClock::default()),
        Options::default(),
    )
    .expect("a SQLite store")
}

impl Flaky {
    fn new() -> Self {
        let clock = Arc::new(ManualClock::default());
        Self::over(Inner::Fake(Fake::with_flaw(clock, Flaw::None)))
    }

    fn over(inner: Inner) -> Self {
        Self {
            inner,
            down: AtomicBool::new(false),
            lie: AtomicBool::new(false),
        }
    }

    /// Set once the store this wraps is dropped; none over the fake.
    fn closed(&self) -> Option<Arc<AtomicBool>> {
        match &self.inner {
            Inner::Sqlite(tracked) => Some(Arc::clone(&tracked.closed)),
            Inner::Postgres(tracked) => Some(Arc::clone(&tracked.closed)),
            Inner::Fake(_) => None,
        }
    }

    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// A crash and reopen: what the store acknowledged survives.
    fn restart(self) -> Self {
        let inner = match self.inner {
            Inner::Fake(fake) => Inner::Fake(fake.restart()),
            Inner::Sqlite(tracked) => {
                // Dropping the store ends its writer and releases the file's lock; the file is
                // then opened again by its path, as a new process does.
                let path = tracked.store.path().to_owned();
                drop(tracked);
                Inner::Sqlite(Tracked::new(open_sqlite(&path)))
            }
            Inner::Postgres(tracked) => {
                // The connections are dropped and opened again on the same schema, as a new
                // process does; what the old store committed is what it finds.
                let config = tracked.store.config().clone();
                drop(tracked);
                let reopened = PostgresStore::open(&config).expect("the Postgres store reopens");
                Inner::Postgres(Box::new(Tracked::new(reopened)))
            }
        };
        Self::over(inner)
    }
}

impl Store for Flaky {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
        let applied = self.inner.store().apply_batch(batch)?;
        if self.lie.load(Ordering::SeqCst) {
            return Err(StoreError::Failed(
                "applied, but not acknowledged".to_owned(),
            ));
        }
        Ok(applied)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
        self.inner.store().get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
        self.inner.store().scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.store().scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.store().changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.inner.store().trim_feed(before)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.store().reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.store().commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.store().now()
    }
}

/// The store, and processes started over it one after another.
struct Disk {
    store: Arc<Flaky>,
    /// Kept for the SQLite file, and removed after the store is closed.
    _dir: Option<Scratch>,
    /// Kept for the Postgres schema, dropped with it after the store is closed. Declared after
    /// `store`, so the store goes first.
    _postgres: Option<PostgresBackend>,
}

impl Disk {
    fn over(backing: Backing) -> Self {
        let (inner, dir) = match backing {
            Backing::Fake => (None, None),
            Backing::Sqlite => {
                let dir = Scratch::new();
                let store = open_sqlite(&dir.0.join("state.db"));
                (Some(Inner::Sqlite(Tracked::new(store))), Some(dir))
            }
            Backing::Postgres => (None, None),
        };
        let postgres = (matches!(backing, Backing::Postgres)).then(|| {
            let url = kbf_testkit::database::server_url();
            PostgresBackend::new(&url, PgFlaw::None)
        });
        let inner = match &postgres {
            Some(backend) => {
                let store = PostgresStore::open(&backend.fresh_config()).expect("a Postgres store");
                Some(Inner::Postgres(Box::new(Tracked::new(store))))
            }
            None => inner,
        };
        let store = inner.map_or_else(Flaky::new, Flaky::over);
        Self {
            store: Arc::new(store),
            _dir: dir,
            _postgres: postgres,
        }
    }

    /// The rows of the process `instance` over the store.
    fn rows(&self, instance: &str) -> Rows {
        Rows::new(self.store.clone() as Arc<dyn Store>, instance)
    }

    /// The process dies: what it held of the store is dropped, and the store reopens as the
    /// disk left it. Over SQLite the old store must be closed and the file opened again (not
    /// the same object kept), and the new store must read every row the old one held.
    fn crash(&mut self, processes: Vec<Rows>) {
        drop(processes);
        let closed = self.store.closed();
        let rows = closed.as_ref().map(|_| self.all_rows());
        // The placeholder holds no file: a SQLite store must be closed before it reopens.
        let store = Arc::into_inner(std::mem::replace(&mut self.store, Arc::new(Flaky::new())));
        self.store = Arc::new(store.expect("nothing else holds the store").restart());
        if let (Some(closed), Some(rows)) = (closed, rows) {
            assert!(
                closed.load(Ordering::SeqCst),
                "the restart kept the store open"
            );
            let reopened = self.store.closed().expect("a real store");
            assert!(!Arc::ptr_eq(&closed, &reopened), "the same store object");
            assert!(!reopened.load(Ordering::SeqCst), "the new store is open");
            assert!(!rows.is_empty(), "the test wrote nothing before the crash");
            assert_eq!(
                self.all_rows(),
                rows,
                "the reopened store lost what was written"
            );
        }
    }

    fn all_rows(&self) -> Vec<(String, Versioned)> {
        self.store.scan_prefix("").expect("scan")
    }

    fn count(&self, prefix: &str) -> usize {
        self.store.scan_prefix(prefix).expect("scan").len()
    }
}

fn action(tag: u8) -> ActionKey {
    ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [tag; 32], 4096 + u64::from(tag)),
    }
}

/// The row of an operation of the action `tag`, whose job is attempt 0.
fn row(tag: u8) -> OpRow {
    let key = action(tag);
    OpRow {
        owner: "ignored".to_owned(),
        instance: key.instance,
        hash: key.action.hash_hex(),
        size: key.action.size_bytes,
        attempt: 0,
        job: format!("k0-{tag}-0"),
        key: None,
        whole_machine: false,
    }
}

fn answer(code: Code, message: &str) -> Finished {
    Finished::Failed(rpc::Status {
        code: code as i32,
        message: message.to_owned(),
        details: Vec::new(),
    })
}

/// Runs each restart test over the fake, over a SQLite file and over a Postgres schema: what
/// the fake keeps across a crash is what the real store must.
macro_rules! restarts {
    ($($name:ident),* $(,)?) => {$(
        mod $name {
            use super::*;

            #[test]
            fn over_the_fake() {
                super::$name(Backing::Fake);
            }

            #[test]
            fn over_sqlite() {
                super::$name(Backing::Sqlite);
            }

            #[test]
            fn over_postgres() {
                super::$name(Backing::Postgres);
            }
        }
    )*};
}

restarts!(
    a_restart_keeps_an_unfinished_operation_and_another_frontend_takes_it_up,
    a_restart_keeps_the_answer_of_a_finished_operation_with_its_action,
    a_restart_never_gives_an_operation_id_twice,
    a_crash_around_a_finish_leaves_the_operation_or_its_answer,
);

/// Asserts that a row the executor wrote for an unfinished operation survives the process: a
/// frontend of the restarted store sees the operation as the gone process's, takes it up, and
/// from then on its row names the new owner.
fn a_restart_keeps_an_unfinished_operation_and_another_frontend_takes_it_up(backing: Backing) {
    let mut disk = Disk::over(backing);
    let before = disk.rows("front-a");
    before
        .put_op(OperationId(7), &row(1))
        .expect("the row is written");
    disk.crash(vec![before]);

    let after = disk.rows("front-b");
    let kept = after
        .op(OperationId(7))
        .expect("a read")
        .expect("the row survived");
    assert_eq!(kept.owner, "front-a");
    assert_eq!(kept.job, "k0-1-0");
    let orphans = after.orphans().expect("a scan");
    let [orphan] = &orphans[..] else {
        panic!("one gone operation expected: {orphans:?}");
    };
    assert_eq!(orphan.operation, OperationId(7));
    let taken = after.claim(orphan).expect("claimed");
    assert_eq!(taken.owner, "front-b");
    assert_eq!(
        after
            .op(OperationId(7))
            .expect("a read")
            .expect("a row")
            .owner,
        "front-b"
    );
    assert!(after.orphans().expect("a scan").is_empty());
}

/// Asserts that a finished operation is answered by name after the process that finished it is
/// gone: the answer and the action it was for are there, its row is not, and the rows are the
/// ones an earlier release reads (`done/` and `doneof/`).
fn a_restart_keeps_the_answer_of_a_finished_operation_with_its_action(backing: Backing) {
    let mut disk = Disk::over(backing);
    let before = disk.rows("front-a");
    before
        .put_op(OperationId(3), &row(2))
        .expect("the row is written");
    let sent = answer(Code::DeadlineExceeded, "timed out");
    let kept = before
        .finish(OperationId(3), &action(2), &sent, DONE_FLOOR)
        .expect("finished");
    assert_eq!(kept, sent);
    disk.crash(vec![before]);

    let after = disk.rows("front-b");
    assert_eq!(
        after.done(OperationId(3)).expect("a read"),
        Some((action(2), sent))
    );
    assert!(after.op(OperationId(3)).expect("a read").is_none());
    assert_eq!(disk.count(OP_PREFIX), 0, "the operation's row is gone");
    assert_eq!(disk.count("done/"), 1);
    assert_eq!(disk.count("doneof/"), 1);
}

/// Asserts that a frontend that restarts, and another that runs beside it, never hand out one
/// operation id twice: the ids of every process over the store are distinct, and a restarted
/// process begins above the block its predecessor reserved.
fn a_restart_never_gives_an_operation_id_twice(backing: Backing) {
    let mut disk = Disk::over(backing);
    let (a, b) = (disk.rows("front-a"), disk.rows("front-b"));
    let mut ids = vec![
        a.next_id().expect("an id"),
        b.next_id().expect("an id"),
        a.next_id().expect("an id"),
    ];
    assert!(ids.iter().all(|id| id.0 > 0), "id 0 is never given");
    disk.crash(vec![a, b]);
    let c = disk.rows("front-c");
    ids.push(c.next_id().expect("an id"));
    let mut distinct = ids.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), ids.len(), "an id was given twice: {ids:?}");
    assert!(
        ids[3].0 > 2 * ID_BLOCK,
        "the restarted process took an id inside a block another process reserved: {ids:?}"
    );
}

/// Asserts that a finish is one write: whether the store refuses it (the process dies at the
/// write) or takes it and does not say so, the store holds exactly one of the operation's row
/// and its answer, so a successor neither loses the result nor runs the action again beside an
/// answer.
fn a_crash_around_a_finish_leaves_the_operation_or_its_answer(backing: Backing) {
    for applied in [false, true] {
        let mut disk = Disk::over(backing);
        let before = disk.rows("front-a");
        before
            .put_op(OperationId(5), &row(3))
            .expect("the row is written");
        if applied {
            disk.store.lie.store(true, Ordering::SeqCst);
        } else {
            disk.store.set_down(true);
        }
        let finished = before.finish(
            OperationId(5),
            &action(3),
            &answer(Code::Internal, "ended"),
            DONE_FLOOR,
        );
        assert!(
            matches!(finished, Err(RowsError::Store(_))),
            "the finish was acknowledged: {finished:?}"
        );
        disk.store.set_down(false);
        disk.store.lie.store(false, Ordering::SeqCst);
        disk.crash(vec![before]);

        let after = disk.rows("front-b");
        let unfinished = usize::from(after.op(OperationId(5)).expect("a read").is_some());
        let answered = usize::from(after.done(OperationId(5)).expect("a read").is_some());
        assert_eq!(
            (unfinished, answered),
            if applied { (0, 1) } else { (1, 0) },
            "applied {applied}: the store holds the operation and its answer, or neither"
        );
    }
}

/// A state store opened as a start opens it: from the flag, over `dir`.
fn store_from_flag(dir: &std::path::Path) -> Result<StateStore, StateError> {
    let uri = format!("--state-store=sqlite://{}", dir.join("state.db").display());
    let args = <kbf_server::Args as clap::Parser>::parse_from(["kbf-server", &uri]);
    args.open_state_store()
        .map(|state| state.expect("the flag is given"))
}

/// Asserts that `--state-store=sqlite:///...` is opened and used: an operation's row written
/// through the flag's store is found by the process a restart starts over the same file.
#[test]
fn an_operation_row_written_over_the_flag_s_store_survives_a_restart_of_the_file() {
    let dir = Scratch::new();
    let before = Rows::new(store_from_flag(&dir.0).expect("a first start").store, "a");
    before.put_op(OperationId(9), &row(4)).expect("written");
    drop(before);

    let after = Rows::new(store_from_flag(&dir.0).expect("a second start").store, "b");
    let kept = after.op(OperationId(9)).expect("a read");
    assert_eq!(kept.map(|row| row.owner), Some("a".to_owned()));
}

/// Asserts that a second frontend started on a state file another holds is refused naming the
/// lock (two frontends over one file would not see each other's rows), and that the file is
/// free again once the first goes.
#[test]
fn a_second_frontend_on_the_same_sqlite_file_is_refused() {
    let dir = Scratch::new();
    let first = store_from_flag(&dir.0).expect("a first start");
    let refused = store_from_flag(&dir.0).err().expect("the file is held");
    assert!(matches!(refused, StateError::Open(_)), "{refused:?}");
    let said = refused.to_string();
    assert!(said.contains("another kbf process"), "{said}");
    assert!(said.contains("state.db.lock"), "{said}");
    drop(first);
    store_from_flag(&dir.0).expect("free again");
}

/// Waits until `look` shows what the test expects, bounded by [`support::PROMPT`].
async fn eventually<T>(what: &str, mut look: impl FnMut() -> Option<T>) -> T {
    let found = async {
        loop {
            if let Some(found) = look() {
                return found;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(support::PROMPT, found)
        .await
        .unwrap_or_else(|_| panic!("{what}"))
}

/// Asserts that a server bound over a state store keeps the operation's row while the job runs
/// and replaces it with its answer when the job finishes: the process's liveness row and an
/// `exec/op/` row while it runs, then a `done/` and a `doneof/` row, and no `exec/op/` row, all
/// of it in the store before the caller is answered.
#[tokio::test]
async fn a_server_over_a_state_store_keeps_an_operation_s_row_while_it_runs_and_its_answer_after() {
    let state = FakeState::new();
    let cell = Cell::start_with_state(state.handle()).await;
    let job = Job::new("/bin/run", &[]);
    cell.upload(&job.blobs()).await;
    cell.direct
        .will_run(&job.action, Ran::writes("out", "kept"));

    let mut ops = cell.execute(&job.action).await;
    cell.direct.run(&job.action).await;
    assert_eq!(state.keys("exec/op/").len(), 1, "the running operation");
    eventually("the process's liveness row", || {
        (state.keys("exec/live/").len() == 1).then_some(())
    })
    .await;
    assert!(state.keys("done/").is_empty());

    let id = cell.direct.job(&job.action, 0).await;
    assert!(cell.direct.fake.finish(&id, 0));
    let result = response(&done(&mut ops).await).result.expect("a result");

    assert_eq!(result.output_files[0].path, "out");
    assert_eq!(state.keys("exec/op/").len(), 0, "the operation is finished");
    assert_eq!(state.keys("done/").len(), 1);
    assert_eq!(state.keys("doneof/").len(), 1);
}

/// Asserts that an operation answered without a result (its job hit its wall limit) leaves the
/// answer the caller was sent in the store: the `done/` row, and no `exec/op/` row. A client
/// that lost its stream asks again by name and gets the same answer.
#[tokio::test]
async fn a_timed_out_operation_leaves_the_answer_the_caller_was_sent_in_the_store() {
    let state = FakeState::new();
    let cell = Cell::start_with_state(state.handle()).await;
    let job = Job::new("/bin/slow", &[]);
    cell.upload(&job.blobs()).await;

    let mut ops = cell.execute(&job.action).await;
    cell.direct.time_out(&job.action).await;
    let sent = response(&done(&mut ops).await).status.expect("a status");

    assert_eq!(sent.code, Code::DeadlineExceeded as i32);
    assert_eq!(state.keys("done/").len(), 1);
    assert!(state.keys("exec/op/").is_empty());
}

/// Asserts that an Execute that joins an open operation of the same action writes no second
/// operation: one row, one job, and both callers are answered by it.
#[tokio::test]
async fn an_execute_that_joins_an_open_operation_writes_no_second_row() {
    let state = FakeState::new();
    let cell = Cell::start_with_state(state.handle()).await;
    let job = Job::new("/bin/run", &[]);
    cell.upload(&job.blobs()).await;

    let mut first = cell.execute(&job.action).await;
    cell.direct.run(&job.action).await;
    let mut twin = cell.execute(&job.action).await;
    assert_eq!(state.keys("exec/op/").len(), 1);
    assert_eq!(cell.direct.jobs_of(&job.action).len(), 1);
    let id = cell.direct.job(&job.action, 0).await;
    assert!(cell.direct.fake.finish(&id, 0));

    assert_eq!(
        response(&done(&mut first).await).status.map(|s| s.code),
        Some(0)
    );
    assert_eq!(
        response(&done(&mut twin).await).status.map(|s| s.code),
        Some(0)
    );
    assert_eq!(state.keys("done/").len(), 1, "one operation was answered");
}

/// Asserts that a state store that is down refuses an Execute the cache cannot answer, as
/// UNAVAILABLE (a client tries again), and submits no job; and that once the store is back the
/// same Execute runs.
#[tokio::test]
async fn a_state_store_that_is_down_refuses_an_execute_and_submits_no_job() {
    let flaky = Arc::new(Flaky::new());
    let state = Arc::new(StateHandle {
        store: flaky.clone(),
    });
    let cell = Cell::start_with_state(state).await;
    let job = Job::new("/bin/run", &[]);
    cell.upload(&job.blobs()).await;

    flaky.set_down(true);
    let refused = match cell.execute_with(&job.action, false).await {
        Ok(mut ops) => loop {
            match ops.message().await {
                Err(status) => break status,
                Ok(Some(_)) => {}
                Ok(None) => panic!("the stream ended without an error"),
            }
        },
        Err(status) => status,
    };
    assert_eq!(refused.code(), Code::Unavailable, "{refused:?}");
    assert_eq!(cell.direct.submitted_jobs(), 0);

    flaky.set_down(false);
    let mut ops = cell.execute(&job.action).await;
    cell.direct.complete(&job.action).await;
    assert_eq!(
        response(&done(&mut ops).await).status.map(|s| s.code),
        Some(0)
    );
}
