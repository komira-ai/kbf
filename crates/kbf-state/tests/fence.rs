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

/// A store that forwards the required methods only, so `reap_expired_if` is the default.
struct Plain(Fake);

impl Store for Plain {
    fn apply_batch(
        &self,
        batch: Vec<Txn>,
    ) -> Result<Vec<Result<kbf_state::Commit, kbf_state::Conflict>>, StoreError> {
        self.0.apply_batch(batch)
    }
    fn get(&self, key: &str) -> Result<Option<kbf_state::Versioned>, StoreError> {
        self.0.get(key)
    }
    fn scan_prefix(&self, p: &str) -> Result<Vec<(String, kbf_state::Versioned)>, StoreError> {
        self.0.scan_prefix(p)
    }
    fn scan_range(&self, s: kbf_state::ScanRange<'_>) -> Result<kbf_state::ScanPage, StoreError> {
        self.0.scan_range(s)
    }
    fn changes(
        &self,
        after: kbf_state::Revision,
        limit: usize,
    ) -> Result<kbf_state::Changes, StoreError> {
        self.0.changes(after, limit)
    }
    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.0.reap_expired()
    }
    fn commit_stats(&self) -> kbf_state::CommitStats {
        self.0.commit_stats()
    }
    fn now(&self) -> kbf_state::StoreTime {
        self.0.now()
    }
}

// Catches the default conditional reap judging a precondition wrongly: each kind of
// precondition, met and unmet, reaps (or refuses) as the transaction rule says.
#[test]
fn the_default_conditional_reap_judges_each_precondition() {
    let clock = Arc::new(ManualClock::default());
    let store = Plain(Fake::with_flaw(clock, Flaw::None));
    let revision = store
        .txn(Txn::new().put("k", "v", Cond::Any))
        .expect("s")
        .expect("c")
        .revision;
    let version = kbf_state::Version(revision.0);
    let refused = [
        ("k", Cond::Absent),
        ("k", Cond::Version(kbf_state::Version(version.0 + 1))),
        ("none", Cond::Version(version)),
    ];
    for (key, cond) in refused {
        let conflict = store.reap_expired_if(key, cond).expect("s").unwrap_err();
        assert_eq!((conflict.index, conflict.key.as_str()), (0, key));
    }
    for (key, cond) in [
        ("k", Cond::Any),
        ("k", Cond::Version(version)),
        ("none", Cond::Absent),
    ] {
        assert_eq!(store.reap_expired_if(key, cond), Ok(Ok(0)));
    }
}

/// A store with the default conditional reap, and a hook that runs just before its plain
/// reap: where a take can land between the default's read and its reap.
struct Gapped {
    inner: Arc<dyn Store>,
    hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Store for Gapped {
    fn apply_batch(
        &self,
        batch: Vec<Txn>,
    ) -> Result<Vec<Result<kbf_state::Commit, kbf_state::Conflict>>, StoreError> {
        self.inner.apply_batch(batch)
    }
    fn get(&self, key: &str) -> Result<Option<kbf_state::Versioned>, StoreError> {
        self.inner.get(key)
    }
    fn scan_prefix(&self, p: &str) -> Result<Vec<(String, kbf_state::Versioned)>, StoreError> {
        self.inner.scan_prefix(p)
    }
    fn scan_range(&self, s: kbf_state::ScanRange<'_>) -> Result<kbf_state::ScanPage, StoreError> {
        self.inner.scan_range(s)
    }
    fn changes(
        &self,
        after: kbf_state::Revision,
        limit: usize,
    ) -> Result<kbf_state::Changes, StoreError> {
        self.inner.changes(after, limit)
    }
    fn reap_expired(&self) -> Result<usize, StoreError> {
        let hook = self.hook.lock().expect("lock").take();
        if let Some(hook) = hook {
            hook();
        }
        self.inner.reap_expired()
    }
    fn commit_stats(&self) -> kbf_state::CommitStats {
        self.inner.commit_stats()
    }
    fn now(&self) -> kbf_state::StoreTime {
        self.inner.now()
    }
}

// Documents the hazard of the trait default, so nobody relies on it: a `Fenced` handle
// over a store that does NOT override `reap_expired_if` is not atomic. A rival that takes
// the generation between the default's read and its reap does not stop the superseded
// handle's reap. This is why a production `Store` must override the method (the fake,
// SQLite and Postgres do; F8 shows the fenced result for them).
#[test]
fn a_store_that_keeps_the_default_conditional_reap_is_not_atomic() {
    let raw = fake();
    let gapped = Arc::new(Gapped {
        inner: Arc::clone(&raw),
        hook: std::sync::Mutex::default(),
    });
    let old = Fenced::take(gapped.clone(), None).expect("take");
    let rival = Arc::clone(&raw);
    *gapped.hook.lock().expect("lock") = Some(Box::new(move || {
        Fenced::take(rival, Some(1)).expect("the rival takes");
    }));
    assert!(
        old.reap_expired().is_ok(),
        "the default is read-then-reap: the superseded handle's reap went through"
    );
    assert_eq!(
        Fenced::take(raw, Some(2))
            .expect("the rival held 2")
            .generation(),
        3,
        "the rival did take generation 2 in the gap"
    );
}

// Catches a guarded reap that judges the precondition but does not reap, or reaps but
// does not make it durable: with the precondition met it removes the expired key and
// writes its `Expire` row; with it unmet it removes nothing.
#[test]
fn a_guarded_reap_removes_what_expired_only_while_its_precondition_holds() {
    let clock = Arc::new(ManualClock::default());
    let store = Fake::with_flaw(clock.clone(), Flaw::None);
    store
        .txn(Txn::new().put("guard", "g", Cond::Any).put_ttl(
            "t",
            "v",
            std::time::Duration::from_secs(5),
            Cond::Any,
        ))
        .expect("s")
        .expect("c");
    clock.advance(std::time::Duration::from_secs(10));
    assert!(
        store
            .reap_expired_if("guard", Cond::Absent)
            .expect("s")
            .is_err()
    );
    assert_eq!(store.reap_expired_if("guard", Cond::Any), Ok(Ok(1)));
    let expired = store
        .changes(kbf_state::Revision::ZERO, 10)
        .expect("feed")
        .rows
        .iter()
        .filter(|row| row.event == kbf_state::Event::Expire)
        .count();
    assert_eq!(expired, 1, "the reap wrote its Expire row once");
}

// Catches a fenced handle that does not pass the liveness calls to the store underneath:
// both are answered by the store, for the generation asked about.
#[test]
fn a_fenced_handle_forwards_the_liveness_calls() {
    let store = Arc::new(Plain(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    )));
    let held = Fenced::take(store, None).expect("take");
    assert!(matches!(
        held.hold_liveness(1),
        Err(StoreError::Failed(why)) if why.contains("cannot announce")
    ));
    assert!(matches!(
        held.holder_alive(1),
        Err(StoreError::Failed(why)) if why.contains("cannot tell")
    ));
}

// Catches a second precondition being forwarded unchecked by a fenced handle: it cannot be
// checked in the same step as the generation, so it is refused and nothing is reaped.
#[test]
fn a_fenced_handle_refuses_a_second_reap_precondition() {
    let held = Fenced::take(fake(), None).expect("take");
    assert!(matches!(
        held.reap_expired_if("k", Cond::Any),
        Err(StoreError::Failed(_))
    ));
}

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
