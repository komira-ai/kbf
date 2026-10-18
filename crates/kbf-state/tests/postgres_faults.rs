//! The Postgres store when the server or the connection fails under it: a backend
//! killed by the server, a connection refused for a role's limit, a pool all in use.
//! Each fault is made on a real server, by a second connection or a role's settings.
//!
//! The tests are `#[ignore]`d so `cargo test --workspace` stays hermetic; the
//! `state-postgres` job in ci.yml runs them with `--include-ignored` against a service
//! container. `KBF_TEST_POSTGRES_URL` names a database whose user may create roles and
//! schemas (a superuser). Waits are on conditions read from the server; a wait the test
//! expects to end has a bound so a lost wake-up fails the test and does not hang it.
#![cfg(feature = "postgres")]

use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use kbf_state::postgres::{Flaw, PostgresBackend, PostgresConfig, PostgresStore};
use kbf_state::{Cond, Revision, Store, StoreError, Txn, Version};
use postgres::{Client, NoTls};

const BOUND: Duration = Duration::from_secs(60);

fn url() -> String {
    std::env::var("KBF_TEST_POSTGRES_URL").unwrap_or_else(|_| {
        panic!("KBF_TEST_POSTGRES_URL must name a postgres:// URL the tests may create schemas in")
    })
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
/// URL is the test URL as that role.
fn limited_role(admin: &mut Client, name: &str, limit: i32) -> String {
    // Two roles granted on one database at once fail with "tuple concurrently updated".
    static CATALOG: Mutex<()> = Mutex::new(());
    let _one_at_a_time = CATALOG.lock().unwrap_or_else(PoisonError::into_inner);
    let dropped = admin
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&name])
        .expect("roles")
        .len();
    if dropped > 0 {
        admin
            .batch_execute(&format!("DROP OWNED BY {name}; DROP ROLE {name}"))
            .expect("drops the role of an earlier run");
    }
    let database: String = admin
        .query_one("SELECT current_database()", &[])
        .expect("database")
        .get(0);
    admin
        .batch_execute(&format!(
            "CREATE ROLE {name} LOGIN CONNECTION LIMIT {limit}; \
             GRANT CREATE ON DATABASE \"{database}\" TO {name}"
        ))
        .expect("creates the role");
    as_user(&url(), name)
}

fn limited_config(url: &str, backend: &PostgresBackend) -> PostgresConfig {
    let schema = config(backend).schema().to_owned();
    PostgresConfig::parse(url)
        .and_then(|c| c.with_schema(&schema))
        .expect("a usable URL")
        .with_application_name(&schema)
}

// Catches a pool that keeps a session the server killed: the call that meets the dead
// session fails, and the next calls must run on a new connection. A store that handed
// the dead session out again would fail every call after.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
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
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
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
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
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
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
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
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
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
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_connection_the_server_refuses_does_not_take_a_pool_slot() {
    let mut admin = admin();
    let url = limited_role(&mut admin, "kbf_fault_pool", 2);
    let backend = PostgresBackend::new(&url, Flaw::None);
    let config = limited_config(&url, &backend).with_max_connections(3);
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

// Catches a liveness lock that reports a connection the server refused as held: the
// holder connection is the store's own and the role's limit refuses it, so the call must
// fail with the server's words and not hand out a guard.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_liveness_lock_whose_connection_is_refused_is_an_error() {
    let mut admin = admin();
    let url = limited_role(&mut admin, "kbf_fault_hold", 1);
    let backend = PostgresBackend::new(&url, Flaw::None);
    let store = PostgresStore::open(&limited_config(&url, &backend)).expect("opens");
    let Err(StoreError::Failed(why)) = store.hold_liveness(1) else {
        panic!("the holder connection is over the role's limit");
    };
    assert!(why.contains("too many connections"), "{why}");
}

// Catches a reap that counts or syncs when it deleted nothing, one that skips a due row,
// and a guarded reap that runs past a precondition that failed: a reap with nothing due
// answers 0, with a row due answers 1, and a guard that does not hold answers the
// conflict and deletes nothing.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_reap_answers_what_it_deleted_and_a_guard_that_fails_deletes_nothing() {
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
    assert_eq!(
        store.reap_expired_if("none", Cond::Absent).expect("reaps"),
        Ok(0),
        "a guard that holds over nothing due"
    );
    store
        .txn(Txn::new().put_ttl("due2", "d", Duration::ZERO, Cond::Any))
        .expect("store")
        .expect("commits");
    let conflict = store
        .reap_expired_if("keep", Cond::Version(Version(u64::MAX >> 1)))
        .expect("reaps")
        .expect_err("keep is not at that version");
    assert_eq!((conflict.index, conflict.key.as_str()), (0, "keep"));
    assert_eq!(
        store.reap_expired_if("none", Cond::Absent).expect("reaps"),
        Ok(1),
        "the due row goes under a guard that holds"
    );
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
// missing) until the tables are back, and the failed connection attempts leave no slot
// taken (a pool of two would block for good on a leaked slot), so the store works again
// once they are.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
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
    let again = PostgresStore::open(&config).expect("creates the tables again");
    again.txn(put("k")).expect("store").expect("commits");
    assert!(store.get("k").expect("recovered").is_some());
}

// Catches a commit the server refuses (a deferred constraint fails at COMMIT) being
// reported as success, a session that survived a refusal being thrown away (a refusal
// leaves the connection usable), and a refusal that leaves the row behind.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
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
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
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
