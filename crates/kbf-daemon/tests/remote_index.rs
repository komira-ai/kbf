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
use kbf_daemon::{PackedCas, RemoteIndex};
use kbf_objstore::{Capabilities, MemoryStore};
use kbf_pack::{Blob, PackKeys, Packer, PackerConfig, Reader, ReaderConfig, WriterKind};
use kbf_proto::reapi::Digest;
use kbf_proto::worker::pack_index_server::{PackIndex as PackIndexRpc, PackIndexServer};
use kbf_proto::worker::{
    CommitPackRequest, CommitPackResponse, PackLocateRequest, PackLocateResponse,
};
use kbf_server::Peers;
use kbf_server::pack_locate::PackLocateService;
use kbf_state::pack_index::memory::MemoryPackIndex;
use kbf_state::{
    BlobDigest, Location, MAX_BATCH, ManualClock, NewPack, PackBlob, PackIndex, PackIndexError,
    StoreError,
};
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
}

impl Frontend {
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

    /// This frontend takes no commits: these tests are of the reads.
    async fn commit_pack(
        &self,
        _: Request<CommitPackRequest>,
    ) -> Result<Response<CommitPackResponse>, Status> {
        Err(Status::unimplemented("this frontend takes no commits"))
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

/// Asserts that a daemon's index touches nothing and says so, and refuses what only a writer
/// of packs does (recording a pack and the garbage-collection queries), without asking the
/// frontend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemons_index_only_locates() {
    let world = World::new().await;
    let remote = &world.remote;
    assert_eq!(remote.touch(&[blob(b"x")]), Ok(0));
    let refusal = |result: Result<(), PackIndexError>| match result {
        Err(PackIndexError::Store(StoreError::Failed(why))) => why,
        other => format!("not refused: {other:?}"),
    };
    let pack = NewPack {
        key: "cas/d/node1/boot1/1".to_owned(),
        writer: "node1".to_owned(),
        size: 1,
        blobs: vec![PackBlob {
            digest: blob(b"x"),
            offset: 0,
        }],
    };
    let said = [
        refusal(remote.record_pack(&pack).map(drop)),
        refusal(remote.pack("cas/d/node1/boot1/1").map(drop)),
        refusal(remote.dead_packs(std::time::Duration::ZERO, 1).map(drop)),
        refusal(remote.unreferenced_keys(&[]).map(drop)),
    ];
    for why in said {
        assert_eq!(why, "a daemon's pack index only locates blobs");
    }
    assert_eq!(world.frontend.calls(), 0);
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
