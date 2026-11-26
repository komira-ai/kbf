//! The sweep's gate of the put directories open operations name ([`SweepGates::named`]): a key in
//! one is kept whatever its age, and the gate is read once a page, after the pack index's answer
//! and before the page's deletes.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use kbf_pack::{
    NamedOf, Slot, SweepConfig, SweepError, SweepGates, SweepReport, Sweeper, TaskPackKey,
    task_key_age,
};
use kbf_pack_index::{BlobDigest, NewPack, PackBlob, PackIndex};

use crate::support::{FaultyStore, Rig};

/// The minute the sweeps of these tests run at.
const NOW_MINUTE: u32 = 100_000;
/// Past the default grace of six hours: 400 minutes old.
const OLD: u32 = NOW_MINUTE - 400;
/// Inside the grace: 100 minutes old.
const YOUNG: u32 = NOW_MINUTE - 100;

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(NOW_MINUTE) * 60)
}

/// The object `slot` of the put directory of `build` minted at `minute`, with nonce byte `n`.
fn in_dir(build: &str, n: u8, minute: u32, slot: Slot) -> String {
    TaskPackKey::in_put_dir(build, [n; 16], minute, slot)
        .expect("a key")
        .to_string()
}

/// The put directory `key` lies in: what an open operation's row names.
fn dir_of(key: &str) -> String {
    key.rsplit_once('/')
        .expect("a put directory key")
        .0
        .to_owned()
}

/// The named gate that answers what `named` holds when asked, and counts each question.
struct Named {
    dirs: Arc<Mutex<Result<BTreeSet<String>, String>>>,
    asked: Arc<AtomicU32>,
}

impl Named {
    fn of(dirs: &[String]) -> Self {
        Self {
            dirs: Arc::new(Mutex::new(Ok(dirs.iter().cloned().collect()))),
            asked: Arc::default(),
        }
    }

    fn gate(&self) -> NamedOf {
        let (dirs, asked) = (Arc::clone(&self.dirs), Arc::clone(&self.asked));
        Arc::new(move || {
            asked.fetch_add(1, Ordering::SeqCst);
            dirs.lock().unwrap().clone()
        })
    }

    fn set(&self, now: Result<BTreeSet<String>, String>) {
        *self.dirs.lock().unwrap() = now;
    }

    fn asked(&self) -> u32 {
        self.asked.load(Ordering::SeqCst)
    }
}

fn sweeper(rig: &Rig, named: &Named, config: SweepConfig) -> Sweeper<FaultyStore> {
    let gates = SweepGates {
        age_of: Box::new(task_key_age),
        is_leader: Box::new(|| true),
        verifying: Box::new(|_| false),
        claim: Box::new(|_| Some(Box::new(()))),
        named: named.gate(),
    };
    let index: Arc<dyn PackIndex> = rig.index.clone();
    Sweeper::new(Arc::clone(&rig.store), index, config, gates)
}

/// Asserts that the old objects of a put directory an open operation names are all kept and
/// counted `named`, while the old object of a directory no operation names is deleted; and that
/// once the operation is closed (its directory no longer named) the next sweep deletes them.
#[tokio::test]
async fn an_old_key_an_open_operation_names_is_kept_until_the_operation_closes() {
    let rig = Rig::new();
    let named_pack = in_dir("lease1", 1, OLD, Slot::Pack(0));
    let named_large = in_dir("lease1", 1, OLD, Slot::Large(2));
    let orphan = in_dir("lease1", 2, OLD, Slot::Pack(0));
    for key in [&named_pack, &named_large, &orphan] {
        rig.store.seed(key, b"bytes").await;
    }
    let named = Named::of(&[dir_of(&named_pack)]);
    let sweep = sweeper(&rig, &named, SweepConfig::default());

    let report = sweep.sweep(now()).await.unwrap();

    assert!(rig.store.holds(&named_pack).await);
    assert!(rig.store.holds(&named_large).await);
    assert!(!rig.store.holds(&orphan).await);
    assert_eq!((report.named, report.deleted), (2, 1));
    assert_eq!(named.asked(), 1, "one question for the one page");

    named.set(Ok(BTreeSet::new()));
    let report = sweep.sweep(now()).await.unwrap();
    assert!(rig.store.keys_under("cas/b/").await.is_empty());
    assert_eq!((report.named, report.deleted), (0, 2));
}

/// Asserts that only an object of a named put directory is named: a key of the presigned layout
/// under the same build is of no put directory, so a set that names its build (not a put
/// directory) keeps nothing, and a put directory of another nonce under that build is not named.
#[tokio::test]
async fn only_an_object_of_a_named_put_directory_is_named() {
    let rig = Rig::new();
    let named_key = in_dir("lease1", 1, OLD, Slot::Pack(0));
    let other_nonce = in_dir("lease1", 3, OLD, Slot::Pack(0));
    let presigned = TaskPackKey::new("lease1", [1; 16], OLD, Slot::Pack(0))
        .expect("a key")
        .to_string();
    assert!(presigned.starts_with("cas/b/lease1/"), "{presigned}");
    for key in [&named_key, &other_nonce, &presigned] {
        rig.store.seed(key, b"bytes").await;
    }
    let named = Named::of(&[dir_of(&named_key), "cas/b/lease1".to_owned()]);
    let report = sweeper(&rig, &named, SweepConfig::default())
        .sweep(now())
        .await
        .unwrap();
    assert!(rig.store.holds(&named_key).await);
    assert!(!rig.store.holds(&other_nonce).await);
    assert!(!rig.store.holds(&presigned).await);
    assert_eq!((report.named, report.deleted), (1, 2));
}

/// Asserts that a dry run counts a named key as named and not as one it would delete.
#[tokio::test]
async fn a_dry_run_does_not_count_a_named_key_as_one_it_would_delete() {
    let rig = Rig::new();
    let named_key = in_dir("lease1", 1, OLD, Slot::Pack(0));
    let orphan = in_dir("lease1", 2, OLD, Slot::Pack(0));
    for key in [&named_key, &orphan] {
        rig.store.seed(key, b"bytes").await;
    }
    let named = Named::of(&[dir_of(&named_key)]);
    let config = SweepConfig {
        dry_run: true,
        ..SweepConfig::default()
    };
    let report = sweeper(&rig, &named, config).sweep(now()).await.unwrap();
    assert_eq!(rig.store.keys_under("cas/b/").await.len(), 2);
    assert_eq!(
        (report.named, report.would_delete, report.deleted),
        (1, 1, 0)
    );
}

/// Asserts that when the open operations cannot be read the sweep stops with `Named` before any
/// delete of the page, and the report still says what it listed.
#[tokio::test]
async fn unreadable_operations_stop_the_sweep_before_its_deletes() {
    let rig = Rig::new();
    let orphan = in_dir("lease1", 2, OLD, Slot::Pack(0));
    rig.store.seed(&orphan, b"bytes").await;
    let named = Named::of(&[]);
    named.set(Err("a row of a later release".to_owned()));
    let sweep = sweeper(&rig, &named, SweepConfig::default());

    let mut report = SweepReport::default();
    let error = sweep.sweep_into(now(), &mut report).await;

    let Err(error @ SweepError::Named(_)) = error else {
        panic!("{error:?}");
    };
    assert_eq!(
        error.to_string(),
        "sweep: the open operations could not be read: a row of a later release"
    );
    assert!(rig.store.holds(&orphan).await);
    assert_eq!(rig.store.deletes(), 0);
    assert_eq!((report.listed, report.deleted), (1, 0));
}

/// Asserts that a page with no key the index leaves unrecorded asks nothing of the operations:
/// an empty listing, a young key (never a candidate), and an old key the index records (a
/// candidate the index's answer leaves no orphan of).
#[tokio::test]
async fn a_page_with_no_orphan_asks_nothing_of_the_operations() {
    let rig = Rig::new();
    let named = Named::of(&[]);
    let sweep = sweeper(&rig, &named, SweepConfig::default());
    sweep.sweep(now()).await.unwrap();
    rig.store
        .seed(&in_dir("lease1", 1, YOUNG, Slot::Pack(0)), b"young")
        .await;
    let report = sweep.sweep(now()).await.unwrap();
    assert_eq!(report.young, 1);
    assert_eq!(named.asked(), 0);

    let indexed = in_dir("lease1", 2, OLD, Slot::Pack(0));
    rig.store.seed(&indexed, b"indexed").await;
    rig.index.record_pack(&recorded(&indexed)).unwrap();
    let report = sweep.sweep(now()).await.unwrap();
    assert_eq!((report.referenced, report.deleted), (1, 0));
    assert_eq!(named.asked(), 0);
    assert!(rig.store.holds(&indexed).await);
}

/// A pack of one blob recorded under `key`.
fn recorded(key: &str) -> NewPack {
    NewPack {
        key: key.to_owned(),
        writer: "f/fe".to_owned(),
        size: 100,
        blobs: vec![PackBlob {
            digest: BlobDigest {
                hash: [7; 32],
                size: 10,
            },
            offset: 0,
        }],
    }
}
