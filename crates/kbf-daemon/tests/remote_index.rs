//! [`RemoteIndex`] and [`PackedCas`] against the frontend's `Locate` logic
//! (`kbf_server::pack_locate`) on a loopback port, over a memory pack index and a memory object
//! store: what a daemon is told about where blobs lie, how it reads them from the packs, and
//! that a frontend that serves nothing (or fails) leaves every read where it was.
//!
//! The listener's mutual TLS and role table are `kbf-server`'s (`tests/frontend/pack_locate.rs`);
//! the service here is `PackLocateService::answer` behind a plain `PackIndex` server.

mod support;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use kbf_daemon::cas::{Cas, CasError, digest_of, label};
use kbf_daemon::{PackCommit, PackWriter, PackedCas, RemoteIndex, WriteSettings};
use kbf_objstore::{Capabilities, MemoryStore};
use kbf_pack::{Blob, PackError, PackKeys, Packer, PackerConfig, Reader, ReaderConfig, WriterKind};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{
    BlobDigest, Location, MAX_BATCH, NewPack, PackBlob, PackIndex, PackIndexError, Recorded,
};
use kbf_proto::reapi::Digest;
use kbf_proto::worker::pack_index_server::{PackIndex as PackIndexRpc, PackIndexServer};
use kbf_proto::worker::{
    CommitPackRequest, CommitPackResponse, PackLocateRequest, PackLocateResponse,
};
use kbf_server::Peers;
use kbf_server::pack_locate::PackLocateService;
use kbf_state::{ManualClock, StoreError};
use support::memory::MemoryCas;
use tokio::runtime::Handle;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Code, Request, Response, Status};

/// What the frontend does with a `Locate` call.
#[derive(Clone, Copy)]
enum Mode {
    /// Answers from its index, as `PackLocateService` does.
    Serve,
    /// Fails with this code.
    Refuse(Code),
    /// Answers with no location at all, whatever was asked.
    Empty,
}

/// The `PackIndex` service of a frontend, without the listener's admission.
struct Frontend {
    service: PackLocateService,
    mode: Mutex<Mode>,
    calls: AtomicUsize,
    /// The code `CommitPack` fails with, or `None` to answer [`COMMITTED`].
    commit: Mutex<Option<Code>>,
    /// The `CommitPack` requests the frontend took, in order.
    commits: Mutex<Vec<CommitPackRequest>>,
}

/// What a `CommitPack` that records the pack answers.
const COMMITTED: CommitPackResponse = CommitPackResponse {
    created: true,
    indexed: 3,
};

impl Frontend {
    fn set_commit(&self, code: Option<Code>) {
        *self.commit.lock().unwrap_or_else(PoisonError::into_inner) = code;
    }

    fn commits(&self) -> Vec<CommitPackRequest> {
        let taken = self.commits.lock().unwrap_or_else(PoisonError::into_inner);
        taken.clone()
    }

    fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[tonic::async_trait]
impl PackIndexRpc for Frontend {
    async fn locate(
        &self,
        request: Request<PackLocateRequest>,
    ) -> Result<Response<PackLocateResponse>, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mode = *self.mode.lock().unwrap_or_else(PoisonError::into_inner);
        match mode {
            Mode::Serve => {
                let answer = self.service.answer(&request.get_ref().digests).await;
                answer.map(Response::new)
            }
            Mode::Refuse(code) => Err(Status::new(code, "scripted")),
            Mode::Empty => Ok(Response::new(PackLocateResponse::default())),
        }
    }

    /// Notes the request, then answers as scripted: [`COMMITTED`], or the code set.
    async fn commit_pack(
        &self,
        request: Request<CommitPackRequest>,
    ) -> Result<Response<CommitPackResponse>, Status> {
        let taken = request.into_inner();
        self.commits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(taken);
        let code = *self.commit.lock().unwrap_or_else(PoisonError::into_inner);
        match code {
            None => Ok(Response::new(COMMITTED)),
            Some(code) => Err(Status::new(code, "scripted")),
        }
    }
}

/// A frontend on a loopback port, a memory index and object store behind it, a packer over
/// them, and the daemon's index and its connection.
struct World {
    store: Arc<MemoryStore>,
    index: Arc<MemoryPackIndex>,
    packer: Packer<MemoryStore>,
    frontend: Arc<Frontend>,
    remote: Arc<RemoteIndex>,
}

impl World {
    async fn new() -> Self {
        let store = Arc::new(MemoryStore::new(Capabilities {
            conditional_put: true,
            object_lock: false,
        }));
        let index = Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
        let shared: Arc<dyn PackIndex> = index.clone();
        let keys = PackKeys::new(WriterKind::Daemon, "node1", "boot1").expect("keys");
        let packer = Packer::new(
            Arc::clone(&store),
            Arc::clone(&shared),
            keys,
            PackerConfig::default(),
        );
        let frontend = Arc::new(Frontend {
            service: PackLocateService::new(shared, Peers::Unauthenticated),
            mode: Mutex::new(Mode::Serve),
            calls: AtomicUsize::new(0),
            commit: Mutex::new(None),
            commits: Mutex::new(Vec::new()),
        });
        let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let addr = incoming.local_addr().expect("address");
        let server = PackIndexServer::from_arc(Arc::clone(&frontend));
        tokio::spawn(
            Server::builder()
                .add_service(server)
                .serve_with_incoming(incoming),
        );
        let endpoint = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint");
        let channel: Channel = endpoint.connect_lazy();
        Self {
            store,
            index,
            packer: packer.expect("packer"),
            frontend,
            remote: Arc::new(RemoteIndex::new(channel, Handle::current())),
        }
    }

    /// Packs `bytes` and returns their digest.
    async fn pack(&self, bytes: &'static [u8]) -> Digest {
        let blob = Blob {
            digest: kbf_pack::blob_digest(bytes),
            bytes: Bytes::from_static(bytes),
        };
        self.packer.pack(vec![blob]).await.expect("packed");
        digest_of(bytes)
    }

    /// A data plane over a CAS that holds nothing, whose reads go to the packs first.
    fn plane(&self) -> (Arc<MemoryCas>, PackedCas<MemoryCas>) {
        let remote: Arc<dyn PackIndex> = self.remote.clone();
        let store = Arc::clone(&self.store);
        let reader = Reader::new(store, remote, None, ReaderConfig::default());
        let underneath = Arc::new(MemoryCas::default());
        (
            Arc::clone(&underneath),
            PackedCas::new(underneath, Arc::new(reader)),
        )
    }

    /// `pack` recorded by the daemon's index, on a thread that may block.
    async fn record(&self, pack: NewPack) -> Result<Recorded, PackIndexError> {
        let remote = Arc::clone(&self.remote);
        let work = move || remote.record_pack(&pack);
        let joined = tokio::task::spawn_blocking(work).await;
        joined.expect("joined")
    }

    /// `digests` located by the daemon's index, on a thread that may block.
    async fn locate(
        &self,
        digests: Vec<BlobDigest>,
    ) -> Result<Vec<Option<Location>>, PackIndexError> {
        let remote = Arc::clone(&self.remote);
        let work = move || remote.locate(&digests);
        let joined = tokio::task::spawn_blocking(work).await;
        joined.expect("joined")
    }
}

fn blob(bytes: &[u8]) -> BlobDigest {
    kbf_pack::blob_digest(bytes)
}

/// Asserts that the daemon's index is told where each blob is, in the order asked: the pack, the
/// offset and the length of a packed blob, and nothing for one the frontend's index lacks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blobs_are_located_in_the_order_asked() {
    let world = World::new().await;
    world.pack(b"first").await;
    world.pack(b"second").await;
    let asked = vec![blob(b"second"), blob(b"unknown"), blob(b"first")];
    let located = world.locate(asked).await.expect("located");
    let [second, unknown, first] = &located[..] else {
        panic!("three answers: {located:?}");
    };
    let second = second.as_ref().expect("located");
    let first = first.as_ref().expect("located");
    assert_eq!((second.length, first.length), (6, 5));
    assert!(second.pack_key.starts_with("cas/d/node1/boot1/"));
    assert_ne!(second.pack_key, first.pack_key);
    assert_eq!(unknown, &None);
    assert_eq!(first.last_touch_us, 0);
}

/// Asserts that a frontend that serves no locations (UNIMPLEMENTED: started without
/// `--pack-index`; UNAVAILABLE: away) has none to give: every digest is answered as not located,
/// the call is no error, and a call after a served one is the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_that_serves_nothing_has_no_locations_to_give() {
    let world = World::new().await;
    world.pack(b"packed").await;
    let asked = vec![blob(b"packed"), blob(b"other")];
    for code in [Code::Unimplemented, Code::Unimplemented, Code::Unavailable] {
        world.frontend.set(Mode::Refuse(code));
        let located = world.locate(asked.clone()).await.expect("no error");
        assert_eq!(located, [None, None], "{code:?}");
    }
    world.frontend.set(Mode::Serve);
    let located = world.locate(asked.clone()).await.expect("served");
    assert!(located[0].is_some());
    world.frontend.set(Mode::Refuse(Code::Unimplemented));
    let located = world.locate(asked).await.expect("no error");
    assert_eq!(located, [None, None]);
    assert_eq!(world.frontend.calls(), 5);
}

/// Asserts that any other failure is the call's error, naming the status, and so is an answer
/// that does not have one location for each digest asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn any_other_failure_is_the_calls_error() {
    let world = World::new().await;
    let asked = vec![blob(b"a"), blob(b"b")];
    world.frontend.set(Mode::Refuse(Code::Internal));
    let failed = world.locate(asked.clone()).await.expect_err("internal");
    let PackIndexError::Store(StoreError::Failed(why)) = &failed else {
        panic!("a store failure: {failed:?}");
    };
    assert!(why.contains("locate"), "{why}");
    assert!(why.contains("Internal"), "{why}");
    world.frontend.set(Mode::Empty);
    let failed = world.locate(asked).await.expect_err("short answer");
    let PackIndexError::Store(StoreError::Failed(why)) = &failed else {
        panic!("a store failure: {failed:?}");
    };
    assert_eq!(why, "0 locations for 2 digests");
}

/// Asserts that a call names at most the index's batch, without asking the frontend, and that
/// the digests among those named that have no location are what `find_missing` answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_is_bounded_and_find_missing_is_what_locate_cannot_place() {
    let world = World::new().await;
    world.pack(b"held").await;
    let over = vec![blob(b"held"); MAX_BATCH + 1];
    let refused = world.locate(over).await.expect_err("too many");
    let expected = PackIndexError::TooMany {
        given: MAX_BATCH + 1,
        max: MAX_BATCH,
    };
    assert_eq!(refused, expected);
    assert_eq!(world.frontend.calls(), 0);

    let remote = Arc::clone(&world.remote);
    let asked = vec![blob(b"gone"), blob(b"held"), blob(b"never")];
    let work = move || remote.find_missing(&asked);
    let missing = tokio::task::spawn_blocking(work).await.expect("joined");
    assert_eq!(missing, Ok(vec![blob(b"gone"), blob(b"never")]));
}

/// Asserts that a daemon's index touches nothing and says so, and refuses what only the keeper
/// of the index does (the garbage-collection queries and a pack's row), without asking the
/// frontend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemons_index_keeps_nothing() {
    let world = World::new().await;
    let remote = &world.remote;
    assert_eq!(remote.touch(&[blob(b"x")]), Ok(0));
    let refusal = |result: Result<(), PackIndexError>| match result {
        Err(PackIndexError::Store(StoreError::Failed(why))) => why,
        other => format!("not refused: {other:?}"),
    };
    let said = [
        refusal(remote.pack("cas/d/node1/boot1/1").map(drop)),
        refusal(remote.dead_packs(std::time::Duration::ZERO, 1).map(drop)),
        refusal(remote.unreferenced_keys(&[]).map(drop)),
    ];
    for why in said {
        assert_eq!(
            why,
            "a daemon's pack index only locates blobs and commits packs"
        );
    }
    assert_eq!(world.frontend.calls(), 0);
    assert_eq!(world.frontend.commits(), []);
}

/// The pack `key` that the writer `writer` stored, holding the blob `x` at offset 0.
fn new_pack(key: &str, writer: &str) -> NewPack {
    NewPack {
        key: key.to_owned(),
        writer: writer.to_owned(),
        size: 9,
        blobs: vec![PackBlob {
            digest: blob(b"x"),
            offset: 0,
        }],
    }
}

/// Asserts that recording a pack commits it to the frontend as the node, not the writer name:
/// the request carries the key, the size, each blob's digest as the wire spells it with its
/// offset, and the node id without its `d/`, and the answer is the frontend's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recorded_pack_is_committed_as_the_node() {
    let world = World::new().await;
    let recorded = world
        .record(new_pack("cas/d/node1/boot1/1", "d/node1"))
        .await;
    assert_eq!(
        recorded,
        Ok(Recorded {
            created: true,
            indexed: 3
        })
    );
    let [sent] = &world.frontend.commits()[..] else {
        panic!("one commit");
    };
    assert_eq!(sent.pack, "cas/d/node1/boot1/1");
    assert_eq!((sent.size, sent.writer.as_str()), (9, "node1"));
    let [packed] = &sent.blobs[..] else {
        panic!("one blob");
    };
    assert_eq!(packed.digest, Some(digest_of(b"x")));
    assert_eq!(packed.offset, 0);
}

/// Asserts that a writer that is not a daemon's (no `d/`) is refused without asking the
/// frontend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pack_of_another_writer_is_not_committed() {
    let world = World::new().await;
    let refused = world.record(new_pack("cas/d/node1/boot1/1", "node1")).await;
    let Err(PackIndexError::Store(StoreError::Failed(why))) = refused else {
        panic!("a store failure: {refused:?}");
    };
    assert!(why.contains("not a daemon's writer name"), "{why}");
    assert_eq!(world.frontend.commits(), []);
}

/// Asserts that each answer of a frontend that does not record the pack is the error it means:
/// UNIMPLEMENTED the index takes no such call, ALREADY_EXISTS a conflict on the key,
/// PERMISSION_DENIED and INVALID_ARGUMENT a refusal, and any other status a store failure that
/// names the commit and the status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commit_the_frontend_does_not_record_fails_as_what_it_means() {
    let world = World::new().await;
    let key = "cas/d/node1/boot1/1";
    let cases = [
        (Code::Unimplemented, PackIndexError::Unsupported),
        (
            Code::AlreadyExists,
            PackIndexError::Conflict {
                key: key.to_owned(),
            },
        ),
    ];
    for (code, expected) in cases {
        world.frontend.set_commit(Some(code));
        let failed = world.record(new_pack(key, "d/node1")).await;
        assert_eq!(failed, Err(expected), "{code:?}");
    }
    for code in [Code::PermissionDenied, Code::InvalidArgument] {
        world.frontend.set_commit(Some(code));
        let failed = world.record(new_pack(key, "d/node1")).await;
        let Err(PackIndexError::Refused(why)) = failed else {
            panic!("a refusal: {failed:?}");
        };
        assert!(why.contains("commit:"), "{why}");
    }
    for code in [Code::Unavailable, Code::FailedPrecondition, Code::Internal] {
        world.frontend.set_commit(Some(code));
        let failed = world.record(new_pack(key, "d/node1")).await;
        let Err(PackIndexError::Store(StoreError::Failed(why))) = failed else {
            panic!("a store failure: {failed:?}");
        };
        assert!(why.contains("commit:"), "{why}");
    }
}

/// Asserts that a packer over the daemon's index commits a pack it stored through the frontend
/// (from the blocking pool, where the index's calls may wait), and that the retries are the
/// packer's: a refusal ends the attempts at the first call and is the packer's record error
/// with the refusal as its source, and a failure that may pass is tried `put_attempts` times.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_packer_commits_through_the_frontend_and_does_not_repeat_a_refusal() {
    let world = World::new().await;
    let remote: Arc<dyn PackIndex> = world.remote.clone();
    let keys = PackKeys::new(WriterKind::Daemon, "node1", "boot1").expect("keys");
    let config = PackerConfig::default();
    let attempts = usize::try_from(config.put_attempts).expect("attempts");
    let packer = Packer::new(Arc::clone(&world.store), remote, keys, config).expect("packer");
    let one = |bytes: &'static [u8]| {
        vec![Blob {
            digest: kbf_pack::blob_digest(bytes),
            bytes: Bytes::from_static(bytes),
        }]
    };

    let packed = packer.pack(one(b"committed")).await.expect("packed");
    assert_eq!(packed.packs.len(), 1);
    assert_eq!(world.frontend.commits().len(), 1);
    assert_eq!(world.frontend.commits()[0].writer, "node1");

    world.frontend.set_commit(Some(Code::Unimplemented));
    let refused = packer.pack(one(b"refused")).await.expect_err("refused");
    let PackError::Record { source, .. } = &refused else {
        panic!("a record error: {refused:?}");
    };
    assert_eq!(source, &PackIndexError::Unsupported);
    assert_eq!(world.frontend.commits().len(), 2);

    world.frontend.set_commit(Some(Code::Unavailable));
    let failed = packer.pack(one(b"flaky")).await.expect_err("failed");
    assert!(matches!(failed, PackError::Record { .. }), "{failed:?}");
    assert_eq!(world.frontend.commits().len(), 2 + attempts);
}

/// Asserts that a blob in a pack the frontend locates is read from the pack, through the
/// frontend's `Locate` and a ranged read of the object store, and not from the plane underneath,
/// which holds nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_located_blob_is_read_from_its_pack() {
    let world = World::new().await;
    let digest = world.pack(b"in a pack, read by a daemon").await;
    let (underneath, cas) = world.plane();
    assert_eq!(
        cas.get(&digest).await,
        Ok(b"in a pack, read by a daemon".to_vec())
    );
    assert_eq!(underneath.blob(&digest), None);
    assert_eq!(cas.stats().served, 1);
    assert_eq!(world.frontend.calls(), 1);
}

/// Asserts that a frontend that serves nothing or fails leaves every read where it was: the
/// blob the plane underneath holds is read from it, for UNIMPLEMENTED (no `--pack-index`),
/// UNAVAILABLE and another failure alike (PERMISSION_DENIED, which a frontend of the previous
/// release answers, among them), each read counted as read elsewhere or as unasked, and a blob
/// nowhere is that plane's `Missing`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_that_serves_nothing_leaves_reads_where_they_were() {
    let world = World::new().await;
    let (underneath, cas) = world.plane();
    let digest = underneath.insert(b"held by the storage nodes");
    let held = b"held by the storage nodes".to_vec();
    for code in [
        Code::Unimplemented,
        Code::Unavailable,
        Code::Internal,
        Code::PermissionDenied,
    ] {
        world.frontend.set(Mode::Refuse(code));
        let read = cas.get(&digest).await;
        assert_eq!(read, Ok(held.clone()), "{code:?}");
    }
    let stats = cas.stats();
    assert_eq!((stats.served, stats.elsewhere, stats.unasked), (0, 2, 2));
    let nowhere = digest_of(b"nowhere");
    let missing = CasError::Missing(label(&nowhere));
    assert_eq!(cas.get(&nowhere).await, Err(missing));
}

/// Asserts that a blob the frontend's index names in a pack the store does not hold fails the
/// read as `Unavailable`, and does not go to the plane underneath, which holds the blob.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_located_blob_in_a_lost_pack_fails_the_read() {
    let world = World::new().await;
    let (underneath, cas) = world.plane();
    let digest = underneath.insert(b"lost");
    let pack = NewPack {
        key: "cas/d/node1/boot1/00000000000000ff".to_owned(),
        writer: "node1".to_owned(),
        size: 4,
        blobs: vec![PackBlob {
            digest: blob(b"lost"),
            offset: 0,
        }],
    };
    world.index.record_pack(&pack).expect("recorded");
    let failed = cas.get(&digest).await;
    assert!(
        matches!(failed, Err(CasError::Unavailable(..))),
        "{failed:?}"
    );
    assert_eq!(cas.stats().lost, 1);
}

/// Asserts that a plane that writes packs over the daemon's index commits the pack through the
/// frontend as the node, with the digests and offsets of its blobs, before the put returns and
/// without the plane underneath being asked to store the blob; and that a frontend that answers
/// the commit UNIMPLEMENTED has the blob stored by the plane underneath, which the put returns
/// the digest of, and turns writing off for the plane (the next put sends no commit).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plane_commits_through_the_frontend_until_it_is_unimplemented() {
    let world = World::new().await;
    let remote: Arc<dyn PackIndex> = world.remote.clone();
    let keys = PackKeys::new(WriterKind::Daemon, "node1", "boot2").expect("keys");
    let store = Arc::clone(&world.store);
    let writer = PackWriter::new(store, remote, keys, WriteSettings::default()).expect("writer");
    let commit = PackCommit::new();
    commit.welcomed(true);
    let (underneath, plane) = world.plane();
    let plane = plane.with_writes(Arc::new(writer), commit.clone());

    let digest = plane.put(b"committed".to_vec()).await.expect("put");
    assert_eq!(digest, digest_of(b"committed"));
    let [sent] = &world.frontend.commits()[..] else {
        panic!("one commit");
    };
    assert_eq!(sent.writer, "node1");
    assert!(sent.pack.starts_with("cas/d/node1/boot2/"), "{}", sent.pack);
    let [packed] = &sent.blobs[..] else {
        panic!("one blob");
    };
    assert_eq!(packed.digest, Some(digest));
    assert_eq!(underneath.puts(), 0);

    world.frontend.set_commit(Some(Code::Unimplemented));
    let refused = plane.put(b"refused".to_vec()).await.expect("put");
    assert_eq!(underneath.blob(&refused), Some(b"refused".to_vec()));
    assert_eq!(world.frontend.commits().len(), 2);
    assert!(!commit.enabled());
    plane.put(b"after".to_vec()).await.expect("put");
    assert_eq!(world.frontend.commits().len(), 2);
    assert_eq!(underneath.puts(), 2);
}
