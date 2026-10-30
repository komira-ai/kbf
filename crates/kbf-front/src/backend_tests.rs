//! [`Backend`] over a cache in this process and over a [`Storage`]: the same answers,
//! and the input check over a storage node.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_meta::{ActionRecord, Role};
use kbf_proto::reapi::{ActionResult, Directory, DirectoryNode, FileNode};
use kbf_proto::worker::segments_server::Segments;
use kbf_route::{MapSource, MapView, Nodes, RefetchError};
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError};
use kbf_types::Digest;
use prost::Message as _;

use super::Backend;
use crate::cache::{Cache, VerifiedBlob};
use crate::meta_log::MemoryMetaLog;
use crate::{CacheStorage, Channels, Routed, SegmentsConfig};

type Mem = Cache<MemoryMetaLog, kbf_objstore::MemoryStore>;

fn proto(d: &Digest) -> kbf_proto::reapi::Digest {
    crate::digest_to_proto(d)
}

fn blob(text: &str) -> (Digest, Bytes) {
    let bytes = Bytes::from(text.to_owned());
    (kbf_segments::sha256(&bytes), bytes)
}

fn dir(files: &[Digest], dirs: &[Digest]) -> (Digest, Bytes) {
    let message = Directory {
        files: files
            .iter()
            .map(|d| FileNode {
                name: d.hash_hex(),
                digest: Some(proto(d)),
                ..FileNode::default()
            })
            .collect(),
        directories: dirs
            .iter()
            .map(|d| DirectoryNode {
                name: d.hash_hex(),
                digest: Some(proto(d)),
            })
            .collect(),
        ..Directory::default()
    };
    let bytes = Bytes::from(message.encode_to_vec());
    (kbf_segments::sha256(&bytes), bytes)
}

async fn put(cache: &Mem, blobs: &[&(Digest, Bytes)]) {
    let blobs = blobs
        .iter()
        .map(|(d, b)| VerifiedBlob::new(*d, b.clone()).expect("verified"))
        .collect();
    cache.store_blobs(blobs).await.expect("stored");
}

/// A cache, and a backend of each kind over it: in this process, and through a storage.
fn both(cache: &Arc<Mem>) -> (Backend, Backend) {
    let storage = CacheStorage::new(Arc::clone(cache), SegmentsConfig::DEFAULT);
    (
        Backend::from(Arc::clone(cache)),
        Backend::remote(Arc::new(storage)),
    )
}

/// What a [`Spy`] gets wrong.
#[derive(Clone, Copy)]
enum Fault {
    /// Nothing.
    None,
    /// Reading this digest says it is held but cannot be read.
    Unreadable(Digest),
    /// Reading this digest fails in the store.
    ReadFails(Digest),
    /// Every FindMissing fails.
    AskFails,
    /// A streamed put answers that the claim was withdrawn, whatever it was given.
    Withdrawn,
    /// A streamed put answers that it stored another blob than the one claimed.
    StoredAnother,
}

/// A [`Storage`] over another, that counts FindMissing calls and gets one thing wrong.
struct Spy {
    inner: Arc<dyn Storage>,
    asks: AtomicUsize,
    fault: Fault,
}

impl Storage for Spy {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.asks.fetch_add(1, Ordering::SeqCst);
        if matches!(self.fault, Fault::AskFails) {
            return Box::pin(async { Err(StorageError::Internal("down".to_owned())) });
        }
        self.inner.find_missing(digests)
    }

    /// Forwards: the inner storage decides what is durable.
    fn find_durable<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.inner.find_durable(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        match self.fault {
            Fault::Unreadable(d) if d == *digest => {
                Box::pin(async move { Err(StorageError::Unreachable(*digest)) })
            }
            Fault::ReadFails(d) if d == *digest => {
                Box::pin(async { Err(StorageError::Internal("disk".to_owned())) })
            }
            _ => self.inner.read_blob(digest),
        }
    }

    fn read_range<'a>(&'a self, d: &'a Digest, offset: u64, limit: u64) -> Answer<'a, ReadStream> {
        self.inner.read_range(d, offset, limit)
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.inner.put(bytes)
    }

    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        self.inner.stream(size)
    }

    fn put_streamed(
        &self,
        size: u64,
        bytes: futures::stream::BoxStream<'static, std::io::Result<Bytes>>,
        claim: futures::future::BoxFuture<'static, Option<Digest>>,
    ) -> Answer<'_, Option<Digest>> {
        if matches!(self.fault, Fault::Withdrawn) {
            return Box::pin(async { Ok(None) });
        }
        if matches!(self.fault, Fault::StoredAnother) {
            let other = blob("another blob").0;
            return Box::pin(async move { Ok(Some(other)) });
        }
        self.inner.put_streamed(size, bytes, claim)
    }

    fn segments(&self) -> Arc<dyn Segments> {
        self.inner.segments()
    }

    fn action_result<'a>(&'a self, a: &'a Digest) -> Answer<'a, Option<(Digest, ActionResult)>> {
        self.inner.action_result(a)
    }

    fn prepare_action_result<'a>(&'a self, r: &'a ActionResult) -> Answer<'a, ActionRecord> {
        self.inner.prepare_action_result(r)
    }

    fn commit_action_record(&self, role: Role, a: Digest, r: ActionRecord) -> Answer<'_, ()> {
        self.inner.commit_action_record(role, a, r)
    }
}

fn spy(cache: &Arc<Mem>, fault: Fault) -> (Backend, Arc<Spy>) {
    let inner = Arc::new(CacheStorage::new(
        Arc::clone(cache),
        SegmentsConfig::DEFAULT,
    ));
    let spy = Arc::new(Spy {
        inner,
        asks: AtomicUsize::new(0),
        fault,
    });
    (Backend::remote(Arc::clone(&spy) as Arc<dyn Storage>), spy)
}

/// Catches: the input check over a storage node answering differently from the cache's
/// own: a missing file, a missing directory (nothing under it is looked at) and a
/// directory reached twice must give the same absent digests, in the same order
/// (directories first), and the same count of directories read.
#[tokio::test]
async fn the_walk_over_storage_finds_what_the_cache_walk_finds() {
    let cache = Arc::new(Cache::memory());
    let (held, lost) = (blob("held"), blob("lost"));
    let shared = dir(&[held.0], &[]);
    let (gone_file, gone_dir) = (blob("never stored"), dir(&[lost.0], &[]));
    let d1 = dir(&[held.0, lost.0], &[shared.0]);
    let d2 = dir(&[gone_file.0], &[shared.0, gone_dir.0]);
    let root = dir(&[held.0], &[d1.0, d2.0]);
    put(&cache, &[&held, &shared, &d1, &d2, &root]).await;
    let (local, remote) = both(&cache);

    let want = local.walk_inputs(root.0).await.expect("cache walk");
    let got = remote.walk_inputs(root.0).await.expect("storage walk");
    assert_eq!(got.absent, want.absent);
    assert_eq!(got.absent.len(), 3, "{:?}", got.absent);
    assert_eq!(got.absent[0], gone_dir.0, "directories come first");
    assert_eq!(got.dirs_read, want.dirs_read);
    assert_eq!(got.dirs_read, 4, "root, d1, d2 and the shared one once");

    put(&cache, &[&lost, &gone_file, &gone_dir]).await;
    assert!(
        remote
            .walk_inputs(root.0)
            .await
            .expect("walk")
            .absent
            .is_empty()
    );
}

/// Catches: a directory the node holds but cannot read stopping the Execute with an
/// error (it must be reported absent so the client uploads it again), a store failure
/// reading one reported as absent (the client would upload what the node holds), a
/// directory that does not decode passing as an empty one, and a node that cannot answer
/// FindMissing reported as "nothing missing".
#[tokio::test]
async fn the_walk_over_storage_reports_what_it_cannot_read() {
    let cache = Arc::new(Cache::memory());
    let file = blob("file");
    let leaf = dir(&[file.0], &[]);
    let root = dir(&[], &[leaf.0]);
    put(&cache, &[&file, &leaf, &root]).await;

    let (remote, _) = spy(&cache, Fault::Unreadable(leaf.0));
    let walked = remote.walk_inputs(root.0).await.expect("walk");
    assert_eq!(walked.absent, vec![leaf.0]);

    let junk = blob("not a Directory message \u{ff}\u{ff}");
    put(&cache, &[&junk]).await;
    let (remote, _) = spy(&cache, Fault::None);
    let e = remote
        .walk_inputs(junk.0)
        .await
        .expect_err("does not decode");
    assert!(matches!(e, StorageError::Invalid(m) if m.contains("does not decode")));

    let (remote, _) = spy(&cache, Fault::ReadFails(leaf.0));
    let e = remote.walk_inputs(root.0).await.expect_err("a read fails");
    assert!(matches!(e, StorageError::Internal(m) if m == "disk"));

    let (remote, _) = spy(&cache, Fault::AskFails);
    let e = remote.walk_inputs(root.0).await.expect_err("node down");
    assert!(matches!(e, StorageError::Internal(m) if m == "down"));
}

/// Catches: every file of a tree named in one FindMissing (a tree of many thousand files
/// overflowing the message limit): 4097 files go in two calls (at most 4096 digests,
/// about 300 KiB, in the first), and all of them are asked about.
#[tokio::test]
async fn files_are_asked_about_in_batches() {
    let cache = Arc::new(Cache::memory());
    let files: Vec<Digest> = (0..4097)
        .map(|n| kbf_segments::sha256(n.to_string().as_bytes()))
        .collect();
    let root = dir(&files, &[]);
    put(&cache, &[&root]).await;
    let (remote, spy) = spy(&cache, Fault::None);
    let walked = remote.walk_inputs(root.0).await.expect("walk");
    assert_eq!(walked.absent.len(), files.len());
    assert_eq!(spy.asks.load(Ordering::SeqCst), 2);
}

/// Catches: a streamed put over the cache's storage that does not land the blob where
/// FindMissing and reads see it, or that answers another digest than the one claimed.
#[tokio::test]
async fn a_streamed_put_over_storage_is_found_and_read_back() {
    use futures::StreamExt as _;
    let cache = Arc::new(Cache::memory());
    let (_, spy) = spy(&cache, Fault::None);
    let (digest, bytes) = blob("streamed");
    let size = bytes.len() as u64;
    let pieces = futures::stream::iter([Ok(bytes.slice(..3)), Ok(bytes.slice(3..))]).boxed();
    let claim = Box::pin(async move { Some(digest) });
    let stored = spy.put_streamed(size, pieces, claim).await.expect("put");
    assert_eq!(stored, Some(digest));
    assert!(spy.find_missing(&[digest]).await.expect("find").is_empty());
    assert_eq!(spy.read_blob(&digest).await.expect("read"), bytes);
}

/// The pieces of `bytes`, from the first byte each time the source is opened.
fn pieces_of(bytes: Bytes) -> impl Fn() -> kbf_storage::PieceStream + Send + Sync {
    use futures::StreamExt as _;
    move || {
        let items: Vec<std::io::Result<Bytes>> = vec![Ok(bytes.slice(..4)), Ok(bytes.slice(4..))];
        futures::stream::iter(items).boxed()
    }
}

/// Catches: a streamed put through the backend that does not land the blob (over a cache in this
/// process and over a storage), that takes bytes that are not the digest named and keeps them, or
/// that calls a store's answer of a withdrawn claim a success.
#[tokio::test]
async fn a_streamed_put_through_a_backend_lands_the_blob_and_refuses_other_bytes() {
    for remote in [false, true] {
        let cache = Arc::new(Cache::memory());
        let (local, through) = both(&cache);
        let backend = if remote { through } else { local };
        let (digest, bytes) = blob("a streamed blob");

        backend
            .put_streamed(&digest, &pieces_of(bytes.clone()))
            .await
            .expect("stored");
        assert!(
            backend
                .find_missing(&[digest])
                .await
                .expect("asked")
                .is_empty()
        );
        assert_eq!(backend.read_blob(&digest).await.expect("read"), bytes);

        let claimed = blob("a streamed blOb").0;
        let refused = backend.put_streamed(&claimed, &pieces_of(bytes)).await;
        assert!(
            refused.is_err(),
            "bytes that are not the digest: {refused:?}"
        );
        let missing = backend.find_missing(&[claimed]).await.expect("asked");
        assert_eq!(missing, [claimed], "nothing is kept under the digest");
    }
    let cache = Arc::new(Cache::memory());
    let (backend, _) = spy(&cache, Fault::Withdrawn);
    let (digest, bytes) = blob("a streamed blob");
    let error = backend
        .put_streamed(&digest, &pieces_of(bytes))
        .await
        .expect_err("not stored");
    assert!(matches!(error, StorageError::Internal(_)), "{error:?}");

    let (backend, _) = spy(&cache, Fault::StoredAnother);
    let (digest, bytes) = blob("a streamed blob");
    let error = backend
        .put_streamed(&digest, &pieces_of(bytes))
        .await
        .expect_err("another blob stored");
    assert!(matches!(error, StorageError::Internal(_)), "{error:?}");
}

/// Catches: the two kinds answering a call differently: the action-cache write (both
/// halves, as a daemon's result) and read, blob reads, FindMissing, and the leadership
/// and metrics each reports (a cache in memory has no log to lead; the metrics render).
#[tokio::test]
async fn both_kinds_answer_the_scheduler_calls_alike() {
    for remote in [false, true] {
        let cache = Arc::new(Cache::memory());
        let (local, through) = both(&cache);
        let backend = if remote { through } else { local };
        let (digest, bytes) = blob("an action");
        put(&cache, &[&(digest, bytes.clone())]).await;
        assert_eq!(backend.read_blob(&digest).await.expect("held"), bytes);
        let absent = blob("absent").0;
        assert_eq!(
            backend.find_missing(&[digest, absent]).await.expect("ask"),
            vec![absent]
        );
        assert!(matches!(
            backend.read_blob(&absent).await,
            Err(StorageError::NotFound(_))
        ));

        assert!(
            backend
                .action_result(&digest)
                .await
                .expect("miss")
                .is_none()
        );
        let record = backend
            .prepare_action_result(&ActionResult::default())
            .await
            .expect("prepared");
        backend
            .commit_action_record(digest, record)
            .await
            .expect("committed");
        let (_, hit) = backend
            .action_result(&digest)
            .await
            .expect("lookup")
            .expect("a hit");
        assert_eq!(hit, ActionResult::default());

        assert!(backend.leadership().is_none());
        let text = backend.render_metrics();
        assert!(text.contains("kbf_"), "{remote}");
        assert_in_flight_families(&text);
        assert_eq!(format!("{backend:?}"), "Backend(..)");
    }
}

/// Three storage nodes in this process, each over a cache of its own, and a map of them.
struct Trio {
    nodes: BTreeMap<MemberId, Arc<dyn Storage>>,
    map: Arc<Map>,
    /// The generation before `map`, while a change settles.
    previous: Option<Arc<Map>>,
}

impl Trio {
    fn new() -> Self {
        Self::replicated(1)
    }

    /// The same, each blob written to `r` owners.
    fn replicated(r: u8) -> Self {
        let nodes = (1..=3)
            .map(|id| {
                let node: Arc<dyn Storage> = Arc::new(CacheStorage::new(
                    Arc::new(Cache::memory()),
                    SegmentsConfig::DEFAULT,
                ));
                (MemberId(id), node)
            })
            .collect();
        let members = (1..=3)
            .map(|id| Member {
                id: MemberId(id),
                addr: format!("127.0.0.1:{id}"),
                zone: String::new(),
                weight: 1,
                state: MemberState::Active,
                boot: 0,
            })
            .collect();
        let map = Map::new(1, members, Rule::Rendezvous, r, 1, Vec::new()).expect("a map");
        Self {
            nodes,
            map: Arc::new(map),
            previous: None,
        }
    }

    /// The same farm with every member's boot known (non-zero), so the router can say
    /// what its "held" answers rest on ([`kbf_route::Router::epoch`]).
    fn booted(mut self) -> Self {
        let members = self
            .map
            .members()
            .iter()
            .cloned()
            .map(|mut m| {
                m.boot = 1;
                m
            })
            .collect();
        let map = Map::new(
            self.map.generation(),
            members,
            Rule::Rendezvous,
            self.map.r(),
            1,
            Vec::new(),
        )
        .expect("a map");
        self.map = Arc::new(map);
        self
    }

    /// The same farm after a change: `map` is generation 2 and the map it replaced is
    /// the one this had.
    fn changed(mut self, ids: &[u64]) -> Self {
        let members = self
            .map
            .members()
            .iter()
            .filter(|m| ids.contains(&m.id.0))
            .cloned()
            .collect();
        let r = self.map.r();
        let next = Map::new(2, members, Rule::Rendezvous, r, 1, Vec::new()).expect("a map");
        self.previous = Some(std::mem::replace(&mut self.map, Arc::new(next)));
        self
    }

    /// A backend routed by the map, whose `--storage` node is member 1, and the same
    /// farm as the one node `--storage` names.
    fn backends(self: &Arc<Self>) -> (Backend, Backend) {
        let routed = Arc::new(Routed::new(self.clone(), self.clone(), self.clone()));
        let first = Arc::clone(&self.nodes[&MemberId(1)]);
        (
            Backend::routed(routed, Arc::clone(&first)),
            Backend::remote(first),
        )
    }

    /// Stores `blob` on the node the map writes it to.
    async fn store(&self, blob: &(Digest, Bytes)) {
        let owner = self.map.write_owners(&blob.0)[0];
        self.nodes[&owner]
            .put(blob.1.clone())
            .await
            .expect("stored");
    }
}

impl Nodes for Trio {
    fn node(&self, member: &Member, _: u64) -> Result<Arc<dyn Storage>, StorageError> {
        Ok(Arc::clone(&self.nodes[&member.id]))
    }
}

impl Channels for Trio {
    fn channel(&self, _: &Member) -> Result<tonic::transport::Channel, StorageError> {
        Err(StorageError::Internal("no channels in process".to_owned()))
    }
}

impl MapSource for Trio {
    fn current(&self) -> Option<MapView> {
        Some(MapView {
            map: Arc::clone(&self.map),
            previous: self.previous.clone(),
            earlier: Vec::new(),
        })
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
        Box::pin(async move {
            Err(RefetchError {
                at_least,
                reason: "nothing newer".to_owned(),
            })
        })
    }
}

/// Catches: the input check asking the `--storage` node about a tree whose directories and
/// files are on the nodes the map put them on (the rehearsal's failure, in the walk): the
/// routed walk finds every one, reads each directory once, and names the one file that is
/// held nowhere; the same tree over the one node reports most of it absent.
#[tokio::test]
async fn the_routed_walk_finds_each_blob_at_its_owner() {
    let trio = Arc::new(Trio::new());
    let files: Vec<_> = (0..30).map(|n| blob(&format!("file {n}"))).collect();
    let never = blob("never stored");
    let subs: Vec<_> = files
        .chunks(10)
        .map(|chunk| dir(&chunk.iter().map(|f| f.0).collect::<Vec<_>>(), &[]))
        .collect();
    let root = dir(&[never.0], &subs.iter().map(|d| d.0).collect::<Vec<_>>());
    for held in files.iter().chain(&subs).chain([&root]) {
        trio.store(held).await;
    }
    let (routed, one_node) = trio.backends();

    let walked = routed.walk_inputs(root.0).await.expect("routed walk");
    assert_eq!(walked.absent, vec![never.0]);
    assert_eq!(walked.dirs_read, 4);
    let alone = one_node
        .walk_inputs(root.0)
        .await
        .expect("walk of one node");
    // The root is not on the `--storage` node, so nothing under it is looked at.
    assert_eq!(alone.absent, vec![root.0]);
}

/// A file in each of the three places the input check must look, and one nobody holds:
/// at the second write owner only (the primary is a joined node that is still empty), at
/// the previous generation's owner only, and nowhere; all under one directory.
struct Spread {
    root: (Digest, Bytes),
    replica_only: (Digest, Bytes),
    previous_only: (Digest, Bytes),
    nowhere: (Digest, Bytes),
}

impl Spread {
    /// Stores them on a farm of three that kept members `now` after a change from all three
    /// (each blob written to two owners).
    async fn store(now: &[u64]) -> (Arc<Trio>, Self) {
        let before = Trio::replicated(2);
        let old = Arc::clone(&before.map);
        let trio = before.changed(now);
        let new = Arc::clone(&trio.map);
        let find = |tag: &str, want: &dyn Fn(&Digest) -> bool| {
            (0..)
                .map(|n| blob(&format!("{tag} {n}")))
                .find(|b| want(&b.0))
                .expect("a blob that fits")
        };
        let replica_only = find("replica", &|d| {
            let owners = new.write_owners(d);
            owners.len() == 2 && old.write_owners(d)[0] == owners[0]
        });
        let previous_only = find("previous", &|d| {
            // The member that left is its old primary.
            old.write_owners(d)[0] == MemberId(3)
        });
        let nowhere = blob("held by nobody");
        let root = dir(&[replica_only.0, previous_only.0, nowhere.0], &[]);
        let owners = new.write_owners(&replica_only.0);
        trio.nodes[&owners[1]]
            .put_unowned(replica_only.1.clone())
            .await
            .expect("stored");
        let prior = old.write_owners(&previous_only.0)[0];
        trio.nodes[&prior]
            .put_unowned(previous_only.1.clone())
            .await
            .expect("stored");
        for owner in new.write_owners(&root.0) {
            trio.nodes[&owner]
                .put_unowned(root.1.clone())
                .await
                .expect("stored");
        }
        (
            Arc::new(trio),
            Self {
                root,
                replica_only,
                previous_only,
                nowhere,
            },
        )
    }
}

/// Catches the refusal that kept a farm from running its actions after a node joined: the
/// input check asked only each file's primary, and a primary that had just joined held
/// nothing, so files its replica (or the previous generation's owner) held were refused as
/// "not in the CAS" on every run. The walk now reports only the file no owner of either
/// generation holds.
#[tokio::test]
async fn the_routed_walk_finds_a_file_any_owner_holds() {
    let (trio, spread) = Spread::store(&[1, 2]).await;
    let (routed, _) = trio.backends();

    let walked = routed.walk_inputs(spread.root.0).await.expect("walk");
    assert_eq!(walked.absent, vec![spread.nowhere.0]);
    assert!(
        !walked.absent.contains(&spread.replica_only.0)
            && !walked.absent.contains(&spread.previous_only.0)
    );
}

/// Every call the scheduler side makes, in the order a run makes them, against `backend`
/// over a farm that holds `held` and the directory `root`: the answers are the same whatever
/// kind the backend is ([`both_kinds_answer_the_scheduler_calls_alike`]).
async fn scheduler_calls(backend: &Backend, held: (Digest, Bytes), root: (Digest, Bytes)) {
    let (digest, bytes) = held;
    assert_eq!(backend.read_blob(&digest).await.expect("held"), bytes);
    let absent = blob("absent").0;
    assert_eq!(
        backend.find_missing(&[digest, absent]).await.expect("ask"),
        vec![absent]
    );
    assert!(matches!(
        backend.read_blob(&absent).await,
        Err(StorageError::NotFound(_))
    ));
    let walked = backend.walk_inputs(root.0).await.expect("walk");
    assert_eq!((walked.absent, walked.dirs_read), (Vec::new(), 1));

    assert!(
        backend
            .action_result(&digest)
            .await
            .expect("miss")
            .is_none()
    );
    let record = backend
        .prepare_action_result(&ActionResult::default())
        .await
        .expect("prepared");
    backend
        .commit_action_record(digest, record)
        .await
        .expect("committed");
    let (_, hit) = backend
        .action_result(&digest)
        .await
        .expect("lookup")
        .expect("a hit");
    assert_eq!(hit, ActionResult::default());

    assert!(backend.leadership().is_none());
    let text = backend.render_metrics();
    assert!(text.contains("kbf_"));
    assert_in_flight_families(&text);
    assert_eq!(format!("{backend:?}"), "Backend(..)");
}

/// Catches: the routed kind answering the scheduler's calls differently from the others
/// ([`both_kinds_answer_the_scheduler_calls_alike`]), on a farm whose blob, directory and
/// action-cache entry are not on the `--storage` node.
#[tokio::test]
async fn the_routed_kind_answers_the_scheduler_calls_alike() {
    let trio = Arc::new(Trio::new());
    let (backend, _) = trio.backends();
    let elsewhere = |tag: &str| {
        (0..)
            .map(|n| blob(&format!("{tag} {n}")))
            .find(|b| trio.map.write_owners(&b.0)[0] != MemberId(1))
            .expect("a blob owned by another node")
    };
    let held = elsewhere("an action");
    let root = dir(&[], &[]);
    trio.store(&held).await;
    trio.store(&root).await;
    scheduler_calls(&backend, held, root).await;
}

/// Catches: a routed prepare that takes a malformed result, a tree that is not one, or a
/// tree no node holds for a result it can store: each is refused as the cache refuses it.
#[tokio::test]
async fn the_routed_prepare_refuses_what_it_cannot_read() {
    use kbf_proto::reapi::{OutputDirectory, OutputFile};
    let trio = Arc::new(Trio::new());
    let (backend, _) = trio.backends();

    let malformed = ActionResult {
        output_files: vec![OutputFile {
            path: "f".to_owned(),
            digest: Some(kbf_proto::reapi::Digest {
                hash: "not hex".to_owned(),
                size_bytes: 1,
            }),
            ..OutputFile::default()
        }],
        ..ActionResult::default()
    };
    let e = backend.prepare_action_result(&malformed).await.unwrap_err();
    assert!(matches!(e, StorageError::Invalid(_)), "{e:?}");

    let junk = blob("not a Tree \u{ff}\u{ff}");
    trio.store(&junk).await;
    let tree_result = |tree: &Digest| ActionResult {
        output_directories: vec![OutputDirectory {
            path: "d".to_owned(),
            tree_digest: Some(proto(tree)),
            ..OutputDirectory::default()
        }],
        ..ActionResult::default()
    };
    let e = backend
        .prepare_action_result(&tree_result(&junk.0))
        .await
        .unwrap_err();
    assert!(matches!(e, StorageError::Invalid(m) if m.contains("does not decode")));

    let e = backend
        .prepare_action_result(&tree_result(&blob("a tree nobody holds").0))
        .await
        .unwrap_err();
    assert!(matches!(e, StorageError::NotFound(_)), "{e:?}");
}

/// Catches: a routed backend that routes before the map has two members (a frontend whose
/// farm is one node would then dial an address the node does not answer on), or never:
/// with a one-member map the calls go to the `--storage` node.
#[tokio::test]
async fn the_routed_kind_uses_the_storage_node_until_the_map_has_two_members() {
    struct One(Trio);
    impl MapSource for One {
        fn current(&self) -> Option<MapView> {
            // A farm of one member, which is not the node `--storage` names.
            let member = self.0.map.members()[1].clone();
            let map = Map::new(1, vec![member], Rule::Rendezvous, 1, 1, Vec::new()).expect("map");
            Some(MapView {
                map: Arc::new(map),
                previous: None,
                earlier: Vec::new(),
            })
        }
        fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
            self.0.refetch(at_least)
        }
    }
    let trio = Arc::new(Trio::new());
    let first = Arc::clone(&trio.nodes[&MemberId(1)]);
    let alone = Arc::new(One(Trio::new()));
    let routed = Arc::new(Routed::new(alone, trio.clone(), trio.clone()));
    assert!(!routed.active());
    let backend = Backend::routed(routed, Arc::clone(&first));
    // Whatever the map would say, every call is the one node's: the blob and the
    // directory are held there alone, though the map would put them elsewhere.
    let elsewhere = |tag: &str| {
        (0..)
            .map(|n| blob(&format!("{tag} {n}")))
            .find(|b| trio.map.write_owners(&b.0)[0] != MemberId(1))
            .expect("a blob owned by another node")
    };
    let held = elsewhere("held");
    let root = dir(&[], &[]);
    for stored in [&held, &root] {
        first.put(stored.1.clone()).await.expect("stored");
    }
    scheduler_calls(&backend, held, root).await;
}

/// Catches: a frontend backend whose `/metrics` text leaves out `kbf_bytes_in_flight` or
/// `kbf_bytes_in_flight_peak` (the proxy kinds once rendered only their own counters).
///
/// Catches: `Remote::render_metrics` returning `self.metrics.render()` alone.
fn assert_in_flight_families(text: &str) {
    for family in ["kbf_bytes_in_flight", "kbf_bytes_in_flight_peak"] {
        assert!(
            text.contains(&format!("# TYPE {family} gauge\n{family} ")),
            "{family} missing from\n{text}"
        );
    }
}

/// A tree of a root over three directories of ten files each, every blob stored on the node
/// the map writes it to; the walk of it reads four directories and asks about 30 files.
async fn stored_tree(trio: &Trio) -> (Digest, usize) {
    let files: Vec<_> = (0..30).map(|n| blob(&format!("file {n}"))).collect();
    let subs: Vec<_> = files
        .chunks(10)
        .map(|chunk| dir(&chunk.iter().map(|f| f.0).collect::<Vec<_>>(), &[]))
        .collect();
    let root = dir(&[], &subs.iter().map(|d| d.0).collect::<Vec<_>>());
    for held in files.iter().chain(&subs).chain([&root]) {
        trio.store(held).await;
    }
    (root.0, 4)
}

/// Catches: the routed kind's walk forgetting nothing the router can vouch for, or never
/// remembering: the second walk of a tree the farm held (a build's next Execute over the
/// same toolchain) skips it, with no read and no FindMissing, because the router gave the
/// map's epoch to the walk. Catches: `Blobs::epoch` for the router returning `None` (the
/// memory would never fire and every other test stays green): the second walk reads all
/// four directories again. Under a map with a member of unknown boot there is no epoch
/// and the second walk reads again.
#[tokio::test]
async fn the_routed_walk_skips_a_tree_it_found_held() {
    let trio = Arc::new(Trio::new().booted());
    let (root, dirs) = stored_tree(&trio).await;
    let (routed, _) = trio.backends();

    let first = routed.walk_inputs(root).await.expect("first walk");
    assert!(first.absent.is_empty(), "{:?}", first.absent);
    assert_eq!((first.dirs_read, first.dirs_skipped), (dirs as u64, 0));
    assert_eq!((first.files_checked, first.file_asks), (30, 1));

    let second = routed.walk_inputs(root).await.expect("second walk");
    assert!(second.absent.is_empty());
    assert_eq!(
        (second.dirs_read, second.dirs_skipped),
        (0, 1),
        "the root was verified, so it alone is skipped"
    );
    assert_eq!((second.files_checked, second.file_asks), (0, 0));

    // A member whose boot is unknown: the router cannot tell a restart, so nothing is kept.
    let unknown = Arc::new(Trio::new());
    let (root, dirs) = stored_tree(&unknown).await;
    let (routed, _) = unknown.backends();
    routed.walk_inputs(root).await.expect("first walk");
    let again = routed.walk_inputs(root).await.expect("second walk");
    // Nothing is believed verified: every Directory is taken again (from the content
    // cache, so none is read, and each is asked about) and every file is asked about.
    assert_eq!((again.dirs_skipped, again.files_checked), (0, 30));
    assert_eq!(
        (
            again.dirs_read,
            again.dir_cache_hits,
            again.dirs_present_checked
        ),
        (0, dirs as u64, dirs as u64)
    );
}

/// Catches: a single storage node's walk remembering what it found held. Nothing about
/// one node says that a restart or a lost blob did not happen between two walks, so
/// [`Backend::remote`] has no epoch and every walk reads the whole tree. Catches: `Held`
/// answering an epoch (`Some(1)`): the second walk skips the root.
#[tokio::test]
async fn a_walk_over_one_storage_node_never_skips() {
    let cache = Arc::new(Cache::memory());
    let held = blob("held");
    let sub = dir(&[held.0], &[]);
    let root = dir(&[], &[sub.0]);
    put(&cache, &[&held, &sub, &root]).await;
    let (_, remote) = both(&cache);

    for round in 0..3 {
        let walked = remote.walk_inputs(root.0).await.expect("walk");
        assert!(walked.absent.is_empty(), "round {round}");
        // The first walk reads both Directories; the later ones take them from the
        // content cache and ask the node about them, but skip nothing as verified.
        let (read, cached) = if round == 0 { (2, 0) } else { (0, 2) };
        assert_eq!(
            (walked.dirs_read, walked.dirs_skipped, walked.files_checked),
            (read, 0, 1),
            "round {round}"
        );
        assert_eq!(
            (walked.dir_cache_hits, walked.dirs_present_checked),
            (cached, cached),
            "round {round}"
        );
    }
}

#[path = "backend_packed_tests.rs"]
mod packed;
