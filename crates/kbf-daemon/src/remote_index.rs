//! [`RemoteIndex`]: the frontend's pack index, as a daemon reaches it.
//!
//! The pack index lives in the frontends' shared state store, and a daemon opens no connection
//! to it. [`RemoteIndex`] is `kbf_pack_index::PackIndex` over the frontend's worker listener
//! (`kbf.worker.v1.PackIndex/Locate`), for `kbf_pack::Reader`, which asks the index where a blob
//! lies and then reads that range of that pack from the object store, and for `kbf_pack::Packer`,
//! which records the packs a daemon stored. It **locates and commits, and does nothing else**:
//!
//! - [`PackIndex::locate`] asks the frontend. A frontend that serves nothing (it was started
//!   without `--pack-index`: UNIMPLEMENTED) or is not there (UNAVAILABLE) has no location to
//!   give, so every digest is answered as not located, and the caller reads the blobs where it
//!   did before. The first such answer is logged, and the next after a call that was served.
//!   Any other failure is the call's error.
//! - [`PackIndex::find_missing`] is the digests `locate` has no place for.
//! - [`PackIndex::touch`] touches nothing and says so: a daemon does not record the use of a
//!   blob (retention sees the frontends' reads only).
//! - [`PackIndex::record_pack`] commits a pack the daemon stored in the object store, through
//!   the frontend (`kbf.worker.v1.PackIndex/CommitPack`; the writer is the node id, which is the
//!   `d/<node>` of the pack's writer name without its kind). The frontend checks a sample of the
//!   pack's blobs and records the pack, or refuses. A frontend that has no pack store answers
//!   UNIMPLEMENTED, which is [`PackIndexError::Unsupported`]; a key that is recorded with other
//!   contents is [`PackIndexError::Conflict`]; a status that a repeat does not change
//!   (PERMISSION_DENIED, INVALID_ARGUMENT) is [`PackIndexError::Refused`]; every other failure
//!   is the call's store error, with the status in its text. `kbf_pack::Packer` tries a record
//!   again after the last kind only. Only a daemon told by the frontend's `Welcome` that it
//!   takes the call should make it.
//! - The garbage-collection queries and [`PackIndex::pack`] are refused: a daemon is not the
//!   index's keeper.
//!
//! The trait's calls block, and `kbf_pack::Reader` makes them on the runtime's blocking pool, so
//! a call waits for the frontend's answer on a [`Handle`] of that runtime. It must not be made
//! on a thread that is running async tasks (`Handle::block_on` panics there).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kbf_pack_index::{
    BlobDigest, Location, MAX_BATCH, NewPack, PackIndex, PackIndexError, PackInfo, Recorded,
};
use kbf_proto::reapi::Digest;
use kbf_proto::worker::pack_index_client::PackIndexClient;
use kbf_proto::worker::{
    CommitPackRequest, PackLocateRequest, PackLocateResponse, PackLocation, PackedBlob,
};
use kbf_state::StoreError;
use tokio::runtime::Handle;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};

/// How long one `Locate` call may take before it fails: the frontend answers from its index
/// in a round trip, so a call this long has found it stuck. Each call holds a thread of the
/// runtime's blocking pool while it waits, so the bound is short; a read that times out is read
/// where it was before.
pub const LOCATE_WITHIN: Duration = Duration::from_secs(5);

/// How long one `CommitPack` call may take before it fails: the frontend checks a sample of the
/// pack's blobs against the object store before it records the pack, so a call takes longer than
/// a `Locate`. A commit that times out is sent again by the packer.
pub const COMMIT_WITHIN: Duration = Duration::from_secs(30);

/// What the methods of the index that a daemon does not have are refused with.
const NOT_KEPT: &str = "a daemon's pack index only locates blobs and commits packs";

/// The frontend's pack index, reached over the worker listener (the module docs).
pub struct RemoteIndex {
    client: PackIndexClient<Channel>,
    runtime: Handle,
    /// Whether the frontend's last answer was one that served nothing (and it was logged).
    unserved: AtomicBool,
}

impl RemoteIndex {
    /// The index over `channel`, the connection to the frontend's worker listener (mutual TLS
    /// with the daemon's certificate), whose calls wait on `runtime`.
    #[must_use]
    pub fn new(channel: Channel, runtime: Handle) -> Self {
        Self {
            client: PackIndexClient::new(channel),
            runtime,
            unserved: AtomicBool::new(false),
        }
    }

    /// One `Locate` call for `digests`.
    async fn ask(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        let asked = digests.len();
        let digests = digests.iter().map(wire).collect();
        let mut request = Request::new(PackLocateRequest { digests });
        request.set_timeout(LOCATE_WITHIN);
        let mut client = self.client.clone();
        match client.locate(request).await {
            Ok(answer) => {
                self.unserved.store(false, Ordering::Relaxed);
                places(answer.into_inner(), asked)
            }
            Err(status) if serves_nothing(&status) => {
                self.say_unserved(&status);
                Ok(vec![None; asked])
            }
            Err(status) => Err(failed(format!("locate: {status}"))),
        }
    }

    /// One `CommitPack` call for `pack`, written by the node `node`.
    async fn commit(&self, node: &str, pack: &NewPack) -> Result<Recorded, PackIndexError> {
        let blobs = pack
            .blobs
            .iter()
            .map(|blob| PackedBlob {
                digest: Some(wire(&blob.digest)),
                offset: blob.offset,
            })
            .collect();
        let mut request = Request::new(CommitPackRequest {
            pack: pack.key.clone(),
            size: pack.size,
            blobs,
            writer: node.to_owned(),
        });
        request.set_timeout(COMMIT_WITHIN);
        let mut client = self.client.clone();
        match client.commit_pack(request).await {
            Ok(answer) => {
                let answer = answer.into_inner();
                Ok(Recorded {
                    created: answer.created,
                    indexed: answer.indexed,
                })
            }
            Err(status) => Err(commit_failed(&status, &pack.key)),
        }
    }

    /// Logs that the frontend serves no locations, the first time after it did.
    fn say_unserved(&self, status: &Status) {
        if !self.unserved.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                code = ?status.code(),
                "pack reads: the frontend serves no blob locations (it needs --pack-index); \
                 blobs are read where they were before"
            );
        }
    }
}

impl PackIndex for RemoteIndex {
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError> {
        let node = pack
            .writer
            .strip_prefix(DAEMON_WRITER)
            .ok_or_else(|| failed(format!("{:?} is not a daemon's writer name", pack.writer)))?;
        self.runtime.block_on(self.commit(node, pack))
    }

    fn pack(&self, _: &str) -> Result<Option<PackInfo>, PackIndexError> {
        Err(failed(NOT_KEPT.to_owned()))
    }

    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        let given = digests.len();
        if given > MAX_BATCH {
            return Err(PackIndexError::TooMany {
                given,
                max: MAX_BATCH,
            });
        }
        self.runtime.block_on(self.ask(digests))
    }

    fn find_missing(&self, digests: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        let places = self.locate(digests)?;
        let unplaced = digests
            .iter()
            .zip(places)
            .filter(|(_, place)| place.is_none());
        Ok(unplaced.map(|(digest, _)| *digest).collect())
    }

    fn touch(&self, _: &[BlobDigest]) -> Result<usize, PackIndexError> {
        Ok(0)
    }

    fn dead_packs(&self, _: Duration, _: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        Err(failed(NOT_KEPT.to_owned()))
    }

    fn unreferenced_keys(&self, _: &[String]) -> Result<Vec<String>, PackIndexError> {
        Err(failed(NOT_KEPT.to_owned()))
    }
}

/// Whether a failed call means the frontend has no locations to give, rather than that
/// something broke: it does not serve the call, or it is away.
fn serves_nothing(status: &Status) -> bool {
    matches!(status.code(), Code::Unimplemented | Code::Unavailable)
}

/// The index's error for `why`.
fn failed(why: String) -> PackIndexError {
    PackIndexError::Store(StoreError::Failed(why))
}

/// The writer name a daemon's packs carry in the index (`kbf_pack::PackKeys::writer`): the kind
/// `d`, a slash, and the node id.
const DAEMON_WRITER: &str = "d/";

/// The index's error for a failed `CommitPack` call of the pack `key`.
fn commit_failed(status: &Status, key: &str) -> PackIndexError {
    match status.code() {
        Code::Unimplemented => PackIndexError::Unsupported,
        Code::AlreadyExists => PackIndexError::Conflict {
            key: key.to_owned(),
        },
        Code::PermissionDenied | Code::InvalidArgument => {
            PackIndexError::Refused(format!("commit: {status}"))
        }
        _ => failed(format!("commit: {status}")),
    }
}

/// A digest as the wire spells it.
pub(crate) fn wire(digest: &BlobDigest) -> Digest {
    Digest {
        hash: hex::encode(digest.hash),
        // A blob's length is at most `i64::MAX`: it came from a REAPI digest.
        size_bytes: i64::try_from(digest.size).unwrap_or(i64::MAX),
    }
}

/// The locations of an answer to a call that named `asked` digests.
fn places(
    answer: PackLocateResponse,
    asked: usize,
) -> Result<Vec<Option<Location>>, PackIndexError> {
    let answered = answer.locations.len();
    if answered != asked {
        return Err(failed(format!("{answered} locations for {asked} digests")));
    }
    Ok(answer.locations.into_iter().map(place).collect())
}

/// One answer: where the blob is, or `None` when the frontend has no row for it.
fn place(location: PackLocation) -> Option<Location> {
    location.located.then_some(Location {
        pack_key: location.pack,
        offset: location.offset,
        length: location.length,
        // The frontend does not say when a blob was last touched, and the reader does not ask.
        last_touch_us: 0,
    })
}
