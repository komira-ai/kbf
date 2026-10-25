//! `kbf.worker.v1.PackIndex` on the worker listener: where a daemon finds a blob inside a pack.
//!
//! A frontend started with `--pack-index` keeps the pack index, which says for each blob the
//! pack that holds it and where. A daemon started with `--pack-read` asks this service where
//! its blobs are, then reads those ranges of those packs from the object store itself and
//! hashes them; it never opens the index's database. The answer for a blob the index has no
//! row for is "not located", which is no error: the daemon reads that blob where it did before.
//!
//! The service is **off unless `--pack-index` is given**: without the flag it is not
//! registered (`service` answers `None`), the router answers a call to it UNIMPLEMENTED, and
//! the daemon reads as it did before. The method is in the listener's role table
//! ([`crate::worker_roles`]) for the daemon role alone, so a storage or frontend certificate
//! is refused PERMISSION_DENIED before the call is routed. Past the role, each call is
//! admitted as the listener's blob calls are ([`crate::blobs`]): mutual TLS, a certificate
//! that names exactly one node, and neither on the deny list.
//!
//! A call names at most [`MAX_BATCH`] digests, the index's own bound; more is
//! INVALID_ARGUMENT, as is a digest that is not a SHA-256 one. The index is asked on the
//! blocking pool, as its calls block.

use std::sync::Arc;

use kbf_proto::reapi::Digest;
use kbf_proto::worker::pack_index_server::{PackIndex as PackIndexRpc, PackIndexServer};
use kbf_proto::worker::{PackLocateRequest, PackLocateResponse, PackLocation};
use kbf_state::{BlobDigest, Location, MAX_BATCH, PackIndex, PackIndexError};
use tonic::{Request, Response, Status};

use crate::blobs::admit;
use crate::identity::Peers;

/// The pack index, answering the daemons' `Locate` calls.
pub struct PackLocateService {
    index: Arc<dyn PackIndex>,
    peers: Peers,
}

impl PackLocateService {
    /// The service over `index`, admitting calls as `peers` (the worker listener's).
    #[must_use]
    pub fn new(index: Arc<dyn PackIndex>, peers: Peers) -> Self {
        Self { index, peers }
    }

    /// The tonic server of the service.
    #[must_use]
    pub fn server(self) -> PackIndexServer<Self> {
        PackIndexServer::new(self)
    }

    /// Where the blobs `digests` are, in the order asked, as the index says now.
    ///
    /// # Errors
    /// INVALID_ARGUMENT: more than [`MAX_BATCH`] digests, or one that is not a SHA-256 digest.
    /// UNAVAILABLE: the index cannot be read. INTERNAL: the call to the index panicked.
    pub async fn answer(&self, digests: &[Digest]) -> Result<PackLocateResponse, Status> {
        let digests = parse_all(digests)?;
        let index = Arc::clone(&self.index);
        let work = move || index.locate(&digests);
        let joined = tokio::task::spawn_blocking(work).await;
        let located = joined.map_err(unfinished)?;
        let located = located.map_err(unavailable)?;
        let locations = located.into_iter().map(location).collect();
        Ok(PackLocateResponse { locations })
    }
}

/// The service the worker listener registers: one over `index` when `--pack-index` opened an
/// index, none otherwise. Not generic, so the frontend's start counts toward one copy of it.
#[must_use]
pub fn service(
    index: Option<&Arc<dyn PackIndex>>,
    peers: &Peers,
) -> Option<PackIndexServer<PackLocateService>> {
    let index = Arc::clone(index?);
    Some(PackLocateService::new(index, peers.clone()).server())
}

#[tonic::async_trait]
impl PackIndexRpc for PackLocateService {
    async fn locate(
        &self,
        request: Request<PackLocateRequest>,
    ) -> Result<Response<PackLocateResponse>, Status> {
        admit(&self.peers, &request, "PackIndex.Locate").await?;
        let located = self.answer(&request.get_ref().digests).await?;
        Ok(Response::new(located))
    }
}

/// The digests of a request as the index takes them.
///
/// # Errors
/// INVALID_ARGUMENT: more than [`MAX_BATCH`] digests, or one that is not a SHA-256 digest.
fn parse_all(digests: &[Digest]) -> Result<Vec<BlobDigest>, Status> {
    let given = digests.len();
    if given > MAX_BATCH {
        let message = format!("{given} digests in one call, at most {MAX_BATCH}");
        return Err(Status::invalid_argument(message));
    }
    digests.iter().map(parse).collect()
}

/// A digest as the index names a blob: the SHA-256 in bytes, and the length.
fn parse(digest: &Digest) -> Result<BlobDigest, Status> {
    let (hash, size) = (&digest.hash, digest.size_bytes);
    let invalid = || Status::invalid_argument(format!("{hash}/{size} is not a SHA-256 digest"));
    let bytes = hex::decode(hash).map_err(|_| invalid())?;
    let hash = <[u8; 32]>::try_from(bytes).map_err(|_| invalid())?;
    let size = u64::try_from(size).map_err(|_| invalid())?;
    Ok(BlobDigest { hash, size })
}

/// The answer for one blob: where it is, or that the index has no row for it.
fn location(place: Option<Location>) -> PackLocation {
    let Some(place) = place else {
        return PackLocation::default();
    };
    PackLocation {
        located: true,
        pack: place.pack_key,
        offset: place.offset,
        length: place.length,
    }
}

/// INTERNAL, with a fixed text: the call to the index panicked or was cancelled, which is for the
/// log, not for a daemon.
fn unfinished(error: tokio::task::JoinError) -> Status {
    tracing::error!(%error, "a pack index call did not finish");
    Status::internal("the pack index could not be read")
}

/// UNAVAILABLE, with a fixed text: the index's own error is for the log, not for a daemon.
fn unavailable(error: PackIndexError) -> Status {
    tracing::error!(%error, "the pack index could not answer a locate call");
    Status::unavailable("the pack index could not be read")
}
