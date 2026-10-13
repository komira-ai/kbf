//! A CAS server for the engine's tests: ByteStream and FindMissing over blobs in
//! memory, which counts what it is asked and can hold reads and writes at a gate, pause
//! a read after its first message, refuse FindMissing, and fail writes.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::channel::mpsc;
use futures::{SinkExt as _, Stream};
use kbf_proto::google::bytestream::byte_stream_server::{ByteStream, ByteStreamServer};
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use kbf_proto::reapi::content_addressable_storage_server::{
    ContentAddressableStorage, ContentAddressableStorageServer,
};
use kbf_proto::reapi::{
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, Digest, FindMissingBlobsRequest, FindMissingBlobsResponse,
    GetChunkMappingRequest, GetChunkMappingResponse, GetTreeRequest, GetTreeResponse,
    RegisterChunkMappingRequest, RegisterChunkMappingResponse, SpliceBlobRequest,
    SpliceBlobResponse, SplitBlobRequest, SplitBlobResponse,
};
use kbf_transfer::{digest_of, label};
use tokio::sync::watch;
use tonic::service::Routes;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Status, Streaming};

/// How the server answers FindMissing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Find {
    /// From the blobs it holds.
    Serve,
    /// UNIMPLEMENTED, as a worker listener without the CAS service.
    Unimplemented,
    /// UNAVAILABLE `n` times, then served.
    UnavailableFirst(usize),
    /// INVALID_ARGUMENT, a refusal no retry helps.
    Refuse,
}

#[derive(Debug)]
pub struct Fake {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Each ByteStream read asked: the blob and the offset.
    pub reads: Mutex<Vec<(String, i64)>>,
    /// Reads being served now, and the most at once.
    pub active: AtomicUsize,
    pub most: AtomicUsize,
    /// Requests of any kind being served now (reads, writes and FindMissing calls), and
    /// the most at once.
    pub requests: AtomicUsize,
    pub most_requests: AtomicUsize,
    /// Reads wait for this to open before their first message (or, when `pause` is
    /// set, after it); writes wait for it before they commit.
    gate: watch::Sender<bool>,
    pub pause: AtomicBool,
    /// Bytes per ReadResponse.
    pub message_bytes: AtomicUsize,
    pub find: Mutex<Find>,
    /// The digest count of each FindMissing call that reached the server.
    pub finds: Mutex<Vec<usize>>,
    /// Each blob a write committed, in order.
    pub writes: Mutex<Vec<String>>,
    /// Writes to fail with UNAVAILABLE before one succeeds.
    pub fail_writes: AtomicUsize,
    /// The most bytes one WriteRequest carried.
    pub largest_write: AtomicUsize,
}

impl Default for Fake {
    fn default() -> Self {
        Self {
            blobs: Mutex::default(),
            reads: Mutex::default(),
            active: AtomicUsize::new(0),
            most: AtomicUsize::new(0),
            requests: AtomicUsize::new(0),
            most_requests: AtomicUsize::new(0),
            gate: watch::channel(true).0,
            pause: AtomicBool::new(false),
            message_bytes: AtomicUsize::new(64 << 10),
            find: Mutex::new(Find::Serve),
            finds: Mutex::default(),
            writes: Mutex::default(),
            fail_writes: AtomicUsize::new(0),
            largest_write: AtomicUsize::new(0),
        }
    }
}

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Fake {
    pub fn insert(&self, bytes: impl Into<Vec<u8>>) -> Digest {
        let bytes = bytes.into();
        let digest = digest_of(&bytes);
        lock(&self.blobs).insert(label(&digest), bytes);
        digest
    }

    /// Stores `bytes` under `digest`, whatever they hash to.
    pub fn lie(&self, digest: &Digest, bytes: Vec<u8>) {
        lock(&self.blobs).insert(label(digest), bytes);
    }

    pub fn remove(&self, digest: &Digest) {
        lock(&self.blobs).remove(&label(digest));
    }

    pub fn holds(&self, digest: &Digest) -> bool {
        lock(&self.blobs).contains_key(&label(digest))
    }

    /// Holds every read before its first message (after it, with `pause`).
    pub fn close(&self) {
        self.gate.send_replace(false);
    }

    pub fn open(&self) {
        self.gate.send_replace(true);
    }

    /// The reads of `digest`, by offset.
    pub fn reads_of(&self, digest: &Digest) -> Vec<i64> {
        let name = label(digest);
        lock(&self.reads)
            .iter()
            .filter(|(n, _)| *n == name)
            .map(|(_, offset)| *offset)
            .collect()
    }

    /// Serves this fake on a loopback port.
    pub async fn serve(self: &Arc<Self>) -> SocketAddr {
        let routes = Routes::new(ByteStreamServer::new(Shared(Arc::clone(self)))).add_service(
            ContentAddressableStorageServer::new(Shared(Arc::clone(self))),
        );
        serve(routes).await
    }
}

/// Serves `routes` on a loopback port.
pub async fn serve(routes: Routes) -> SocketAddr {
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("addr");
    tokio::spawn(
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming),
    );
    addr
}

struct Shared(Arc<Fake>);

/// Counts a request of any kind as being served until dropped.
struct Serving(Arc<Fake>);

impl Serving {
    fn start(fake: &Arc<Fake>) -> Self {
        let now = fake.requests.fetch_add(1, Ordering::SeqCst) + 1;
        fake.most_requests.fetch_max(now, Ordering::SeqCst);
        Self(Arc::clone(fake))
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.0.requests.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Counts a read as active until dropped.
struct Active(Arc<Fake>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

type ReadStream = Pin<Box<dyn Stream<Item = Result<ReadResponse, Status>> + Send>>;

#[tonic::async_trait]
impl ByteStream for Shared {
    type ReadStream = ReadStream;

    async fn read(&self, request: Request<ReadRequest>) -> Result<Response<ReadStream>, Status> {
        let fake = Arc::clone(&self.0);
        let request = request.into_inner();
        let name = request
            .resource_name
            .strip_prefix("blobs/")
            .unwrap_or_default()
            .to_owned();
        lock(&fake.reads).push((name.clone(), request.read_offset));
        let serving = Serving::start(&fake);
        let now = fake.active.fetch_add(1, Ordering::SeqCst) + 1;
        fake.most.fetch_max(now, Ordering::SeqCst);
        let active = Active(Arc::clone(&fake));
        let Some(bytes) = lock(&fake.blobs).get(&name).cloned() else {
            return Err(Status::not_found(name));
        };
        let (mut sender, messages) = mpsc::channel(1);
        tokio::spawn(async move {
            // Dropped before `sender`, so the read stops counting before its stream
            // ends.
            let _active = active;
            let _serving = serving;
            let mut gate = fake.gate.subscribe();
            let pause = fake.pause.load(Ordering::SeqCst);
            if !pause {
                let _ = gate.wait_for(|open| *open).await;
            }
            let size = fake.message_bytes.load(Ordering::SeqCst);
            let start = usize::try_from(request.read_offset)
                .unwrap_or(0)
                .min(bytes.len());
            for (i, piece) in bytes[start..].chunks(size).enumerate() {
                let message = Ok(ReadResponse {
                    data: piece.to_vec(),
                });
                if sender.send(message).await.is_err() {
                    return;
                }
                if pause && i == 0 && start == 0 {
                    let _ = gate.wait_for(|open| *open).await;
                }
            }
        });
        Ok(Response::new(Box::pin(messages)))
    }

    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let fake = &self.0;
        let _serving = Serving::start(fake);
        let mut stream = request.into_inner();
        let mut bytes = Vec::new();
        while let Some(message) = stream.message().await? {
            fake.largest_write
                .fetch_max(message.data.len(), Ordering::SeqCst);
            bytes.extend_from_slice(&message.data);
        }
        let _ = fake.gate.subscribe().wait_for(|open| *open).await;
        let failing = fake.fail_writes.load(Ordering::SeqCst);
        if failing > 0 {
            fake.fail_writes.store(failing - 1, Ordering::SeqCst);
            return Err(Status::unavailable("write refused for the test"));
        }
        let digest = fake.insert(bytes);
        lock(&fake.writes).push(label(&digest));
        Ok(Response::new(WriteResponse {
            committed_size: digest.size_bytes,
        }))
    }

    async fn query_write_status(
        &self,
        _: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }
}

#[tonic::async_trait]
impl ContentAddressableStorage for Shared {
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        let fake = &self.0;
        let _serving = Serving::start(fake);
        let digests = request.into_inner().blob_digests;
        lock(&fake.finds).push(digests.len());
        let mode = *lock(&fake.find);
        match mode {
            Find::Serve => {}
            Find::Unimplemented => return Err(Status::unimplemented("no CAS service here")),
            Find::Refuse => return Err(Status::invalid_argument("refused for the test")),
            Find::UnavailableFirst(n) => {
                *lock(&fake.find) = if n > 1 {
                    Find::UnavailableFirst(n - 1)
                } else {
                    Find::Serve
                };
                return Err(Status::unavailable("busy for the test"));
            }
        }
        let missing = digests.into_iter().filter(|d| !fake.holds(d)).collect();
        Ok(Response::new(FindMissingBlobsResponse {
            missing_blob_digests: missing,
        }))
    }

    async fn batch_update_blobs(
        &self,
        _: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }

    async fn batch_read_blobs(
        &self,
        _: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }

    type GetTreeStream = Pin<Box<dyn Stream<Item = Result<GetTreeResponse, Status>> + Send>>;

    async fn get_tree(
        &self,
        _: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        Err(Status::unimplemented("not used"))
    }

    async fn split_blob(
        &self,
        _: Request<SplitBlobRequest>,
    ) -> Result<Response<SplitBlobResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }

    type GetChunkMappingStream =
        Pin<Box<dyn Stream<Item = Result<GetChunkMappingResponse, Status>> + Send>>;

    async fn get_chunk_mapping(
        &self,
        _: Request<GetChunkMappingRequest>,
    ) -> Result<Response<Self::GetChunkMappingStream>, Status> {
        Err(Status::unimplemented("not used"))
    }

    async fn splice_blob(
        &self,
        _: Request<SpliceBlobRequest>,
    ) -> Result<Response<SpliceBlobResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }

    async fn register_chunk_mapping(
        &self,
        _: Request<Streaming<RegisterChunkMappingRequest>>,
    ) -> Result<Response<RegisterChunkMappingResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }
}

/// Polls `done` every 10 ms until it holds, for at most 30 s.
pub async fn until(what: &str, done: impl Fn() -> bool) {
    assert!(within(30, done).await, "not within 30 s: {what}");
}

/// Polls `done` every 10 ms until it holds or `secs` seconds pass; returns whether it
/// held. A test asserts what it then sees, so a defect fails with the value it made.
pub async fn within(secs: u64, done: impl Fn() -> bool) -> bool {
    for _ in 0..secs * 100 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    done()
}
