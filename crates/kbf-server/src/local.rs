//! The server's blob store as calls in this process: [`LocalStore`], what a worker
//! daemon linked into the server's own process (`kbf-all-in-one`) reads and writes blobs
//! through, with no listener, TLS or protobuf encoding between them.
//!
//! Each call is one the worker listener serves a remote daemon, on the same [`Cache`]:
//! FindMissing ([`LocalStore::find_missing`]), a whole-blob read
//! ([`LocalStore::read_blob`]), a whole-blob write hashed here once, under the same
//! admission as a ByteStream `Write` ([`LocalStore::put`]), and the `Segments` handlers
//! a remote upload runs ([`LocalStore::begin`], [`LocalStore::put_range`],
//! [`LocalStore::commit`]): with a write tier the daemon's digests are trusted at the
//! commit and the flusher hashes every record before its PUT, dropping one that does
//! not match; without one each record is hashed as its ranges arrive. Nothing here
//! skips a check the listener's path makes.
//!
//! **Admission.** Every call is admitted first, by the rule that admits a remote
//! daemon's blob call ([`crate::identity`]): the [`Caller`]'s certificate must name
//! exactly one node, and neither that node nor the certificate (its serial or its public
//! key) may be on the worker listener's deny list, read again at every call. What the
//! listener checks and a call in this process has no use for is the transport: that
//! the call came over mutual TLS with that certificate.

use std::sync::Arc;

use futures::future::BoxFuture;
use kbf_front::{
    Cache, CacheError, Connections, MetaLog, SegmentsConfig, SegmentsService, VerifiedBlob,
};
use kbf_objstore::ObjectStore;
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{
    BeginRequest, BeginResponse, CommitRequest, CommitResponse, PutRangeRequest, PutRangeResponse,
};
use kbf_types::Digest;
use tonic::{Request, Status};

use crate::identity::{PeerCert, Peers};

/// The server's cache, as calls one daemon in the same process makes, each admitted
/// for that daemon.
pub struct LocalStore {
    inner: Arc<dyn Blobs>,
    peers: Peers,
    caller: Caller,
    /// This caller's own `Segments` service: its uploads, no one else's.
    segments: Arc<dyn Segments>,
}

/// Who calls a [`LocalStore`]: the daemon's own client certificate, the one it opens
/// its control stream to the server with.
#[derive(Clone, Debug)]
pub struct Caller {
    cert: PeerCert,
    node: String,
}

impl Caller {
    /// The caller whose certificate is the first one in `pem`.
    ///
    /// # Errors
    /// `pem` holds no certificate, or one that does not name exactly one node.
    pub fn from_pem(pem: &[u8]) -> Result<Self, String> {
        let (_, pem) = x509_parser::pem::parse_x509_pem(pem)
            .map_err(|e| format!("not a PEM certificate: {e}"))?;
        let cert = PeerCert::from_der(&pem.contents)?;
        let node = cert.node().map_err(|e| e.message().to_owned())?.to_owned();
        Ok(Self { cert, node })
    }
}

/// The parts of a [`LocalStore`] the server holds, to make one per caller.
#[derive(Clone)]
pub(crate) struct LocalServer {
    inner: Arc<dyn Blobs>,
    peers: Peers,
}

impl LocalServer {
    /// The calls over `cache`, its `Segments` services keeping `config`, admitted by
    /// `peers` (the worker listener's).
    pub(crate) fn new<M: MetaLog, O: ObjectStore + 'static>(
        cache: Arc<Cache<M, O>>,
        config: SegmentsConfig,
        peers: Peers,
    ) -> Self {
        Self {
            inner: Arc::new(Over { cache, config }),
            peers,
        }
    }

    /// The store as `caller` calls it.
    pub(crate) fn store(&self, caller: Caller) -> LocalStore {
        LocalStore {
            segments: self.inner.segments(),
            inner: Arc::clone(&self.inner),
            peers: self.peers.clone(),
            caller,
        }
    }
}

/// [`LocalStore`]'s calls over a cache of any metadata log and object store.
trait Blobs: Send + Sync {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>>;
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Vec<u8>>;
    fn put(&self, bytes: Vec<u8>) -> Answer<'_, Digest>;
    fn segments(&self) -> Arc<dyn Segments>;
}

type Answer<'a, T> = BoxFuture<'a, Result<T, CacheError>>;

/// A cache and the `--segment-upload-*` limits its `Segments` services keep.
struct Over<M, O> {
    cache: Arc<Cache<M, O>>,
    config: SegmentsConfig,
}

impl<M: MetaLog, O: ObjectStore + 'static> Blobs for Over<M, O> {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(self.cache.find_missing(digests))
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Vec<u8>> {
        Box::pin(async move { self.cache.read_blob(digest).await.map(Vec::from) })
    }

    fn put(&self, bytes: Vec<u8>) -> Answer<'_, Digest> {
        Box::pin(async move {
            let size = bytes.len() as u64;
            let blob = VerifiedBlob::hashed(bytes.into());
            let digest = blob.digest();
            let admitted = self.cache.admit(size).await;
            let stored = self.cache.store_admitted(vec![blob], admitted).await;
            stored.map(|()| digest)
        })
    }

    fn segments(&self) -> Arc<dyn Segments> {
        // Its own uploads and connections: no other caller can name its uploads, and
        // having no connection, an upload it leaves is dropped when idle.
        let connections = Connections::new();
        Arc::new(SegmentsService::new(
            Arc::clone(&self.cache),
            self.config,
            connections,
        ))
    }
}

impl LocalStore {
    /// The calls `caller` makes over `cache`, its `Segments` service keeping `config`,
    /// admitted by `peers`.
    pub fn new<M: MetaLog, O: ObjectStore + 'static>(
        cache: Arc<Cache<M, O>>,
        config: SegmentsConfig,
        peers: Peers,
        caller: Caller,
    ) -> Self {
        LocalServer::new(cache, config, peers).store(caller)
    }

    /// Whether the caller may make a call now (the module docs say by what rule).
    async fn admitted(&self) -> Result<(), Status> {
        let _ = (&self.peers, &self.caller.cert, &self.caller.node);
        Ok(())
    }

    /// The digests among `digests` the cache does not hold, in the order asked, as
    /// FindMissingBlobs answers; the held ones are touched.
    ///
    /// # Errors
    /// The caller is not admitted, or the metadata log is unavailable.
    pub async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, Status> {
        self.admitted().await?;
        Ok(self.inner.find_missing(digests).await?)
    }

    /// The blob's bytes, checked against its digest by the cache.
    ///
    /// # Errors
    /// The caller is not admitted; NOT_FOUND if the blob is not held, or why it cannot
    /// be read.
    pub async fn read_blob(&self, digest: &Digest) -> Result<Vec<u8>, Status> {
        self.admitted().await?;
        Ok(self.inner.read_blob(digest).await?)
    }

    /// Hashes `bytes` once, stores them under that digest within
    /// `--write-bytes-in-flight` (waiting for room, as a ByteStream `Write` does), and
    /// returns the digest once they are durable.
    ///
    /// # Errors
    /// The caller is not admitted, the blob is over the largest the cache accepts, or
    /// the store or the metadata log failed.
    pub async fn put(&self, bytes: Vec<u8>) -> Result<Digest, Status> {
        self.admitted().await?;
        Ok(self.inner.put(bytes).await?)
    }

    /// `Segments.Begin`, on this caller's own service: its uploads are named by no
    /// other caller. An upload is held until it commits, or until it has had no call
    /// for `--segment-upload-idle-secs`.
    ///
    /// # Errors
    /// The caller is not admitted, or what `Begin` answers.
    pub async fn begin(&self, request: BeginRequest) -> Result<BeginResponse, Status> {
        self.admitted().await?;
        let begun = self.segments.begin(Request::new(request)).await?;
        Ok(begun.into_inner())
    }

    /// `Segments.PutRange`, on this caller's own service.
    ///
    /// # Errors
    /// The caller is not admitted, or what `PutRange` answers.
    pub async fn put_range(&self, request: PutRangeRequest) -> Result<PutRangeResponse, Status> {
        self.admitted().await?;
        let put = self.segments.put_range(Request::new(request)).await?;
        Ok(put.into_inner())
    }

    /// `Segments.Commit`, on this caller's own service.
    ///
    /// # Errors
    /// The caller is not admitted, or what `Commit` answers.
    pub async fn commit(&self, request: CommitRequest) -> Result<CommitResponse, Status> {
        self.admitted().await?;
        let committed = self.segments.commit(Request::new(request)).await?;
        Ok(committed.into_inner())
    }
}
