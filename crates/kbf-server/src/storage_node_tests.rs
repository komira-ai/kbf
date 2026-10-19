//! A registered node's cache ([`open_registered`]): its keys across a wiped metadata
//! directory, and what it refuses before it opens.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use kbf_front::{ObjectKeys, VerifiedBlob};
use kbf_log::MachineVersion;
use kbf_meta::Retention;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

use super::{one_voter, open_among, open_registered, prefix_root};
use crate::RaftMetaLog;
use crate::join::Joined;

/// One bucket for every start of a test, which records the keys written to it.
#[derive(Clone, Default)]
struct Bucket(Arc<(Mutex<Vec<String>>, BucketStore)>);

struct BucketStore(MemoryStore);

impl Default for BucketStore {
    /// A bucket that refuses a key twice, as `conditional-put=true` does.
    fn default() -> Self {
        Self(MemoryStore::new(Capabilities {
            conditional_put: true,
            object_lock: false,
        }))
    }
}

impl Bucket {
    fn written(&self) -> Vec<String> {
        self.0
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl ObjectStore for Bucket {
    fn capabilities(&self) -> Capabilities {
        self.0.1.0.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(key.as_str().to_owned());
        self.0.1.0.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.0.1.0.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.1.0.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.1.0.list(prefix, after, max_keys).await
    }
}

fn joined(member_id: u64, boot: u64) -> Joined {
    Joined {
        member_id,
        boot,
        interval: Duration::from_secs(1),
        generation: 1,
        objstore_generation: 0,
    }
}

/// A fresh, empty metadata directory for `name`.
fn meta_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-node-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A start of a registered node over a new log in `dir`, as [`super::assemble`] opens
/// it: the keys from the log's prefix, then [`open_registered`]. Writes `n` blobs one at
/// a time and returns the keys written.
async fn start_and_write(dir: &Path, bucket: &Bucket, who: &Joined, n: usize) -> Vec<String> {
    let before = bucket.written().len();
    let meta = RaftMetaLog::open_dir(dir, Retention::default(), "kbf/")
        .await
        .expect("open the log");
    let prefix = meta.store_prefix();
    let keys = ObjectKeys {
        root: prefix_root(&prefix, meta.log_id()).expect("a root"),
        legacy: prefix,
    };
    let cache = open_registered(meta, bucket.clone(), keys, who)
        .await
        .expect("open as the registration");
    for i in 0..n {
        let text = format!("boot {} blob {i}", who.boot);
        let blob = VerifiedBlob::hashed(Bytes::from(text));
        cache.store_blobs(vec![blob]).await.expect("store");
    }
    bucket.written()[before..].to_vec()
}

/// Catches: a node whose keys repeat after its metadata directory is wiped (the new log
/// starts its epochs, and any counter it keeps, from 1 again). The node registers
/// (member 7, boot 1), writes, loses its directory, registers again (the state store
/// gives boot 2) and writes into the same conditional bucket: every write succeeds and
/// the two key sets are disjoint, under the root, not the logs' prefixes. Mutant seen
/// red: the writer's boot taken from a fresh local `AllocEpoch` (both starts name
/// boot 1, and the second start's first PUT is refused).
#[tokio::test]
async fn a_wiped_node_never_reuses_its_keys() {
    let bucket = Bucket::default();
    let (first_dir, wiped_dir) = (meta_dir("first"), meta_dir("wiped"));
    let first = start_and_write(&first_dir, &bucket, &joined(7, 1), 3).await;
    let second = start_and_write(&wiped_dir, &bucket, &joined(7, 2), 3).await;
    let first_set: BTreeSet<&String> = first.iter().collect();
    assert_eq!((first.len(), second.len()), (3, 3), "{first:?} {second:?}");
    assert!(
        second.iter().all(|k| !first_set.contains(k)),
        "{first:?} {second:?}"
    );
    for (keys, prefix) in [
        (&first, "kbf/cas/0000000000000007/0000000000000001/"),
        (&second, "kbf/cas/0000000000000007/0000000000000002/"),
    ] {
        assert!(keys.iter().all(|k| k.starts_with(prefix)), "{keys:?}");
    }
    for dir in [first_dir, wiped_dir] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Catches: a registration of member or boot zero taken (no `NodeWriter` can name it,
/// and a default-filled answer would make every node one writer), a log left below the
/// version that takes `SetWriter`, and a second start as a boot the log has set. Each
/// refusal comes before anything is written.
#[tokio::test]
async fn a_registration_the_log_cannot_take_refuses_the_open() {
    let dir = meta_dir("refusals");
    let bucket = Bucket::default();
    for bad in [joined(0, 1), joined(7, 0)] {
        let meta = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
            .await
            .expect("open the log");
        let keys = ObjectKeys::under(meta.store_prefix());
        let refused = open_registered(meta, bucket.clone(), keys, &bad)
            .await
            .err();
        let refused = refused.map(|e| e.to_string()).unwrap_or_default();
        assert!(refused.contains("neither may be zero"), "{refused}");
    }
    let meta = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
        .await
        .expect("open the log");
    assert_eq!(
        meta.machine_version(),
        MachineVersion::V1,
        "a refusal raised nothing"
    );
    let keys = ObjectKeys::under(meta.store_prefix());
    let cache = open_registered(meta, bucket.clone(), keys, &joined(7, 3))
        .await
        .expect("open");
    drop(cache);
    let meta = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
        .await
        .expect("open the log again");
    assert_eq!(meta.machine_version(), MachineVersion::V4);
    let keys = ObjectKeys::under(meta.store_prefix());
    let refused = open_registered(meta, bucket.clone(), keys, &joined(7, 3))
        .await
        .err();
    let refused = refused.map(|e| e.to_string()).unwrap_or_default();
    assert!(refused.contains("is not after boot 3"), "{refused}");
    assert!(bucket.written().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

/// Catches a stale boot from a real [`open_registered`] (the frontend's state store
/// restored from an older copy, so its counter hands out a boot the log has set) that
/// does not exit as not ready, which the units' `RestartPreventExitStatus=2` leaves down
/// for good (mutant: the `StaleWriter` arm of [`crate::StartFailure`] removed). And that
/// the restart it asks for recovers: the next registration's boot, one past, opens the
/// same log, so a gap of N boots takes N restarts.
#[tokio::test]
async fn a_stale_boot_exits_not_ready_and_a_later_boot_opens() {
    let dir = meta_dir("stale-exit");
    let bucket = Bucket::default();
    for boot in [3, 3, 2, 4] {
        let meta = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
            .await
            .expect("open the log");
        let keys = ObjectKeys::under(meta.store_prefix());
        let opened = open_registered(meta, bucket.clone(), keys, &joined(7, boot)).await;
        match (boot, opened) {
            (3 | 4, Ok(cache)) => drop(cache),
            (_, Err(e)) => {
                assert_eq!(crate::StartFailure::of(e.as_ref()).status(), 75, "{e}");
            }
            (boot, Ok(_)) => panic!("boot {boot} opened over a log at boot 3"),
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Catches: a node root that keeps the log id (node keys would sit under one log's
/// prefix, and a wiped node's new log would put them elsewhere) or that takes a prefix
/// not made of a root and the log id.
#[test]
fn the_root_is_the_logs_prefix_less_its_id() {
    let log = kbf_log::FarmLogId::new(std::num::NonZeroU64::new(0xab).expect("not zero"));
    let prefix = KeyPrefix::new("bucket/kbf/00000000000000ab/").expect("prefix");
    let root = prefix_root(&prefix, log).expect("a root");
    assert_eq!(root.as_str(), "bucket/kbf/");
    let other = KeyPrefix::new("bucket/kbf/00000000000000cd/").expect("prefix");
    let refused = prefix_root(&other, log).err().map(|e| e.to_string());
    assert_eq!(
        refused.as_deref(),
        Some(
            "the log's key prefix bucket/kbf/00000000000000cd/ does not end in its id 00000000000000ab"
        )
    );
}

/// Catches: a registered node that raises a log of a cell of several voters to version
/// 4 (a replica that does not run it would refuse the log from then on).
#[test]
fn only_a_single_voter_log_is_raised_for_a_node() {
    assert_eq!(one_voter(1), Ok(()));
    assert_eq!(
        one_voter(3).err().as_deref(),
        Some("a registered storage node's metadata log has 3 voters; it must have one")
    );
}

/// Catches: a registered node's open that does not refuse a log of several voters
/// (mutant: the [`one_voter`] call dropped from the open): the open fails with the
/// voters' message before the log is raised or anything is written. A log this version
/// creates has one voter, so the count is the open's argument.
#[tokio::test]
async fn a_log_of_several_voters_refuses_the_open_before_it_is_raised() {
    let dir = meta_dir("voters");
    let bucket = Bucket::default();
    let meta = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
        .await
        .expect("open the log");
    let keys = ObjectKeys::under(meta.store_prefix());
    let one = std::num::NonZeroU64::new(1).expect("not zero");
    let refused = open_among(meta, bucket.clone(), keys, (one, one), 3)
        .await
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        refused.as_deref(),
        Some("a registered storage node's metadata log has 3 voters; it must have one")
    );
    assert!(bucket.written().is_empty());
    let meta = RaftMetaLog::open_dir(&dir, Retention::default(), "kbf/")
        .await
        .expect("open the log again");
    assert_eq!(meta.machine_version(), MachineVersion::V1, "nothing raised");
    drop(meta);
    let _ = std::fs::remove_dir_all(dir);
}

// ---- unit U0 of docs/design/frontend-proxy.md, section 11: a node meets a FRESH state store

/// A frontend over a store with nothing in it: the fresh Postgres of the design.
fn fresh_frontend() -> crate::members::Members {
    let clock = Arc::new(kbf_state::ManualClock::default());
    let fake = kbf_state::fake::Fake::with_flaw(clock, kbf_state::fake::Flaw::None);
    crate::members::Members::new(
        Arc::new(fake),
        crate::members::MembershipConfig {
            ttl: Duration::from_secs(30),
            dwell: Duration::from_secs(120),
            replication: 2,
        },
    )
}

fn register_as(members: &crate::members::Members, instance: &str) -> Joined {
    let r = members
        .register(&crate::members::Registrant {
            instance_id: instance.to_owned(),
            addr: "192.0.2.1:8982".to_owned(),
            zone: String::new(),
            weight: 1,
            protocol: crate::members::PROTOCOL,
            progress: crate::members::Progress::CaughtUp,
        })
        .expect("register");
    joined(r.id.0, r.boot)
}

/// A node that has run as `member` for boots `1..=last_boot`, each writing one blob.
/// Returns the digests it holds.
async fn node_that_has_run(
    dir: &Path,
    bucket: &Bucket,
    member: u64,
    last_boot: u64,
) -> Vec<kbf_types::Digest> {
    let mut held = Vec::new();
    for boot in 1..=last_boot {
        start_and_write(dir, bucket, &joined(member, boot), 1).await;
        held.push(VerifiedBlob::hashed(Bytes::from(format!("boot {boot} blob 0"))).digest());
    }
    held
}

async fn open_over(
    dir: &Path,
    bucket: &Bucket,
    who: &Joined,
) -> Result<kbf_front::Cache<RaftMetaLog<kbf_store::StdFs>, Bucket>, Box<dyn std::error::Error>> {
    let meta = RaftMetaLog::open_dir(dir, Retention::default(), "kbf/")
        .await
        .expect("open the log");
    let prefix = meta.store_prefix();
    let keys = ObjectKeys {
        root: prefix_root(&prefix, meta.log_id()).expect("a root"),
        legacy: prefix,
    };
    open_registered(meta, bucket.clone(), keys, who).await
}

/// Design section 11 asks U0 to check that "a storage node re-registers its existing blobs
/// without re-upload" against a fresh store. Holds when the fresh store hands the node a
/// member id its log never set: the log is the node's own, so its blobs are indexed
/// already; nothing is uploaded again and the new boot writes under the new member.
/// Catches an open that rebuilds or re-puts what the log already indexes.
#[tokio::test]
async fn a_node_given_a_new_member_id_by_a_fresh_store_keeps_its_blobs() {
    let dir = meta_dir("fresh-new-id");
    let bucket = Bucket::default();
    let held = node_that_has_run(&dir, &bucket, 7, 3).await;
    let uploaded = bucket.written();
    let members = fresh_frontend();
    let _other = register_as(&members, "other");
    let mine = register_as(&members, "me");
    assert_eq!((mine.member_id, mine.boot), (2, 1), "ids restart at 1");
    let cache = open_over(&dir, &bucket, &mine)
        .await
        .expect("opens at once");
    assert_eq!(
        cache.present(&held).await.expect("present"),
        vec![true; held.len()]
    );
    assert_eq!(bucket.written(), uploaded, "nothing was uploaded again");
    let _ = std::fs::remove_dir_all(dir);
}

/// What the same step costs when the fresh store hands the node the member id its log has
/// set already: boots restart at 1, `SetWriter` refuses every boot up to the latest the log
/// holds, so the node exits 75 once per boot it ever had and opens on the next one. The
/// blobs are still there and nothing is uploaded. Pins the cost the design must name: N
/// boots are N restarts at the unit's `RestartSec=`, each after a registration, during
/// which the node is in no map. Catches a `SetWriter` that stops refusing an old boot (two
/// processes could then put under one writer) and a fresh-store registration that starts
/// above the node's boot.
#[tokio::test]
async fn a_node_given_its_old_member_id_opens_only_after_one_restart_per_old_boot() {
    let dir = meta_dir("fresh-same-id");
    let bucket = Bucket::default();
    let held = node_that_has_run(&dir, &bucket, 1, 4).await;
    let uploaded = bucket.written();
    let members = fresh_frontend();
    let mut restarts = 0;
    let cache = loop {
        let mine = register_as(&members, "me");
        assert_eq!(
            mine.member_id, 1,
            "the first node of a fresh store is member 1"
        );
        match open_over(&dir, &bucket, &mine).await {
            Ok(cache) => break cache,
            Err(e) => {
                assert_eq!(crate::StartFailure::of(e.as_ref()).status(), 75, "{e}");
                restarts += 1;
            }
        }
        assert!(restarts < 20, "never opened");
    };
    assert_eq!(restarts, 4, "one 'not ready' exit per boot the log has set");
    assert_eq!(
        cache.present(&held).await.expect("present"),
        vec![true; held.len()]
    );
    assert_eq!(bucket.written(), uploaded, "nothing was uploaded again");
    let _ = std::fs::remove_dir_all(dir);
}

/// The design's claim as a requirement: the first registration on a fresh store opens the
/// node. FAILS today for a node that gets its old member id (see the test above).
#[tokio::test]
#[ignore = "U0 red: a node that gets its old member id from a fresh store exits 75 once per old boot before it opens; U9 must seed the fresh store's counters or the node must tolerate it"]
async fn the_first_registration_on_a_fresh_store_opens_a_node_that_had_run_before() {
    let dir = meta_dir("fresh-first");
    let bucket = Bucket::default();
    let _held = node_that_has_run(&dir, &bucket, 1, 4).await;
    let mine = register_as(&fresh_frontend(), "me");
    let opened = open_over(&dir, &bucket, &mine).await.map(drop);
    assert!(opened.is_ok(), "{opened:?}");
    let _ = std::fs::remove_dir_all(dir);
}
