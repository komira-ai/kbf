//! `share` on a real Postgres server: frontends that open one store together all succeed and
//! agree on the marker.
//!
//! The tests need a server (`KBF_TEST_POSTGRES_URL` names it) and fail without one.
#![cfg(feature = "postgres")]

use std::thread;

use kbf_state::postgres::{Flaw, PostgresBackend, PostgresStore};
use kbf_state::{Cond, GENERATION, SHARED_WRITERS, Store, Txn, share};

fn url() -> String {
    kbf_testkit::database::server_url()
}

/// A store on a schema of its own.
struct Rig {
    _backend: PostgresBackend,
    store: PostgresStore,
}

impl Rig {
    fn new() -> Rig {
        let backend = PostgresBackend::new(&url(), Flaw::None);
        let store = PostgresStore::open(&backend.fresh_config()).expect("opens");
        Rig {
            _backend: backend,
            store,
        }
    }
}

/// Shares the rig's store from eight threads at once, and checks that every call succeeded
/// and that the generation row holds the marker and nothing else.
fn share_together(rig: &Rig) {
    thread::scope(|scope| {
        let workers: Vec<_> = (0..8).map(|_| scope.spawn(|| share(&rig.store))).collect();
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
/// succeed and agree: the row ends holding exactly the shared marker, written over the
/// generation.
#[test]
fn concurrent_shares_of_a_store_that_holds_a_generation_leave_exactly_the_marker() {
    let rig = Rig::new();
    rig.store
        .txn(Txn::new().put(GENERATION, 1_u64.to_be_bytes(), Cond::Any))
        .expect("store")
        .expect("commits");
    let before = rig.store.get(GENERATION).expect("read").expect("a row");
    share_together(&rig);
    let row = rig.store.get(GENERATION).expect("read").expect("a row");
    assert_ne!(
        row.version, before.version,
        "the generation was not replaced"
    );
}
