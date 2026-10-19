//! Writers that queue behind the commit head on a real Postgres server: each waits its
//! turn and then commits, and none is rolled back and run again.
//!
//! The batch is a `SERIALIZABLE` transaction whose first statement locks the head row. A
//! transaction that waits on that row has its snapshot from before the wait, so when the
//! holder commits it is told "could not serialize access due to concurrent update" and the
//! store runs it again, behind the same queue. With several writers every waiter fails
//! every round but the one that is lucky enough to take the lock, and a takeover that
//! competes with a busy writer can use up its attempts. The store therefore queues on a
//! table lock before its snapshot is taken.
//!
//! The test is `#[ignore]`d with the rest of the server tests; the `state-postgres` job
//! runs it with `--include-ignored`. It holds a transaction open on the server, so the
//! interleaving is the one under test and not a lucky schedule, and it waits on a
//! condition (the waiters are queued), never on elapsed time.
#![cfg(feature = "postgres")]

use std::sync::Arc;
use std::thread;

use kbf_state::postgres::{Flaw, PostgresBackend, PostgresStore};
use kbf_state::{Cond, Store, Txn};
use postgres::{Client, NoTls};

const URL_VAR: &str = "KBF_TEST_POSTGRES_URL";
const WAITERS: usize = 4;

fn url() -> String {
    std::env::var(URL_VAR).unwrap_or_else(|_| {
        panic!("{URL_VAR} must name a postgres:// URL the tests may create schemas in")
    })
}

// Catches a batch that waits for the commit head inside a snapshot that predates the wait:
// A holds the head with a write in flight, four writers queue behind it, A commits. Every
// writer must then commit on its first attempt. With the head row as the only lock, all
// four are rolled back for a serialization failure and the store's retry counter is not 0.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn writers_queued_behind_the_head_commit_without_a_retry() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let config = backend.fresh_config();
    let app = config.schema().to_owned();
    let store = Arc::new(PostgresStore::open(&config).expect("the test database opens"));
    let mut monitor = Client::connect(&url(), NoTls).expect("a monitoring connection");

    let held = store
        .begin(Txn::new().put("held", "1", Cond::Any))
        .expect("A's write runs and holds the head");
    let writers: Vec<_> = (0..WAITERS)
        .map(|w| {
            let store = Arc::clone(&store);
            thread::spawn(move || store.txn(Txn::new().put(format!("w/{w}"), "v", Cond::Any)))
        })
        .collect();
    let mut polls = 0_u32;
    loop {
        let waiting: i64 = monitor
            .query_one(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE application_name = $1 AND wait_event_type = 'Lock'",
                &[&app],
            )
            .expect("the activity view is readable")
            .get(0);
        if waiting == i64::try_from(WAITERS).expect("small") {
            break;
        }
        polls += 1;
        assert!(polls < 2_000_000, "only {waiting} writers ever queued");
        thread::yield_now();
    }
    assert_eq!(
        store.retries(),
        0,
        "nothing has been rolled back while queued"
    );
    held.commit().expect("A commits");

    let mut revisions = Vec::new();
    for writer in writers {
        let commit = writer
            .join()
            .expect("no panic")
            .expect("the store works")
            .expect("no precondition");
        revisions.push(commit.revision);
    }
    revisions.sort();
    revisions.dedup();
    assert_eq!(
        revisions.len(),
        WAITERS,
        "each writer has a revision of its own"
    );
    assert_eq!(
        store.retries(),
        0,
        "a writer that waited its turn was rolled back and run again"
    );
    assert_eq!(store.scan_prefix("w/").expect("scan").len(), WAITERS);
}

// Catches a head lock strong enough to stop a pool from growing: preparing the store's
// statements on the head table needs `ROW EXCLUSIVE` on it, so with a stronger table lock a
// new connection could not be made while a writer is in flight. A holds the head with a
// write in flight on the store's only connection; a read needs a second one, which the
// pool must open. The server's lock timeout turns a connection that would wait into an
// error (3 s: the bound of a failure, not something the passing test waits for).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_connection_opens_while_a_writer_holds_the_head() {
    let base = url();
    let sep = if base.contains('?') { '&' } else { '?' };
    let timed = format!("{base}{sep}options=-c%20lock_timeout%3D3000");
    let backend = PostgresBackend::new(&timed, Flaw::None);
    let config = backend.fresh_config();
    let store = PostgresStore::open(&config).expect("the test database opens");
    let held = store
        .begin(Txn::new().put("held", "1", Cond::Any))
        .expect("A's write runs and holds the head");

    assert_eq!(
        store
            .get("held")
            .expect("a new connection opens and reads past the holder"),
        None
    );
    held.commit().expect("A commits");
    assert!(store.get("held").expect("reads").is_some());
}
