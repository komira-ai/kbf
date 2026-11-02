//! The registry's storage: the write order, the generation, the audit, the secret files.
//! Each test names the defect it catches.

mod audit_unknown;
mod crash;
mod files;
mod leaks;
mod repoint;
mod rules;
mod view;

use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Op, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_testkit::dir::TestDir;
use kbf_types::object_store_api::{
    AuditOutcome, BindingPut, ConnectionName, ConnectionPut, Secret,
};

use super::{Registry, RegistryError};
use crate::enroll::Random;

pub(super) const SENTINEL: &str = "SENTINEL-s3-secret-9f8e7d6c5b4a";

/// Every draw is the next number, big-endian: the versions are 1, 2, 3, and a file name
/// can be predicted.
pub(super) struct Counting(AtomicU64);

impl Counting {
    pub(super) fn arc() -> Arc<dyn Random> {
        Self::from(1)
    }

    /// Draws `first`, `first + 1`, ...
    pub(super) fn from(first: u64) -> Arc<dyn Random> {
        Arc::new(Self(AtomicU64::new(first)))
    }
}

impl Random for Counting {
    fn fill(&self, buf: &mut [u8]) -> io::Result<()> {
        buf.copy_from_slice(&self.0.fetch_add(1, Ordering::SeqCst).to_be_bytes());
        Ok(())
    }
}

/// A source that cannot draw.
pub(super) struct NoRandom;

impl Random for NoRandom {
    fn fill(&self, _buf: &mut [u8]) -> io::Result<()> {
        Err(io::Error::other("no entropy"))
    }
}

/// A scratch directory, removed when it drops.
pub(super) fn scratch(label: &str) -> TestDir {
    crate::test_dir::scratch_dir(&format!("kbf-objstore-{label}"))
}

pub(super) fn fake() -> Arc<Fake> {
    let clock = Arc::new(ManualClock::default());
    clock.advance(std::time::Duration::from_secs(1_000));
    Arc::new(Fake::with_flaw(clock, Flaw::None))
}

pub(super) fn name(text: &str) -> ConnectionName {
    text.parse().expect("a connection name")
}

/// A PUT body for `secret`, everything else fixed.
pub(super) fn put(secret: &str) -> ConnectionPut {
    ConnectionPut {
        endpoint: "http://s3.example:9000".to_owned(),
        region: None,
        access_key_id: "AKID".to_owned(),
        secret_access_key: Secret::new(secret.to_owned()),
        conditional_put: None,
        min_put_rate: None,
        expect_generation: None,
        verify: None,
    }
}

pub(super) fn bind(connection: &str, bucket: &str) -> BindingPut {
    BindingPut {
        connection: name(connection),
        bucket: bucket.to_owned(),
        prefix: "farm/".to_owned(),
        expect_generation: None,
        verify: None,
    }
}

/// `Registry::open` with no sweep grace, so a test's leftovers are swept at once.
pub(super) fn open_now(
    store: Arc<dyn Store>,
    dir: &Path,
    random: Arc<dyn Random>,
) -> Result<Registry, RegistryError> {
    Registry::open_with_grace(store, dir, random, std::time::Duration::ZERO)
}

/// A registry over `store` with files in `dir`, and the same pair opened again after a
/// crash (the second call).
pub(super) fn open(store: &Arc<dyn Store>, dir: &Path) -> Registry {
    open_now(Arc::clone(store), dir, Counting::arc()).expect("open the registry")
}

pub(super) fn as_store(fake: &Arc<Fake>) -> Arc<dyn Store> {
    fake.clone()
}

/// The files in `dir`, sorted.
pub(super) fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("list")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

pub(super) fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

/// The file a connection's row names, as a path.
pub(super) fn file_of(dir: &Path, name: &str, version: u64) -> PathBuf {
    dir.join(format!("{name}.{version:016x}.json"))
}

/// A store that fails every transaction that appends an audit row, and passes the rest.
pub(super) struct FailsAudit(pub(super) Arc<dyn Store>);

impl Store for FailsAudit {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let audits = batch.iter().any(|txn| {
            txn.ops()
                .iter()
                .any(|op| matches!(op, Op::Put { key, .. } if key.starts_with("objstore/audit/")))
        });
        if audits {
            return Err(StoreError::Failed("injected".to_owned()));
        }
        self.0.apply_batch(batch)
    }
    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.0.get(key)
    }
    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.0.scan_prefix(prefix)
    }
    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.0.scan_range(scan)
    }
    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.0.changes(after, limit)
    }
    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.0.reap_expired()
    }
    fn commit_stats(&self) -> CommitStats {
        self.0.commit_stats()
    }
    fn now(&self) -> StoreTime {
        self.0.now()
    }
}

// Catches a first write that does not leave a row, a 0600 file, a 0700 directory, a
// generation of 1 and an audit row; and a read that returns another secret.
#[test]
fn a_first_put_leaves_a_row_a_private_file_and_one_audit_row() {
    let store = as_store(&fake());
    let root = scratch("first");
    let dir = root.join("secrets/objstore");
    let reg = open(&store, &dir);
    let written = reg.put_connection(&name("cold"), &put(SENTINEL)).unwrap();
    assert_eq!(written.generation, 1);
    assert_eq!(written.outcome, AuditOutcome::Created);
    assert_eq!(reg.generation().unwrap(), 1);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(listing(&dir), ["cold.0000000000000001.json"]);
    assert_eq!(mode(&file_of(&dir, "cold", 1)), 0o600);
    assert_eq!(reg.secret(&name("cold")).unwrap().expose(), SENTINEL);
    let info = reg.connection(&name("cold")).unwrap().unwrap();
    assert_eq!(
        (info.endpoint.as_str(), info.region.as_str()),
        ("http://s3.example:9000", "")
    );
    assert_eq!(
        (info.access_key_id.as_str(), info.conditional_put),
        ("AKID", false)
    );
    assert_eq!((info.min_put_rate, info.updated_generation), (None, 1));
    let audit = reg.audit(std::num::NonZeroUsize::new(10).unwrap()).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(
        (audit[0].seq, audit[0].at, audit[0].generation),
        (1, 1_000, 1)
    );
    assert_eq!(audit[0].subject, "cold");
    assert!(reg.connection(&name("other")).unwrap().is_none());
}

// Catches a same-content write that bumps the generation (nodes would refetch for
// nothing and "converged" would flap), writes a new file, or leaves no audit row.
#[test]
fn the_same_put_again_changes_nothing_but_the_audit_log() {
    let store = as_store(&fake());
    let root = scratch("same");
    let dir = root.join("s");
    let reg = open(&store, &dir);
    reg.put_connection(&name("cold"), &put(SENTINEL)).unwrap();
    let again = reg.put_connection(&name("cold"), &put(SENTINEL)).unwrap();
    assert_eq!(
        (again.generation, again.outcome),
        (1, AuditOutcome::Unchanged)
    );
    assert_eq!(reg.generation().unwrap(), 1);
    assert_eq!(listing(&dir), ["cold.0000000000000001.json"]);
    let audit = reg.audit(std::num::NonZeroUsize::new(10).unwrap()).unwrap();
    let outcomes: Vec<_> = audit
        .iter()
        .map(|a| (a.seq, a.outcome, a.generation))
        .collect();
    assert_eq!(
        outcomes,
        [
            (2, AuditOutcome::Unchanged, 1),
            (1, AuditOutcome::Created, 1)
        ],
        "newest first"
    );
    // A later change continues the sequence.
    let mut other = put(SENTINEL);
    other.region = Some("eu-1".to_owned());
    assert_eq!(
        reg.put_connection(&name("cold"), &other)
            .unwrap()
            .generation,
        2
    );
    assert_eq!(
        reg.audit(std::num::NonZeroUsize::new(1).unwrap()).unwrap()[0].seq,
        3
    );
}

// Catches a field change that writes a new secret file (the secret is the same: the
// version must not move) or one that does not bump the generation.
#[test]
fn a_change_that_keeps_the_secret_keeps_its_file() {
    let store = as_store(&fake());
    let root = scratch("keep");
    let dir = root.join("s");
    let reg = open(&store, &dir);
    reg.put_connection(&name("cold"), &put(SENTINEL)).unwrap();
    let mut changed = put(SENTINEL);
    changed.region = Some("eu-1".to_owned());
    changed.conditional_put = Some(true);
    changed.min_put_rate = std::num::NonZeroU64::new(4096);
    let written = reg.put_connection(&name("cold"), &changed).unwrap();
    assert_eq!(
        (written.generation, written.outcome),
        (2, AuditOutcome::Changed)
    );
    assert_eq!(listing(&dir), ["cold.0000000000000001.json"]);
    let info = reg.connection(&name("cold")).unwrap().unwrap();
    assert_eq!(info.region, "eu-1");
    assert!(info.conditional_put);
    assert_eq!(info.min_put_rate.map(std::num::NonZeroU64::get), Some(4096));
    assert_eq!(info.updated_generation, 2);
}

// Catches a rotation that edits the file in place (a crash would lose both keys), keeps
// the old version forever, or removes the new one.
#[test]
fn a_new_secret_is_a_new_version_and_the_old_one_goes_after_the_row() {
    let store = as_store(&fake());
    let root = scratch("rotate");
    let dir = root.join("s");
    let reg = open(&store, &dir);
    reg.put_connection(&name("cold"), &put("old-secret"))
        .unwrap();
    let written = reg
        .put_connection(&name("cold"), &put("new-secret"))
        .unwrap();
    assert_eq!(
        (written.generation, written.outcome),
        (2, AuditOutcome::Changed)
    );
    assert_eq!(listing(&dir), ["cold.0000000000000002.json"]);
    assert_eq!(reg.secret(&name("cold")).unwrap().expose(), "new-secret");
    assert!(matches!(
        reg.secret(&name("none")),
        Err(RegistryError::NoSuchConnection)
    ));
}

// Catches a missing compare-and-set on the stale generation an operator read.
#[test]
fn a_stale_expect_generation_writes_nothing() {
    let store = as_store(&fake());
    let root = scratch("expect");
    let dir = root.join("s");
    let reg = open(&store, &dir);
    reg.put_connection(&name("cold"), &put("one")).unwrap();
    let mut stale = put("two");
    stale.expect_generation = Some(0);
    let err = reg.put_connection(&name("cold"), &stale).unwrap_err();
    assert!(
        matches!(err, RegistryError::GenerationMismatch { current: 1 }),
        "{err}"
    );
    assert_eq!(err.to_string(), "the registry is at generation 1");
    assert_eq!(reg.secret(&name("cold")).unwrap().expose(), "one");
    assert_eq!(reg.generation().unwrap(), 1);
    assert_eq!(listing(&dir).len(), 1);
    assert_eq!(
        reg.audit(std::num::NonZeroUsize::new(9).unwrap())
            .unwrap()
            .len(),
        1
    );
    let mut current = put("two");
    current.expect_generation = Some(1);
    assert_eq!(
        reg.put_connection(&name("cold"), &current)
            .unwrap()
            .generation,
        2
    );
}

// Catches an audit row written in a transaction of its own: with the audit failing, the
// row (and the generation) must not exist, and the file written for it must be gone.
#[test]
fn the_row_and_its_audit_commit_together_or_not_at_all() {
    let inner = as_store(&fake());
    let root = scratch("atomic");
    let dir = root.join("s");
    let failing: Arc<dyn Store> = Arc::new(FailsAudit(Arc::clone(&inner)));
    let reg = open(&failing, &dir);
    let err = reg
        .put_connection(&name("cold"), &put(SENTINEL))
        .unwrap_err();
    assert!(
        matches!(err, RegistryError::Store(StoreError::Failed(_))),
        "{err}"
    );
    assert!(
        inner.scan_prefix("objstore/").unwrap().is_empty(),
        "no row, no audit, no generation"
    );
    assert!(
        listing(&dir).is_empty(),
        "the file of the refused write is removed"
    );
}

// Catches two writers of one process interleaving a file write and a row commit: both
// calls succeed one after the other, and exactly the winner's file is left.
#[test]
fn concurrent_puts_serialize_and_leave_one_file() {
    let store = as_store(&fake());
    let root = scratch("threads");
    let dir = root.join("s");
    let reg = Arc::new(open(&store, &dir));
    let handles: Vec<_> = ["alpha", "beta", "gamma", "delta"]
        .into_iter()
        .map(|secret| {
            let reg = Arc::clone(&reg);
            std::thread::spawn(move || reg.put_connection(&name("cold"), &put(secret)).unwrap())
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(reg.generation().unwrap(), 4);
    assert_eq!(listing(&dir).len(), 1);
    let live = reg.secret(&name("cold")).unwrap();
    assert!(["alpha", "beta", "gamma", "delta"].contains(&live.expose()));
}

// Catches a registry that cannot draw a version going on to write.
#[test]
fn a_failing_random_source_writes_nothing() {
    let store = as_store(&fake());
    let root = scratch("norandom");
    let dir = root.join("s");
    let reg = open_now(Arc::clone(&store), &dir, Arc::new(NoRandom)).unwrap();
    let err = reg.put_connection(&name("cold"), &put("x")).unwrap_err();
    assert!(matches!(err, RegistryError::Secret(_)), "{err}");
    assert!(err.to_string().contains("draw a version"), "{err}");
    assert!(store.scan_prefix("objstore/").unwrap().is_empty());
}
