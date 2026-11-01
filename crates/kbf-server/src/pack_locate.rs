//! `kbf.worker.v1.PackIndex` on the worker listener: where a daemon finds a blob inside a pack,
//! and where a daemon that writes packs has them recorded.
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
//!
//! **CommitPack.** A daemon that stores its outputs in packs (the object store, create-only)
//! asks the frontend to record each pack in the index. The method is served only by a frontend
//! that has the pack index **and** the pack store ([`PackCommits`]: the frontend must read the
//! pack to check it); a frontend with the index alone answers it UNIMPLEMENTED, as does one
//! without the index. The checks are those of [`kbf_front::Packs::commit_pack`]: the writer must
//! be the node the caller's certificate names (PERMISSION_DENIED otherwise: a daemon commits
//! only its own packs), the pack must be named `cas/d/<writer>/<boot>/<sequence>`, and a sample
//! of its blobs (`--pack-commit-sample`) must be the bytes of their digests. A refusal records
//! nothing and is counted (`kbf_pack_commit_rejects_total`) and logged. A frontend tells its
//! daemons it takes the call in `Welcome.pack_commit` ([`commit_served`]).

use std::sync::Arc;

use kbf_front::{CommitError, Packs};
use kbf_pack_index::{
    BlobDigest, Location, MAX_BATCH, NewPack, PackBlob, PackIndex, PackIndexError,
};
use kbf_proto::reapi::Digest;
use kbf_proto::worker::pack_index_server::{PackIndex as PackIndexRpc, PackIndexServer};
use kbf_proto::worker::{
    CommitPackRequest, CommitPackResponse, PackLocateRequest, PackLocateResponse, PackLocation,
    PackedBlob,
};
use tonic::{Request, Response, Status};

use crate::blobs::admit;
use crate::identity::Peers;

/// What a frontend that has the pack store commits packs with: its packs, and how many blobs
/// of a pack it checks against their digests (`--pack-commit-sample`).
#[derive(Clone, Debug)]
pub struct PackCommits {
    packs: Packs,
    sample: usize,
}

impl PackCommits {
    /// Commits through `packs`, checking `sample` blobs of each pack (all of them if it has
    /// fewer; none if 0).
    #[must_use]
    pub const fn new(packs: Packs, sample: usize) -> Self {
        Self { packs, sample }
    }

    /// How many blobs of a pack are checked against their digests.
    #[must_use]
    pub const fn sample(&self) -> usize {
        self.sample
    }
}

/// The pack index, answering the daemons' `Locate` calls and, with the pack store, their
/// `CommitPack` calls.
pub struct PackLocateService {
    index: Arc<dyn PackIndex>,
    peers: Peers,
    commits: Option<PackCommits>,
}

impl PackLocateService {
    /// The service over `index`, admitting calls as `peers` (the worker listener's). It answers
    /// `CommitPack` UNIMPLEMENTED until [`PackLocateService::with_commits`] gives it packs.
    #[must_use]
    pub fn new(index: Arc<dyn PackIndex>, peers: Peers) -> Self {
        Self {
            index,
            peers,
            commits: None,
        }
    }

    /// The service committing packs through `commits`, when the frontend has the pack store;
    /// with `None`, as [`PackLocateService::new`] makes it.
    #[must_use]
    pub fn with_commits(mut self, commits: Option<PackCommits>) -> Self {
        self.commits = commits;
        self
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

    /// Records the pack `request` names, as the node `caller` (whom the call was admitted as)
    /// commits it.
    ///
    /// # Errors
    /// UNIMPLEMENTED: the frontend has no pack store. INVALID_ARGUMENT: a blob has no digest or
    /// one that is not a SHA-256 digest, or the pack is not one that can be recorded.
    /// PERMISSION_DENIED: the writer is not `caller`. FAILED_PRECONDITION: the pack is not in the
    /// object store. ALREADY_EXISTS: the pack was recorded with other contents. UNAVAILABLE: the
    /// object store or the index could not answer.
    pub async fn commit(
        &self,
        caller: &str,
        request: CommitPackRequest,
    ) -> Result<CommitPackResponse, Status> {
        let Some(commits) = &self.commits else {
            return Err(Status::unimplemented(
                "this frontend has no pack store to check a pack against",
            ));
        };
        let packs = &commits.packs;
        let pack = match new_pack(request) {
            Ok(pack) => pack,
            Err(refused) => {
                packs.metrics().commit_rejects.add(1);
                let reason = refused.message();
                tracing::warn!(
                    reason,
                    "a pack commit was refused: a blob has no usable digest"
                );
                return Err(refused);
            }
        };
        let done = packs.commit_pack(caller, pack, commits.sample).await;
        let recorded = done.map_err(commit_refused)?;
        Ok(CommitPackResponse {
            created: recorded.created,
            indexed: recorded.indexed,
        })
    }
}

/// The service the worker listener registers: one over `index` when `--pack-index` opened an
/// index, none otherwise; it commits packs through `commits` when the frontend has the pack
/// store. Not generic, so the frontend's start counts toward one copy of it.
#[must_use]
pub fn service(
    index: Option<&Arc<dyn PackIndex>>,
    peers: &Peers,
    commits: Option<PackCommits>,
) -> Option<PackIndexServer<PackLocateService>> {
    let index = Arc::clone(index?);
    let service = PackLocateService::new(index, peers.clone());
    Some(service.with_commits(commits).server())
}

/// Whether the service [`service`] builds from these takes `CommitPack`: it has the index and
/// the pack store both. What a frontend's `Welcome` says to its daemons.
#[must_use]
pub fn commit_served(index: Option<&Arc<dyn PackIndex>>, commits: Option<&PackCommits>) -> bool {
    index.zip(commits).is_some()
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

    async fn commit_pack(
        &self,
        request: Request<CommitPackRequest>,
    ) -> Result<Response<CommitPackResponse>, Status> {
        let caller = admit(&self.peers, &request, "PackIndex.CommitPack").await?;
        let committed = self.commit(&caller, request.into_inner()).await?;
        Ok(Response::new(committed))
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

/// The pack a commit names, as the index takes it.
///
/// # Errors
/// INVALID_ARGUMENT: a blob has no digest, or one that is not a SHA-256 digest.
fn new_pack(request: CommitPackRequest) -> Result<NewPack, Status> {
    let blobs = request.blobs.iter().map(packed).collect::<Result<_, _>>()?;
    Ok(NewPack {
        key: request.pack,
        writer: request.writer,
        size: request.size,
        blobs,
    })
}

/// A blob of a commit as the index takes it.
fn packed(blob: &PackedBlob) -> Result<PackBlob, Status> {
    let Some(digest) = &blob.digest else {
        let message = "a blob of the pack has no digest";
        return Err(Status::invalid_argument(message));
    };
    Ok(PackBlob {
        digest: parse(digest)?,
        offset: blob.offset,
    })
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

/// The status of a refused commit. What the daemon is told is about its own request, but for a
/// store or an index that failed: that is UNAVAILABLE with a fixed text, as `Locate`'s is.
fn commit_refused(error: CommitError) -> Status {
    let message = error.message().to_owned();
    match error {
        CommitError::Forbidden(_) => Status::permission_denied(message),
        CommitError::Invalid(_) | CommitError::Mismatch(_) => Status::invalid_argument(message),
        CommitError::Missing(_) => Status::failed_precondition(message),
        CommitError::Conflict(_) => Status::already_exists(message),
        CommitError::Unavailable(_) => {
            Status::unavailable("the pack store or the pack index could not answer")
        }
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
