//! The holder's liveness announcement on Postgres: a session advisory lock on a
//! connection of its own, visible from every other session, released when the guard is
//! dropped and when the server loses the connection.
//!
//! The tests need a server and are `#[ignore]`d; the `state-postgres` job runs them with
//! `--include-ignored`. Waits are on the condition (the lock being released), polled with
//! a bound, never on a fixed delay.
#![cfg(feature = "postgres")]

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kbf_state::postgres::{Flaw, PostgresBackend, PostgresStore};
use kbf_state::{Fenced, GENERATION, Store, StoreError, TakeError};
use postgres::{Client, NoTls};

const URL_VAR: &str = "KBF_TEST_POSTGRES_URL";

fn url() -> String {
    std::env::var(URL_VAR).unwrap_or_else(|_| {
        panic!("{URL_VAR} must name a postgres:// URL the tests may create schemas in")
    })
}

/// A store, and a second store (another set of sessions) on the same schema, the way
/// another process sees it.
struct Pair {
    _backend: PostgresBackend,
    app: String,
    holder: Arc<PostgresStore>,
    observer: Arc<PostgresStore>,
}

impl Pair {
    fn new() -> Pair {
        let backend = PostgresBackend::new(&url(), Flaw::None);
        let config = backend.fresh_config();
        Pair {
            app: config.schema().to_owned(),
            holder: Arc::new(PostgresStore::open(&config).expect("opens")),
            observer: Arc::new(PostgresStore::open(&config).expect("opens again")),
            _backend: backend,
        }
    }
}

/// Polls until `holder_alive(generation)` is `want`, up to about 30 seconds.
fn becomes(store: &PostgresStore, generation: u64, want: bool) {
    for _ in 0..6000 {
        if store.holder_alive(generation).expect("asks") == want {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("holder_alive({generation}) never became {want}");
}

// Catches a liveness announcement that is not held across the connection (a lock that is
// taken and let go, or at transaction level): while the guard lives, another session
// sees the holder alive; with nothing announced it sees it dead.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_held_announcement_is_seen_from_another_session() {
    let pair = Pair::new();
    assert_eq!(
        pair.observer.holder_alive(1),
        Ok(false),
        "nothing announced"
    );
    let guard = pair.holder.hold_liveness(1).expect("announces");
    for _ in 0..3 {
        assert_eq!(pair.observer.holder_alive(1), Ok(true), "the guard lives");
    }
    assert_eq!(
        pair.holder.holder_alive(1),
        Ok(true),
        "the holder's own store sees it too"
    );
    drop(guard);
    becomes(&pair.observer, 1, false);
}

// Catches the process-death case being a guess: the server ends the guard's connection
// (as it does when the process dies) while the guard is still held, and the holder reads
// as dead. A pid probe or a lock held on a pooled connection would not follow.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn the_announcement_goes_when_the_connection_dies() {
    let pair = Pair::new();
    let _guard = pair.holder.hold_liveness(1).expect("announces");
    assert_eq!(pair.observer.holder_alive(1), Ok(true));
    let mut admin = Client::connect(&url(), NoTls).expect("connects");
    let ended: i64 = admin
        .query_one(
            "SELECT count(*) FROM (SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE application_name = $1) ended",
            &[&format!("{}-holder", pair.app)],
        )
        .expect("terminates")
        .get(0);
    assert_eq!(ended, 1, "exactly the guard's connection is ended");
    becomes(&pair.observer, 1, false);
}

// Catches two live holders of one generation: a second announcement of a generation that
// is announced is refused, and is accepted once the first is gone.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_generation_has_one_live_holder() {
    let pair = Pair::new();
    let first = pair.holder.hold_liveness(1).expect("announces");
    let refused = pair.observer.hold_liveness(1).expect_err("already held");
    assert!(
        matches!(refused, StoreError::Failed(why) if why.contains("already has a live holder"))
    );
    drop(first);
    becomes(&pair.observer, 1, false);
    let _second = pair.observer.hold_liveness(1).expect("free again");
}

// Catches announcements that collide: generations are announced independently of one
// another, and one schema's holder is not another schema's.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn generations_and_schemas_are_independent() {
    let pair = Pair::new();
    let other = Pair::new();
    let _one = pair.holder.hold_liveness(1).expect("announces 1");
    let _two = pair.holder.hold_liveness(2).expect("announces 2");
    assert_eq!(pair.observer.holder_alive(1), Ok(true));
    assert_eq!(pair.observer.holder_alive(2), Ok(true));
    assert_eq!(pair.observer.holder_alive(3), Ok(false));
    assert_eq!(other.observer.holder_alive(1), Ok(false), "another schema");
}

// Catches a take that announces after it writes, and one that leaves an announcement behind
// when it fails: take_holding announces generation 2 before the row says 2, a take whose
// generation a rival has already announced fails with the row unwritten, and the old
// holder's generation is still announced until its guard goes.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn take_holding_announces_before_it_writes() {
    let pair = Pair::new();
    let (first, first_guard) =
        Fenced::take_holding(Arc::clone(&pair.holder) as Arc<dyn Store>, None).expect("takes");
    assert_eq!(first.generation(), 1);
    assert_eq!(pair.observer.holder_alive(1), Ok(true));

    let rival = pair.observer.hold_liveness(2).expect("a rival announces 2");
    let row = pair.holder.get(GENERATION).expect("read");
    let refused = Fenced::take_holding(Arc::clone(&pair.holder) as Arc<dyn Store>, Some(1))
        .expect_err("held");
    assert!(matches!(refused, TakeError::Store(StoreError::Failed(_))));
    assert_eq!(pair.holder.get(GENERATION).expect("read"), row, "row moved");
    drop(rival);
    becomes(&pair.observer, 2, false);

    let (second, second_guard) =
        Fenced::take_holding(Arc::clone(&pair.holder) as Arc<dyn Store>, Some(1)).expect("takes");
    assert_eq!(second.generation(), 2);
    assert_eq!(pair.observer.holder_alive(2), Ok(true));
    assert_eq!(
        pair.observer.holder_alive(1),
        Ok(true),
        "the first holder is alive until its guard goes: a swap has two live holders"
    );
    drop(first_guard);
    becomes(&pair.observer, 1, false);
    assert_eq!(pair.observer.holder_alive(2), Ok(true));
    drop(second_guard);
}
