//! The Postgres store when the server or the connection fails under it: a backend
//! killed by the server, a connection refused for a role's limit, a pool all in use.
//! Each fault is made on a real server, by a second connection or a role's settings.
//!
//! The tests need a server and fail without one, so the pull-request gate runs them with the
//! rest. `KBF_TEST_POSTGRES_URL` names a database whose user may create roles and schemas (a
//! superuser). Waits are on conditions read from the server; a wait the test expects to end
//! has a bound so a lost wake-up fails the test and does not hang it.
#![cfg(feature = "postgres")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use kbf_state::postgres::{Flaw, PostgresBackend, PostgresConfig, PostgresStore, schema};
use kbf_state::{Cond, Revision, Store, StoreError, Txn};
use kbf_testkit::database::{CatalogGuard, batch_execute_retrying, ddl};
use postgres::{Client, NoTls};

const BOUND: Duration = Duration::from_secs(60);

fn url() -> String {
    kbf_testkit::database::server_url()
}

fn admin() -> Client {
    Client::connect(&url(), NoTls).expect("connect")
}

/// Polls `ready` until it holds, failing after [`BOUND`].
fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let polls = BOUND.as_millis() / 5;
    for _ in 0..polls {
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("{what}: not within {BOUND:?}");
}

/// How many connections named `app` the server has, optionally only those waiting on a lock.
fn connections(admin: &mut Client, app: &str, waiting: bool) -> i64 {
    admin
        .query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE application_name = $1 \
             AND ($2 = false OR wait_event_type = 'Lock')",
            &[&app, &waiting],
        )
        .expect("the activity view")
        .get(0)
}

/// Terminates every connection named `app` from this one, and waits until they are gone.
fn kill(admin: &mut Client, app: &str) {
    admin
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name = $1",
            &[&app],
        )
        .expect("terminates");
    until("the killed connections go", || {
        connections(admin, app, false) == 0
    });
}

fn put(key: &str) -> Txn {
    Txn::new().put(key, "v", Cond::Any)
}

/// Runs `f` on a thread and returns its result, failing if it takes longer than [`BOUND`].
fn within<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, result) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(f());
    });
    result
        .recv_timeout(BOUND)
        .unwrap_or_else(|_| panic!("{what}: not finished within {BOUND:?}"))
}

/// A configuration on a schema of its own, with the application name the schema's.
fn config(backend: &PostgresBackend) -> PostgresConfig {
    backend.fresh_config()
}

fn app(config: &PostgresConfig) -> String {
    config.schema().to_owned()
}

/// `url` with its user replaced by `user` and no password.
fn as_user(url: &str, user: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("a URL");
    let rest = rest.split_once('@').map_or(rest, |(_, tail)| tail);
    format!("{scheme}://{user}@{rest}")
}

/// A role that may log in, create schemas, and hold at most `limit` connections. The
/// URL is the test URL as that role. Role names are one per test, and the roles of all the
/// tests of a run are made on one server by processes running at once, so the DDL goes
/// through the guard of the shared catalogs (see `kbf_testkit::database`).
fn limited_role(admin: &mut Client, name: &str, limit: i32) -> String {
    let _catalog = CatalogGuard::take();
    let dropped = admin
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&name])
        .expect("roles")
        .len();
    if dropped > 0 {
        let remove = format!("DROP OWNED BY {name}; DROP ROLE {name}");
        batch_execute_retrying(admin, &remove).expect("drops the role of an earlier run");
    }
    let database: String = admin
        .query_one("SELECT current_database()", &[])
        .expect("database")
        .get(0);
    let create = format!(
        "CREATE ROLE {name} LOGIN CONNECTION LIMIT {limit}; \
         GRANT CREATE ON DATABASE \"{database}\" TO {name}"
    );
    batch_execute_retrying(admin, &create).expect("creates the role");
    as_user(&url(), name)
}

/// How many backends the server has for `role`.
fn backends_of(admin: &mut Client, role: &str) -> i64 {
    admin
        .query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE usename = $1",
            &[&role],
        )
        .expect("the activity view")
        .get(0)
}

/// Waits until the server has no backend for `role`. A client that drops its connection
/// (the installer step that made the schema) has only sent the goodbye: the server frees
/// the role's slot when its backend exits, a moment later, and a role limited to one
/// connection refuses the next connect until then (SQLSTATE 53300). The bound only ends
/// a hang.
fn wait_for_no_backend_of(admin: &mut Client, role: &str) {
    until(&format!("the backends of {role} to exit"), || {
        backends_of(admin, role) == 0
    });
}

/// Opens the store on a one-connection role, again while the server refuses it for the
/// role's limit, and counts each refusal in `refusals`. Any other error panics at once.
///
/// It exists because [`wait_for_no_backend_of`] can return a moment before the slot is
/// free. Postgres clears a backend's `pg_stat_activity` row in `pgstat_beshutdown_hook`
/// (a `before_shmem_exit` callback) and releases its process slot, the one the
/// connection limit counts, later in `ProcKill` (an `on_shmem_exit` callback), so a
/// connect between the two is refused with SQLSTATE 53300. The bound only ends a hang.
fn open_when_the_slot_is_free(config: &PostgresConfig, refusals: &AtomicU64) -> PostgresStore {
    let started = Instant::now();
    loop {
        match PostgresStore::open(config) {
            Ok(store) => return store,
            Err(StoreError::Failed(why)) if why.contains("SQLSTATE 53300") => {
                assert!(
                    started.elapsed() < BOUND,
                    "still refused after {BOUND:?}: {why}"
                );
                refusals.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(5));
            }
            Err(other) => panic!("opens: {other:?}"),
        }
    }
}

/// The configuration for the limited role's own schema (`DROP OWNED BY` removes it with
/// the role at the start of the next run).
fn limited_config(url: &str, schema: &str) -> PostgresConfig {
    let config = PostgresConfig::parse(url)
        .and_then(|c| c.with_schema(schema))
        .expect("a usable URL")
        .with_application_name(schema);
    // The installer's step, as the role the store will connect as.
    schema::apply(&config, &schema::embedded(), schema::latest()).expect("the installer's step");
    config
}

// Catches a pool that keeps a session the server killed: the call that meets the dead
// session fails, and the next calls must run on a new connection. A store that handed
// the dead session out again would fail every call after.
#[test]
fn a_session_the_server_killed_fails_one_call_and_is_replaced() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let config = config(&backend);
    let store = PostgresStore::open(&config).expect("opens");
    store.txn(put("k")).expect("store").expect("commits");
    let mut admin = admin();
    kill(&mut admin, &app(&config));
    let first = store.get("k");
    assert!(
        matches!(first, Err(StoreError::Failed(_))),
        "the dead session fails the call that meets it: {first:?}"
    );
    assert!(
        store.get("k").expect("a new connection").is_some(),
        "the next call runs on a new connection and sees the row"
    );
}

// Catches a write on a dead session that reports success or leaves a half-open
// transaction: the call fails, nothing is written, and the store takes the next write.
#[test]
fn a_write_on_a_killed_session_fails_writes_nothing_and_the_store_recovers() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let config = config(&backend);
    let store = PostgresStore::open(&config).expect("opens");
    let mut admin = admin();
    kill(&mut admin, &app(&config));
    let lost = store.txn(put("lost"));
    assert!(
        matches!(lost, Err(StoreError::Failed(_))),
        "a write on a dead session is an error: {lost:?}"
    );
    assert!(
        store.get("lost").expect("reads").is_none(),
        "nothing was written"
    );
    store
        .txn(put("after"))
        .expect("store")
        .expect("the store recovered");
}

// Catches a commit whose connection died being taken for a success (the caller would
// believe a write that never happened): it is an error, the row is absent, and the
// session is not reused.
#[test]
fn a_commit_that_loses_its_connection_is_an_error_and_writes_nothing() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let config = config(&backend);
    let store = PostgresStore::open(&config).expect("opens");
    let pending = store.begin(put("k")).expect("runs to the commit");
    let mut admin = admin();
    kill(&mut admin, &app(&config));
    let committed = pending.commit();
    assert!(
        matches!(committed, Err(StoreError::Failed(_))),
        "the lost commit is an error: {committed:?}"
    );
    assert!(
        store.get("k").expect("reads").is_none(),
        "the write did not happen"
    );
    store
        .txn(put("after"))
        .expect("store")
        .expect("the store recovers");
}

// Catches a transaction left open on a session whose connection died: dropping it must
// not panic or hang, and the store must take writes after.
#[test]
fn a_held_transaction_dropped_after_its_connection_died_does_not_poison_the_store() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let config = config(&backend);
    let store = PostgresStore::open(&config).expect("opens");
    let pending = store.begin(put("k")).expect("runs to the commit");
    let mut admin = admin();
    kill(&mut admin, &app(&config));
    drop(pending);
    store
        .txn(put("after"))
        .expect("store")
        .expect("the store recovers");
    assert!(store.get("k").expect("reads").is_none());
}

// Catches a pool of two that grows past two or does not wake a waiter: with the one
// session held by a commit in progress, a second call opens the second connection, and
// a third waits for a session to come back and gets it when it does.
#[test]
fn a_pool_of_two_opens_a_second_connection_and_makes_a_third_call_wait() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let config = config(&backend).with_max_connections(2);
    let app = app(&config);
    let store = Arc::new(PostgresStore::open(&config).expect("opens"));
    let held = store.begin(put("held")).expect("runs to the commit");
    let second = {
        let store = Arc::clone(&store);
        thread::spawn(move || store.txn(put("second")))
    };
    let mut admin = admin();
    until("the second call queued on the commit head", || {
        connections(&mut admin, &app, true) == 1
    });
    assert_eq!(
        connections(&mut admin, &app, false),
        2,
        "the pool grew to two"
    );
    let (started, running) = mpsc::channel();
    let third = {
        let store = Arc::clone(&store);
        thread::spawn(move || {
            started.send(()).expect("main is waiting");
            store.get("held")
        })
    };
    running.recv_timeout(BOUND).expect("the third call started");
    // The third call has no session to take. Nothing on the server shows it waiting, so
    // it gets a moment to reach the wait; the commit below wakes it either way.
    thread::sleep(Duration::from_millis(200));
    held.commit().expect("the held transaction commits");
    second
        .join()
        .expect("no panic")
        .expect("store")
        .expect("commits");
    within("the waiting call", move || third.join().expect("no panic"))
        .expect("the waiter got a session");
    assert_eq!(
        connections(&mut admin, &app, false),
        2,
        "never more than two"
    );
}

// Catches a failed attempt to open a pooled connection leaving the pool counting it: the
// refusal must not eat a slot. With a role limited to two connections and a pool of three,
// the third connection is refused twice running, both times with the server's words, and
// a call that waits on the pool would never have been woken had the slot leaked.
#[test]
fn a_connection_the_server_refuses_does_not_take_a_pool_slot() {
    let mut admin = admin();
    let url = limited_role(&mut admin, "kbf_fault_pool", 2);
    let config = limited_config(&url, "kbf_fault_pool_schema").with_max_connections(3);
    let store = Arc::new(PostgresStore::open(&config).expect("opens"));
    let held = store.begin(put("held")).expect("runs to the commit");
    let second = {
        let store = Arc::clone(&store);
        thread::spawn(move || store.txn(put("second")))
    };
    let app = app(&config);
    until("the second call queued on the commit head", || {
        connections(&mut admin, &app, true) == 1
    });
    for _ in 0..2 {
        let store = Arc::clone(&store);
        let refused = within("a refused connection", move || store.get("held"));
        let Err(StoreError::Failed(why)) = refused else {
            panic!("a third connection is over the role's limit: {refused:?}");
        };
        assert!(why.contains("too many connections"), "{why}");
    }
    held.commit().expect("the held transaction commits");
    second
        .join()
        .expect("no panic")
        .expect("store")
        .expect("commits");
    assert!(store.get("held").expect("reads").is_some());
}

/// Asserts that a reap with nothing due answers 0, with a row due answers 1 and then 0, and
/// leaves a row that is not due.
#[test]
fn a_reap_answers_what_it_deleted() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let store = PostgresStore::open(&config(&backend)).expect("opens");
    assert_eq!(store.reap_expired().expect("reaps"), 0, "nothing is due");
    store
        .txn(
            Txn::new()
                .put_ttl("due1", "d", Duration::ZERO, Cond::Any)
                .put("keep", "k", Cond::Any),
        )
        .expect("store")
        .expect("commits");
    assert_eq!(store.reap_expired().expect("reaps"), 1, "due1 goes");
    assert_eq!(store.reap_expired().expect("reaps"), 0, "it went once");
    assert!(store.get("keep").expect("reads").is_some());
}

/// The pids of the connections named `app`, sorted.
fn pids(admin: &mut Client, app: &str) -> Vec<i32> {
    let mut found: Vec<i32> = admin
        .query(
            "SELECT pid FROM pg_stat_activity WHERE application_name = $1",
            &[&app],
        )
        .expect("the activity view")
        .iter()
        .map(|row| row.get(0))
        .collect();
    found.sort_unstable();
    found
}

// Catches a store whose tables were dropped under it either hanging or handing out a
// half-prepared session: every call fails with the server's words (the relation is
// missing) until the installer's step has made them again, and the failed connection attempts leave no slot
// taken (a pool of two would block for good on a leaked slot), so the store works again
// once they are.
#[test]
fn a_store_whose_tables_were_dropped_fails_its_calls_and_recovers_when_they_return() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let config = config(&backend).with_max_connections(2);
    let store = Arc::new(PostgresStore::open(&config).expect("opens"));
    assert_eq!(store.config().schema(), config.schema());
    let mut admin = admin();
    kill(&mut admin, &app(&config));
    admin
        .batch_execute(&format!("DROP SCHEMA {} CASCADE", config.schema()))
        .expect("drops the tables");
    for _ in 0..3 {
        let store = Arc::clone(&store);
        let said = within("a call on dropped tables", move || store.get("k"));
        assert!(
            matches!(&said, Err(StoreError::Failed(why)) if !why.is_empty()),
            "{said:?}"
        );
    }
    schema::apply(&config, &schema::embedded(), schema::latest()).expect("the installer's step");
    store.txn(put("k")).expect("store").expect("commits");
    assert!(store.get("k").expect("recovered").is_some());
}

// Catches a commit the server refuses (a deferred constraint fails at COMMIT) being
// reported as success, a session that survived a refusal being thrown away (a refusal
// leaves the connection usable), and a refusal that leaves the row behind.
#[test]
fn a_commit_the_server_refuses_is_an_error_and_the_session_is_kept() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let config = config(&backend);
    let schema = config.schema().to_owned();
    let store = PostgresStore::open(&config).expect("opens");
    let mut admin = admin();
    admin
        .batch_execute(&format!(
            "CREATE FUNCTION {schema}.refuse() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'commit refused by the test'; END $$; \
             CREATE CONSTRAINT TRIGGER refuse AFTER INSERT ON {schema}.kv \
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {schema}.refuse()"
        ))
        .expect("a trigger that fails at commit");
    let before = pids(&mut admin, &app(&config));
    assert_eq!(before.len(), 1);
    let refused = store.txn(put("k"));
    assert!(
        matches!(&refused, Err(StoreError::Failed(why)) if why.contains("commit refused by the test")),
        "{refused:?}"
    );
    assert_eq!(
        pids(&mut admin, &app(&config)),
        before,
        "a refusal from the server leaves the same connection in use"
    );
    admin
        .batch_execute(&format!("DROP TRIGGER refuse ON {schema}.kv"))
        .expect("removes the trigger");
    assert!(
        store.get("k").expect("reads").is_none(),
        "the refused row is not there"
    );
    store
        .txn(put("k"))
        .expect("store")
        .expect("commits once the trigger is gone");
}

// Catches a transaction that only checks (or deletes a key that is not there) writing a
// revision or a feed row, and a commit that reports a revision other than the head: the
// reported revision is the last one written. A store with sequence revisions has no
// head to report and says 0.
#[test]
fn a_transaction_that_writes_nothing_commits_at_the_head_and_adds_no_feed_row() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let store = PostgresStore::open(&config(&backend)).expect("opens");
    let written = store.txn(put("k")).expect("store").expect("commits");
    for nothing in [
        Txn::new().check("k", Cond::Any),
        Txn::new().delete("absent", Cond::Any),
    ] {
        let commit = store.txn(nothing).expect("store").expect("commits");
        assert_eq!(
            commit.revision, written.revision,
            "the head, not a new revision"
        );
    }
    assert_eq!(
        store.changes(Revision::ZERO, 10).expect("feed").rows.len(),
        1
    );

    let flawed = PostgresBackend::new(&url(), Flaw::SequenceRevisions);
    let flawed = PostgresStore::open(&config(&flawed))
        .expect("opens")
        .with_flaw(Flaw::SequenceRevisions);
    flawed.txn(put("k")).expect("store").expect("commits");
    let commit = flawed
        .txn(Txn::new().check("k", Cond::Any))
        .expect("store")
        .expect("commits");
    assert_eq!(commit.revision, Revision::ZERO);
}

// Catches a wait that returns while the role still has a backend: it ends a hang, and
// the role's backends must be gone when it returns. The slot is held, the wait starts on
// its own thread, and only then is the slot freed (`freed` is set before the drop). A
// no-op wait, or one that filters on a column that never matches, returns while `freed` is
// still false.
#[test]
fn the_wait_for_a_roles_backends_ends_only_after_they_are_gone() {
    let waiter_admin = admin();
    let mut admin = admin();
    let url = limited_role(&mut admin, "kbf_slot_wait", 1);
    let _config = limited_config(&url, "kbf_slot_wait_schema");
    wait_for_no_backend_of(&mut admin, "kbf_slot_wait");

    let held = Client::connect(&url, NoTls).expect("takes the role's one slot");
    assert_eq!(backends_of(&mut admin, "kbf_slot_wait"), 1);
    let started = Arc::new(AtomicBool::new(false));
    let freed = Arc::new(AtomicBool::new(false));
    let waiter = {
        let (started, freed) = (Arc::clone(&started), Arc::clone(&freed));
        thread::spawn(move || {
            let mut admin = waiter_admin;
            started.store(true, Ordering::SeqCst);
            wait_for_no_backend_of(&mut admin, "kbf_slot_wait");
            (
                freed.load(Ordering::SeqCst),
                backends_of(&mut admin, "kbf_slot_wait"),
            )
        })
    };
    until("the waiter to start", || started.load(Ordering::SeqCst));
    assert_eq!(backends_of(&mut admin, "kbf_slot_wait"), 1, "still held");
    freed.store(true, Ordering::SeqCst);
    drop(held);
    let (freed_when_it_returned, left) = waiter.join().expect("no panic");
    assert!(
        freed_when_it_returned,
        "the wait returned with the slot held"
    );
    assert_eq!(left, 0);
}

// Catches an open that gives up on a refusal that is only the previous backend's slot not
// yet released, and one that retries every error. The slot is held across the first
// attempts; the helper counts its refusals, and only after it has refused once is the slot
// freed, so the open must have retried and then succeeded. A helper that does not retry
// panics on the first refusal.
#[test]
fn an_open_refused_for_the_roles_limit_is_retried_until_the_slot_is_free() {
    let mut admin = admin();
    let url = limited_role(&mut admin, "kbf_slot_retry", 1);
    let config = limited_config(&url, "kbf_slot_retry_schema");
    wait_for_no_backend_of(&mut admin, "kbf_slot_retry");

    let held = Client::connect(&url, NoTls).expect("takes the role's one slot");
    let refusals = Arc::new(AtomicU64::new(0));
    let freer = {
        let refusals = Arc::clone(&refusals);
        thread::spawn(move || {
            until("the open to be refused once", || {
                refusals.load(Ordering::SeqCst) > 0
            });
            drop(held);
        })
    };
    let _store = open_when_the_slot_is_free(&config, &refusals);
    freer.join().expect("no panic");
    assert!(refusals.load(Ordering::SeqCst) >= 1);
}

// Catches a retry that hides other failures: an open that fails for another reason (a
// schema this store does not support, here a role that does not exist) fails at once.
#[test]
fn an_open_refused_for_another_reason_is_not_retried() {
    let mut admin = admin();
    let role_url = limited_role(&mut admin, "kbf_slot_other", 1);
    let config = limited_config(&role_url, "kbf_slot_other_schema");
    wait_for_no_backend_of(&mut admin, "kbf_slot_other");
    ddl(&url(), "ALTER ROLE kbf_slot_other NOLOGIN").expect("revokes the login");
    let refusals = AtomicU64::new(0);
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        open_when_the_slot_is_free(&config, &refusals)
    }));
    assert!(failed.is_err(), "an open that cannot log in succeeded");
    assert_eq!(refusals.load(Ordering::SeqCst), 0, "it was retried");
}
