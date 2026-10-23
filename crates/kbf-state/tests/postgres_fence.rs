//! The fence on a real Postgres server: every generic fencing case, and the races the
//! fence exists to close, with a transaction held open on the server so the interleaving
//! is the one under test and not a lucky schedule.
//!
//! The tests need a server (`KBF_TEST_POSTGRES_URL` names it) and fail without one. Waits are
//! on conditions (a connection waiting on a lock), never on elapsed time.
#![cfg(feature = "postgres")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use kbf_state::conformance::fence;
use kbf_state::postgres::{Flaw, PostgresBackend, PostgresStore};
use kbf_state::{Cond, Fenced, GENERATION, SHARED_WRITERS, Store, StoreError, Txn, share};
use postgres::{Client, NoTls};

fn url() -> String {
    kbf_testkit::database::server_url()
}

fn backend() -> PostgresBackend {
    PostgresBackend::new(&url(), Flaw::None)
}

macro_rules! on_postgres {
    ($($case:ident),+ $(,)?) => {
        $(
            #[test]
            fn $case() {
                fence::$case(&backend());
            }
        )+
    };
}

on_postgres!(
    f1_each_taker_holds_the_next_generation,
    f2_a_taker_that_expects_another_generation_is_refused,
    f3_concurrent_takers_never_hold_the_same_generation,
    f4_a_superseded_handle_fails_every_kind_of_write,
    f5_the_holder_still_sees_its_own_conflicts,
    f6_reads_pass_through,
    f7_a_write_superseded_before_it_lands_is_fenced,
    f8_a_reap_superseded_before_it_runs_is_fenced,
);

/// A store on a schema of its own, and a way to see its connections wait.
struct Rig {
    _backend: PostgresBackend,
    store: Arc<PostgresStore>,
    app: String,
    raw: Mutex<Client>,
}

impl Rig {
    fn new() -> Rig {
        let backend = backend();
        let config = backend.fresh_config();
        let app = config.schema().to_owned();
        let store = Arc::new(PostgresStore::open(&config).expect("opens"));
        let raw = Client::connect(&url(), NoTls).expect("connects");
        Rig {
            _backend: backend,
            store,
            app,
            raw: Mutex::new(raw),
        }
    }

    fn dynstore(&self) -> Arc<dyn Store> {
        Arc::clone(&self.store) as Arc<dyn Store>
    }

    /// Blocks until a connection of this store waits on a lock.
    fn wait_for_a_waiter(&self) {
        for _ in 0..6000 {
            let waiting: i64 = self
                .raw
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity \
                     WHERE application_name = $1 AND wait_event_type = 'Lock'",
                    &[&self.app],
                )
                .expect("the activity view is readable")
                .get(0);
            if waiting > 0 {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("no connection ever waited behind the held transaction");
    }

    /// The transaction a take of generation `next` runs, to hold open.
    fn take_txn(&self, current: u64, next: u64) -> Txn {
        let version = self
            .store
            .get(GENERATION)
            .expect("read")
            .expect("row")
            .version;
        assert_eq!(current + 1, next);
        Txn::new().put(GENERATION, next.to_be_bytes(), Cond::Version(version))
    }
}

// Catches a fence that is not atomic with the write: A's write transaction is open on the
// server (the generation checked, not committed), B's take must wait for it, A commits,
// and only then does B hold generation 2. Never are both writers live: A's next write
// fails Fenced.
#[test]
fn a_take_waits_for_the_holders_open_transaction() {
    let rig = Rig::new();
    let a = Fenced::take(rig.dynstore(), None).expect("A takes");
    let version = rig
        .store
        .get(GENERATION)
        .expect("read")
        .expect("row")
        .version;
    let held = Txn::new()
        .put("a/k", "v", Cond::Any)
        .check(GENERATION, Cond::Version(version));
    let pending = rig.store.begin(held).expect("A's transaction runs");
    let taken = AtomicBool::new(false);
    let b = thread::scope(|scope| {
        let b = scope.spawn(|| {
            let b = Fenced::take(rig.dynstore(), Some(1));
            taken.store(true, Ordering::SeqCst);
            b
        });
        rig.wait_for_a_waiter();
        assert!(
            !taken.load(Ordering::SeqCst),
            "B took the generation while A's transaction was open"
        );
        let results = pending.commit().expect("A commits");
        assert!(results.iter().all(Result::is_ok), "A's write committed");
        b.join().expect("join")
    })
    .expect("B takes once A has committed");
    assert_eq!(b.generation(), 2);
    assert_eq!(
        a.txn(Txn::new().put("a/k2", "late", Cond::Any))
            .unwrap_err(),
        StoreError::Fenced,
        "A wrote after B held the generation"
    );
    assert!(rig.store.get("a/k2").expect("read").is_none());
    assert!(rig.store.get("a/k").expect("read").is_some());
}

// Catches a fence checked outside the write's transaction: B's take is open (not
// committed) when A's write starts; A passes a plain read of the generation, waits on the
// server, B commits, and A's write must then fail Fenced and write nothing.
#[test]
fn a_write_that_waits_behind_a_take_is_fenced() {
    let rig = Rig::new();
    let a = Fenced::take(rig.dynstore(), None).expect("A takes");
    let pending = rig
        .store
        .begin(rig.take_txn(1, 2))
        .expect("B's take runs, uncommitted");
    let result = thread::scope(|scope| {
        let write = scope.spawn(|| a.txn(Txn::new().put("a/k", "stale", Cond::Any)));
        rig.wait_for_a_waiter();
        pending.commit().expect("B commits");
        write.join().expect("join")
    });
    assert_eq!(result.unwrap_err(), StoreError::Fenced);
    assert!(
        rig.store.get("a/k").expect("read").is_none(),
        "A's write landed"
    );
}

// Catches a reap that checks the generation in a read of its own and then reaps: B's take
// is open when A's reap starts, A's check read passes, A's reap waits, B commits, and the
// reap must then fail Fenced.
#[test]
fn a_reap_that_waits_behind_a_take_is_fenced() {
    let rig = Rig::new();
    let a = Fenced::take(rig.dynstore(), None).expect("A takes");
    let pending = rig
        .store
        .begin(rig.take_txn(1, 2))
        .expect("B's take runs, uncommitted");
    let result = thread::scope(|scope| {
        let reap = scope.spawn(|| a.reap_expired());
        rig.wait_for_a_waiter();
        pending.commit().expect("B commits");
        reap.join().expect("join")
    });
    assert_eq!(result, Err(StoreError::Fenced));
}

/// Shares the rig's store from eight threads at once, and checks that every call succeeded
/// and that the generation row holds the marker and nothing else.
fn share_together(rig: &Rig) {
    thread::scope(|scope| {
        let workers: Vec<_> = (0..8).map(|_| scope.spawn(|| share(&*rig.store))).collect();
        for worker in workers {
            worker.join().expect("join").expect("shares");
        }
    });
    let row = rig.store.get(GENERATION).expect("read").expect("a row");
    assert_eq!(row.value, SHARED_WRITERS);
}

/// Asserts that eight frontends that open an empty store together all succeed and agree: the
/// generation row ends holding exactly the shared marker.
#[test]
fn concurrent_shares_of_an_empty_store_leave_exactly_the_marker() {
    share_together(&Rig::new());
}

/// Asserts that eight frontends that open a store that holds a generation together all
/// succeed and agree: the row ends holding exactly the shared marker, and the handle that
/// held the generation is fenced.
#[test]
fn concurrent_shares_of_a_store_that_holds_a_generation_leave_exactly_the_marker() {
    let rig = Rig::new();
    let holder = Fenced::take(rig.dynstore(), None).expect("A takes");
    share_together(&rig);
    let late = holder.txn(Txn::new().put("a/k", "late", Cond::Any));
    assert_eq!(late.unwrap_err(), StoreError::Fenced);
    assert!(rig.store.get("a/k").expect("read").is_none());
}
