//! The write tier over the Raft metadata log ([`kbf_server::write_tier::attach`], as the
//! binary calls it): the Raft log raised to machine version 2 once, only when a tier is
//! configured, and staged, sealed and flushed objects kept across a restart.
//!
//! Each test runs the server's start on real directories (the raft directory and the
//! tier's) and a store that outlives the restarts and can be taken down, so a read
//! that succeeds while it is down came from the tier. Each test's comment names the
//! defect it is there to catch.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use bytes::Bytes;
use kbf_front::{Cache, MetaLog, TierStartup, VerifiedBlob};
use kbf_log::MachineVersion;
use kbf_meta::{BlobAnswer, ObjectId, Retention, StagedState, TierId};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_server::stores::{Hot, StoreFlags, StoreUri};
use kbf_server::write_tier::attach;
use kbf_server::{RaftLogError, RaftMetaLog};
use kbf_store::StdFs;
use kbf_tier::TierOptions;
use kbf_threads::disk::{FreeSpace, Space};
use kbf_types::Digest;

/// A bucket that outlives each start over it and refuses every call while down.
#[derive(Clone)]
struct Store {
    memory: Arc<MemoryStore>,
    down: Arc<AtomicBool>,
}

impl Store {
    fn new() -> Self {
        let capabilities = Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        Self {
            memory: Arc::new(MemoryStore::new(capabilities)),
            down: Arc::new(AtomicBool::new(false)),
        }
    }

    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    fn up(&self) -> Result<&MemoryStore, ObjectStoreError> {
        if self.down.load(Ordering::SeqCst) {
            let down = std::io::Error::other("down for the test");
            return Err(ObjectStoreError::Transport(Arc::new(down)));
        }
        Ok(&self.memory)
    }
}

impl ObjectStore for Store {
    fn capabilities(&self) -> Capabilities {
        self.memory.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.up()?.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.up()?.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.up()?.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.up()?.list(prefix, after, max_keys).await
    }
}

/// A raft directory and a tier directory, empty, for one test.
struct Dirs {
    raft: PathBuf,
    tier: PathBuf,
}

impl Dirs {
    fn new(name: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("kbf-server-write-tier-raft")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        Self {
            raft: root.join("raft"),
            tier: root.join("tier"),
        }
    }

    /// The hot store a server resolves `--hot-store` to, when `tier`: the directory and a
    /// 1 GiB budget.
    fn hot(&self, tier: bool) -> Option<Hot> {
        tier.then(|| Hot {
            dir: self.tier.clone(),
            budget: 1 << 30,
            abandon: None,
        })
    }

    /// Opens the log as a binary that runs machine versions up to `runs` would.
    async fn open_running(&self, runs: MachineVersion) -> Result<Log, RaftLogError> {
        let fs = StdFs::new(&self.raft)?;
        let no_new_log = || Err(std::io::Error::other("the log exists"));
        RaftMetaLog::open_running(fs, Retention::default(), "kbf/", no_new_log, runs).await
    }
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.raft.parent().expect("a root"));
    }
}

type Log = RaftMetaLog<StdFs>;
type TierCache = Cache<Log, Store>;

/// One start of the server: the Raft log in `dirs`, a cache over it and `store`, and
/// the tier attached as `attach` does it when `tier`.
async fn start(dirs: &Dirs, store: &Store, tier: bool) -> (TierCache, Option<TierStartup>) {
    let log = RaftMetaLog::open_dir(&dirs.raft, Retention::default(), "kbf/")
        .await
        .expect("open the Raft log");
    let prefix = log.store_prefix();
    let cache = Cache::open(log, store.clone(), prefix)
        .await
        .expect("open the cache");
    attach(cache, dirs.hot(tier).as_ref())
        .await
        .expect("attach")
}

fn blob(seed: u8) -> (VerifiedBlob, Bytes, Digest) {
    let bytes = Bytes::from(vec![seed; 4096]);
    let blob = VerifiedBlob::hashed(bytes.clone());
    let digest = blob.digest();
    (blob, bytes, digest)
}

/// Where `digest` is staged (`Some(None)` once flushed); `None` unless present.
async fn staged_on(cache: &TierCache, digest: Digest) -> Option<Option<TierId>> {
    let answer = cache.meta().query(move |s| s.blob(&digest)).await;
    match answer.expect("query") {
        BlobAnswer::Present { staged_on, .. } => Some(staged_on),
        _ => None,
    }
}

async fn object_of(cache: &TierCache, digest: Digest) -> ObjectId {
    let answer = cache.meta().query(move |s| s.blob(&digest)).await;
    match answer.expect("query") {
        BlobAnswer::Present { location, .. } => location.object,
        other => panic!("{digest} is {other:?}"),
    }
}

async fn state(cache: &TierCache, object: ObjectId) -> Option<StagedState> {
    let query = move |s: &kbf_meta::MetaState| s.staged(object).map(kbf_meta::StagedObject::state);
    cache.meta().query(query).await.expect("query")
}

fn later() -> Instant {
    Instant::now() + TierOptions::SEAL_AFTER
}

/// Catches: a server whose tier attaches over a Raft log left at machine version 1
/// (the tier's first commit, `AddTier`, cannot be encoded in format 1: the start is
/// refused), a raise committed again at every start (each one an entry a version-1
/// binary would also refuse, and a raise that is not idempotent), and a raised log that
/// an older binary opens instead of refusing at the raise.
#[tokio::test]
async fn a_raft_server_with_a_tier_raises_its_log_once() {
    let dirs = Dirs::new("raised-once");
    let store = Store::new();
    let (cache, startup) = start(&dirs, &store, true).await;
    assert!(startup.expect("a tier").registered);
    assert_eq!(cache.meta().machine_version(), MachineVersion::V2);
    drop(cache);

    let (cache, startup) = start(&dirs, &store, true).await;
    assert!(!startup.expect("a tier").registered);
    let log = cache.meta();
    assert_eq!(log.machine_version(), MachineVersion::V2);
    let applied = log.applied_index();
    log.raise_machine_version(MachineVersion::V2)
        .expect("raise");
    assert_eq!(log.applied_index(), applied, "the raise committed again");
    drop(cache);

    let rolled_back = dirs.open_running(MachineVersion::V1).await;
    assert!(
        matches!(rolled_back, Err(RaftLogError::Decode { .. })),
        "{rolled_back:?}"
    );
}

/// Catches: a raft server that raises its log without a tier (an older binary could no
/// longer read the log of a server that never ran one).
#[tokio::test]
async fn a_raft_server_without_a_tier_leaves_its_log_at_version_1() {
    let dirs = Dirs::new("no-tier");
    let store = Store::new();
    let (cache, startup) = start(&dirs, &store, false).await;
    assert!(startup.is_none());
    assert!(cache.write_tier().is_none());
    assert_eq!(cache.meta().machine_version(), MachineVersion::V1);
    drop(cache);
    let older = dirs.open_running(MachineVersion::V1).await;
    assert_eq!(
        older
            .expect("a version-1 binary opens it")
            .machine_version(),
        MachineVersion::V1
    );
}

/// Catches: a raft server that clears the tier directory at a start, as the memory
/// log's does (its `TIER` gone while objects are staged, the start is refused; the
/// staged blocks, the only copies of acknowledged blobs, deleted), or that forgets an
/// object's staged, sealed or flushed state across a restart. Before the restart: one
/// object flushed (`ObjectFlushed`), one sealed whose PUT failed (`SealStaged`), one
/// open (`PutStaged` only). After it, with the store down, the staged ones are read
/// from the tier, and once the store is back the next round flushes both.
#[tokio::test]
async fn staged_sealed_and_flushed_objects_survive_a_raft_restart() {
    let dirs = Dirs::new("survive");
    let store = Store::new();
    let (cache, _) = start(&dirs, &store, true).await;
    let (a, a_bytes, a_digest) = blob(1);
    cache.store_blobs(vec![a]).await.expect("stored");
    let flushed = object_of(&cache, a_digest).await;
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, [flushed]);

    let (b, b_bytes, b_digest) = blob(2);
    cache.store_blobs(vec![b]).await.expect("stored");
    let sealed = object_of(&cache, b_digest).await;
    store.set_down(true);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!((round.sealed, round.flushed), (vec![sealed], vec![]));
    let (c, c_bytes, c_digest) = blob(3);
    cache.store_blobs(vec![c]).await.expect("stored");
    let open = object_of(&cache, c_digest).await;
    assert_ne!(open, sealed);
    let tier = staged_on(&cache, b_digest).await.expect("present");
    assert!(tier.is_some());
    drop(cache);

    let (cache, startup) = start(&dirs, &store, true).await;
    let startup = startup.expect("a tier");
    assert_eq!(Some(startup.tier), tier, "the same tier");
    assert_eq!(startup.recovery.lost, []);
    assert_eq!(startup.recovery.sealed, [sealed]);
    assert_eq!(startup.recovery.unsealed, [open]);
    assert_eq!(staged_on(&cache, a_digest).await, Some(None), "flushed");
    assert_eq!(state(&cache, flushed).await, None);
    assert_eq!(state(&cache, sealed).await, Some(StagedState::Sealed));
    assert_eq!(state(&cache, open).await, Some(StagedState::Open));
    for (digest, bytes) in [(b_digest, &b_bytes), (c_digest, &c_bytes)] {
        assert_eq!(staged_on(&cache, digest).await, Some(tier));
        let read = cache.read_blob(&digest).await.expect("read from the tier");
        assert_eq!(&read, bytes);
    }

    store.set_down(false);
    let round = cache.flush_tier(later()).await.expect("round");
    let mut done = round.flushed.clone();
    done.sort_unstable();
    let mut both = vec![sealed, open];
    both.sort_unstable();
    assert_eq!(done, both, "{round:?}");
    for (digest, bytes) in [
        (a_digest, &a_bytes),
        (b_digest, &b_bytes),
        (c_digest, &c_bytes),
    ] {
        assert_eq!(staged_on(&cache, digest).await, Some(None));
        assert_eq!(&cache.read_blob(&digest).await.expect("read"), bytes);
    }
}

/// A disk with room: only the flags' resolution reads it.
struct Roomy;

impl FreeSpace for Roomy {
    fn space(&self, _: &std::path::Path) -> std::io::Result<Space> {
        Ok(Space {
            total: 1 << 40,
            available: 1 << 39,
        })
    }
}

/// Catches an upgrade that loses data: a server run with its raft log in R and its tier
/// in T (`--meta-dir=R --hot-store=file://T`, which is how the old flags laid them out)
/// must, on the new flags, resolve the log to R (not `<T>/meta`, which would start an
/// empty log beside the tier and orphan every staged block), recover the same log
/// (identity and applied entries) and the same tier, serve the staged block, and make no
/// `<T>/meta`. One block is staged and unflushed, the only copy of an acknowledged blob.
#[tokio::test]
async fn the_new_flags_open_an_old_raft_directory_and_tier_with_their_data() {
    let dirs = Dirs::new("upgrade");
    let store = Store::new();
    let (cache, _) = start(&dirs, &store, true).await;
    let (a, a_bytes, a_digest) = blob(7);
    cache.store_blobs(vec![a]).await.expect("stored");
    let object = object_of(&cache, a_digest).await;
    let (log_id, applied) = (cache.meta().log_id(), cache.meta().applied_index());
    let tier = staged_on(&cache, a_digest).await.expect("present");
    assert!(tier.is_some(), "staged on the tier");
    drop(cache);
    assert!(dirs.raft.join("log-record").is_file());
    assert!(dirs.tier.join("TIER").is_file());

    let cold = StoreUri::File(dirs.raft.parent().expect("a root").join("cold"));
    let hot = StoreUri::File(dirs.tier.clone());
    let flags = StoreFlags {
        cold: Some(&cold),
        hot: Some(&hot),
        hot_max_bytes: None,
        hot_abandon: None,
        meta_dir: Some(&dirs.raft),
    };
    let stores = flags.resolve(&Roomy).expect("the new flags resolve");
    assert_eq!(stores.meta_dir, dirs.raft);
    assert!(!stores.meta_in_hot);
    let tier_dir = stores.hot.as_ref().map(|hot| hot.dir.clone());
    assert_eq!(tier_dir.as_deref(), Some(dirs.tier.as_path()));

    let log = RaftMetaLog::open_dir(&stores.meta_dir, Retention::default(), "kbf/")
        .await
        .expect("open the old raft directory");
    assert_eq!(log.log_id(), log_id, "a different log");
    assert_eq!(log.applied_index(), applied, "entries lost or added");
    let prefix = log.store_prefix();
    let cache = Cache::open(log, store.clone(), prefix)
        .await
        .expect("open the cache");
    let (cache, startup) = attach(cache, stores.hot.as_ref()).await.expect("attach");
    let startup = startup.expect("a tier");
    assert_eq!(Some(startup.tier), tier, "the same tier");
    assert!(!startup.registered, "registered again");
    assert_eq!(startup.recovery.lost, []);
    assert_eq!(startup.recovery.unsealed, [object]);
    assert_eq!(staged_on(&cache, a_digest).await, Some(tier));
    let read = cache.read_blob(&a_digest).await.expect("read from the tier");
    assert_eq!(read, a_bytes);
    assert!(!dirs.tier.join("meta").exists(), "<T>/meta made");
}
