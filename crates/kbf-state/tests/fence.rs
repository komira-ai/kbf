//! Writer fencing: the generation row, the handle that checks it on every write, and
//! `demote` handing a SQLite file to a second process's store.
//!
//! Every test names the defect it catches.

mod common;

use std::sync::Arc;

use common::{SqliteBackend, scratch};
use kbf_state::conformance::fence;
use kbf_state::fake::{Fake, FakeBackend, Flaw};
use kbf_state::sqlite::{Options, SqliteError, SqliteStore};
use kbf_state::{Clock, Cond, Fenced, ManualClock, Store, StoreError, Txn};

fn fake() -> Arc<dyn Store> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

fn open(dir: &std::path::Path) -> Result<SqliteStore, SqliteError> {
    let clock: Arc<dyn Clock> = Arc::new(kbf_state::SystemClock::default());
    SqliteStore::open(&dir.join("state.db"), clock, Options::default())
}

// The generic cases (kbf_state::conformance::fence) on the fake and on SQLite; the Postgres
// run is postgres_fence.rs. Each case names the defect it catches.
macro_rules! on_each_backend {
    ($($case:ident),+ $(,)?) => {
        mod on_the_fake {
            use super::*;
            $(#[test] fn $case() {
                fence::$case(&FakeBackend::new(Flaw::None));
            })+
        }
        mod on_sqlite {
            use super::*;
            $(#[test] fn $case() {
                fence::$case(&SqliteBackend::new("fence"));
            })+
        }
    };
}

on_each_backend!(
    f1_each_taker_holds_the_next_generation,
    f2_a_taker_that_expects_another_generation_is_refused,
    f3_concurrent_takers_never_hold_the_same_generation,
    f4_a_superseded_handle_fails_every_kind_of_write,
    f5_the_holder_still_sees_its_own_conflicts,
    f6_reads_pass_through,
    f7_a_write_superseded_before_it_lands_is_fenced,
    f8_a_reap_superseded_before_it_runs_is_fenced,
);

// Catches a fake that grows a writer role it does not have: the fake cannot demote.
#[test]
fn the_fake_has_no_writer_role_to_give_up() {
    let held = Fenced::take(fake(), None).expect("take");
    assert!(held.demote().is_err());
}

// Catches a demote that leaves the lock or the writer thread behind: after it a second
// open of the file succeeds, the old handle still reads, and its writes fail Fenced.
#[test]
fn demote_frees_the_file_for_a_second_store_and_keeps_reads() {
    let dir = scratch("demote");
    let old = open(&dir).expect("open");
    old.txn(Txn::new().put("k", "v", Cond::Any))
        .expect("s")
        .expect("c");
    assert!(
        matches!(open(&dir), Err(SqliteError::Locked { .. })),
        "the lock is held until the demote"
    );
    old.demote().expect("demote");
    old.demote().expect("demoting twice is harmless");
    let new = open(&dir).expect("the file is free");
    new.txn(Txn::new().put("k2", "v2", Cond::Any))
        .expect("s")
        .expect("c");
    assert_eq!(old.get("k").expect("read").expect("row").value, b"v");
    assert_eq!(
        old.get("k2").expect("read").expect("row").value,
        b"v2",
        "the old handle sees the new writer's rows"
    );
    assert_eq!(
        old.txn(Txn::new().put("k3", "x", Cond::Any)).unwrap_err(),
        StoreError::Fenced
    );
    assert_eq!(old.reap_expired().unwrap_err(), StoreError::Fenced);
    assert_eq!(new.get("k3").expect("read"), None);
}

// Catches a demote that drops a write already acknowledged to its caller or still queued
// behind the writer: calls made before it complete, and are on disk for the successor.
#[test]
fn writes_made_before_a_demote_are_on_disk_for_the_next_store() {
    let dir = scratch("demote-writes");
    let old = Arc::new(open(&dir).expect("open"));
    let writers: Vec<_> = (0..16)
        .map(|n| {
            let old = Arc::clone(&old);
            std::thread::spawn(move || old.txn(Txn::new().put(format!("w{n}"), "v", Cond::Any)))
        })
        .collect();
    old.demote().expect("demote");
    let mut acknowledged = Vec::new();
    for (n, writer) in writers.into_iter().enumerate() {
        match writer.join().expect("join") {
            Ok(Ok(_)) => acknowledged.push(n),
            Err(StoreError::Fenced) => {}
            other => panic!("neither committed nor fenced: {other:?}"),
        }
    }
    let new = open(&dir).expect("open");
    for n in acknowledged {
        assert!(new.get(&format!("w{n}")).expect("read").is_some(), "w{n}");
    }
}

// Catches: a handle that cannot be printed (it is held in structs that derive Debug) or
// that prints without saying which generation it holds.
#[test]
fn a_handle_prints_its_generation() {
    let handle = Fenced::take(fake(), None).expect("take");
    let text = format!("{handle:?}");
    assert!(
        text.contains("Fenced") && text.contains("generation: 1"),
        "{text}"
    );
}
