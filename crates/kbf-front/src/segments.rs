//! `kbf.worker.v1.Segments`: a daemon uploads an action's outputs as whole segments
//! with one commit, and reads blobs as ranges of the objects that hold them
//! (`docs/design/write-path.md`, section 4).
//!
//! Served on the worker listener only, behind its admission (kbf-server wraps this
//! service as it wraps the blob services); this type authorizes nothing itself.
//!
//! - **Begin** checks every footer, refuses an upload over
//!   `--segment-upload-max-bytes` or [`MAX_UPLOAD_RECORDS`], and then takes its whole
//!   length from the memory budget, waiting (never refusing) while it is spent. Ranges
//!   are held in memory until the commit: the direct path; the local write tier is a
//!   later stage.
//! - **PutRange** writes a range of one segment's records. Once Commit was called,
//!   every PutRange is refused, so the bytes checked are the bytes stored.
//! - **Commit** runs in a task of its own, detached from the call and the connection,
//!   until it has indexed or failed: every record's SHA-256 is checked, each segment
//!   gets a footer built here (the daemon's CRC-32C values never reach the store), each
//!   is PUT and read back, and one `PutBlobs` commits every record. A second Commit
//!   waits for the first and returns its answer.
//! - An upload belongs to the connection that began it ([`Connections`]). When that
//!   connection closes, or no call names the upload for `--segment-upload-idle-secs`, an
//!   upload still receiving is dropped and its memory returned at once; a committing
//!   one is kept until its commit ends.

mod connection;
mod upload;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kbf_meta::{Epoch, ObjectId};
use kbf_objstore::ObjectStore;
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{
    BeginRequest, BeginResponse, BlobLocation, CommitRequest, CommitResponse, LocateRequest,
    LocateResponse, ObjectRef, PutRangeRequest, PutRangeResponse, ReadRangeRequest,
    ReadRangeResponse,
};
use kbf_types::Digest;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::time::Instant;
use tonic::{Request, Response, Status};

pub use self::connection::{Connection, Connections, Tracked};
use self::upload::{Declared, Put, Receiving, declare};
use crate::cache::Cache;
use crate::meta_log::MetaLog;
use crate::{MAX_BLOB_BYTES, wire};

/// The most records one upload may hold: bounds `Begin`'s message (16,384 entries of
/// 52 bytes, about 850 KiB) and the upload's one `PutBlobs`.
pub const MAX_UPLOAD_RECORDS: usize = 16_384;

/// The most bytes one `PutRange` or `ReadRange` carries.
pub const MAX_RANGE_BYTES: u64 = 4 << 20;

/// The smallest `--segment-upload-max-bytes`: one record of [`MAX_BLOB_BYTES`] and its
/// footer, with room to spare, so any legal blob can be uploaded.
pub const MIN_SEGMENT_UPLOAD_BYTES: u64 = MAX_BLOB_BYTES + (1 << 20);

/// The memory budget counts whole KiB, so a cap of terabytes still fits a permit count.
const BUDGET_UNIT: u64 = 1 << 10;

/// The `--segment-upload-*` flags of the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentsConfig {
    /// The most bytes one upload may declare, and the memory every upload in progress
    /// may hold together (`--segment-upload-max-bytes`).
    pub max_bytes: u64,
    /// How long an upload may go without a call before it is dropped
    /// (`--segment-upload-idle-secs`).
    pub idle: Duration,
}

impl SegmentsConfig {
    /// The defaults: [`MIN_SEGMENT_UPLOAD_BYTES`] and 60 seconds.
    pub const DEFAULT: Self = Self {
        max_bytes: MIN_SEGMENT_UPLOAD_BYTES,
        idle: Duration::from_secs(60),
    };
}

impl Default for SegmentsConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The `Segments` service over one cache.
#[derive(Debug)]
pub struct SegmentsService<M, O> {
    shared: Arc<Shared<M, O>>,
}

impl<M, O> Clone for SegmentsService<M, O> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

#[derive(Debug)]
struct Shared<M, O> {
    cache: Arc<Cache<M, O>>,
    config: SegmentsConfig,
    connections: Connections,
    budget: Arc<Semaphore>,
    next: AtomicU64,
    uploads: Mutex<HashMap<u64, Arc<Upload>>>,
}

#[derive(Debug)]
struct Upload {
    owner: Option<Connection>,
    declared: Vec<Declared>,
    phase: Mutex<Phase>,
    last_call: Mutex<Instant>,
    /// The commit's answer, once there is one.
    done: watch::Sender<Option<Result<Vec<Digest>, Status>>>,
}

#[derive(Debug)]
enum Phase {
    Receiving {
        segments: Vec<Receiving>,
        _budget: OwnedSemaphorePermit,
    },
    Committing,
    /// Committed or failed; the answer is in [`Upload::done`].
    Done,
    Dropped,
}

/// Every lock here guards state that is whole after each statement.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Upload {
    fn touch(&self) {
        *lock(&self.last_call) = Instant::now();
    }

    fn committing(&self) -> bool {
        matches!(*lock(&self.phase), Phase::Committing)
    }

    /// The commit's answer, once it has one.
    async fn answer(&self) -> Result<Vec<Digest>, Status> {
        let mut done = self.done.subscribe();
        let answered = done.wait_for(Option::is_some).await;
        match answered.as_deref() {
            Ok(Some(answer)) => answer.clone(),
            // The sender lives in `self`, so it is never dropped while borrowed.
            _ => Err(Status::internal(
                "an upload's commit ended without an answer",
            )),
        }
    }

    /// Drops the upload, freeing its buffers and budget even if a call still holds it,
    /// unless a commit of it is running: decided under the phase lock, so a Commit
    /// cannot start between the check and the drop.
    fn try_drop(&self) -> bool {
        let mut phase = lock(&self.phase);
        if matches!(*phase, Phase::Committing) {
            return false;
        }
        *phase = Phase::Dropped;
        true
    }

    /// Waits while a commit is running, so it is never dropped.
    async fn settled(&self) {
        if self.committing() {
            let _ = self.answer().await;
        }
    }
}

impl<M: MetaLog, O: ObjectStore + 'static> SegmentsService<M, O> {
    /// The service over `cache`, with uploads bounded as `config` says and owned by the
    /// connections `connections` tracks (the listener's accepted streams must be
    /// wrapped by [`Connections::track`], or uploads are dropped only when idle).
    #[must_use]
    pub fn new(cache: Arc<Cache<M, O>>, config: SegmentsConfig, connections: Connections) -> Self {
        let permits = usize::try_from(config.max_bytes.div_ceil(BUDGET_UNIT))
            .unwrap_or(usize::MAX)
            .min(Semaphore::MAX_PERMITS);
        Self {
            shared: Arc::new(Shared {
                cache,
                config,
                connections,
                budget: Arc::new(Semaphore::new(permits)),
                next: AtomicU64::new(1),
                uploads: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Bytes of the memory budget held by uploads now, in [`BUDGET_UNIT`]s rounded up
    /// per upload: what a test watches to see a dropped upload give its memory back.
    #[must_use]
    pub fn budget_in_use(&self) -> u64 {
        let total = self.shared.config.max_bytes.div_ceil(BUDGET_UNIT);
        (total - self.shared.budget.available_permits() as u64) * BUDGET_UNIT
    }

    /// How many uploads are open (receiving, committing, or holding a commit's answer).
    #[must_use]
    pub fn open_uploads(&self) -> usize {
        lock(&self.shared.uploads).len()
    }

    /// The upload `id`, if the caller's connection owns it.
    fn upload<T>(&self, request: &Request<T>, id: u64) -> Result<Arc<Upload>, Status> {
        let caller = self.shared.connections.of(request.remote_addr());
        let upload = lock(&self.shared.uploads).get(&id).cloned();
        upload
            .filter(|u| u.owner.as_ref().map(Connection::id) == caller.as_ref().map(Connection::id))
            .ok_or_else(|| Status::not_found(format!("no upload {id} on this connection")))
    }

    /// Drops upload `id` once no commit of it is running: called when its connection
    /// closed or it went idle.
    async fn reap(shared: Arc<Shared<M, O>>, id: u64, upload: Arc<Upload>) {
        let idle = shared.config.idle;
        let closed = async {
            match &upload.owner {
                Some(owner) => owner.closed().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(closed);
        let mut closed_seen = false;
        loop {
            if !closed_seen {
                let deadline = *lock(&upload.last_call) + idle;
                tokio::select! {
                    () = &mut closed => closed_seen = true,
                    () = tokio::time::sleep_until(deadline) => {
                        if *lock(&upload.last_call) + idle > Instant::now() {
                            continue;
                        }
                    }
                }
            }
            if upload.try_drop() {
                break;
            }
            // A commit is running: it is never dropped. Once it ends, its answer is
            // kept for another idle period (or dropped at once if the connection is
            // gone).
            upload.settled().await;
        }
        lock(&shared.uploads).remove(&id);
    }

    /// Checks every record, builds and stores the segments, and commits them; then
    /// answers every Commit waiting and returns the memory. Runs detached.
    async fn commit_task(
        cache: Arc<Cache<M, O>>,
        upload: Arc<Upload>,
        segments: Vec<Receiving>,
        budget: OwnedSemaphorePermit,
    ) {
        let checking = Arc::clone(&upload);
        let checked = tokio::task::spawn_blocking(move || {
            segments
                .into_iter()
                .zip(&checking.declared)
                .map(|(received, declared)| received.check(declared))
                .collect::<Result<Vec<_>, Status>>()
        })
        .await
        .unwrap_or_else(|e| Err(Status::internal(format!("checking an upload failed: {e}"))));
        let answer = match checked {
            Ok(built) => cache.store_upload(built).await.map_err(Status::from),
            Err(refused) => Err(refused),
        };
        drop(budget);
        *lock(&upload.phase) = Phase::Done;
        upload.touch();
        upload.done.send_replace(Some(answer));
    }
}

#[tonic::async_trait]
impl<M: MetaLog, O: ObjectStore + 'static> Segments for SegmentsService<M, O> {
    async fn begin(
        &self,
        request: Request<BeginRequest>,
    ) -> Result<Response<BeginResponse>, Status> {
        let owner = self.shared.connections.of(request.remote_addr());
        let segments: Vec<(u64, &[u8])> = request
            .get_ref()
            .segments
            .iter()
            .map(|s| (s.length, s.footer.as_slice()))
            .collect();
        let (declared, total, records) = declare(&segments, MAX_BLOB_BYTES)?;
        let cap = self.shared.config.max_bytes;
        if total > cap {
            return Err(Status::invalid_argument(format!(
                "an upload of {total} bytes is over the {cap}-byte cap (--segment-upload-max-bytes)"
            )));
        }
        if records > MAX_UPLOAD_RECORDS {
            return Err(Status::invalid_argument(format!(
                "an upload of {records} records is over the {MAX_UPLOAD_RECORDS}-record limit"
            )));
        }
        // `total <= cap`, so the permits fit the budget and this always ends.
        let units = u32::try_from(total.div_ceil(BUDGET_UNIT)).unwrap_or(u32::MAX);
        let budget = Arc::clone(&self.shared.budget)
            .acquire_many_owned(units)
            .await
            .map_err(|_| Status::unavailable("the upload budget is closed"))?;
        let segments = declared.iter().map(Receiving::new).collect();
        let (done, _) = watch::channel(None);
        let upload = Arc::new(Upload {
            owner,
            declared,
            phase: Mutex::new(Phase::Receiving {
                segments,
                _budget: budget,
            }),
            last_call: Mutex::new(Instant::now()),
            done,
        });
        let id = self.shared.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.shared.uploads).insert(id, Arc::clone(&upload));
        tokio::spawn(Self::reap(Arc::clone(&self.shared), id, upload));
        Ok(Response::new(BeginResponse { upload: id }))
    }

    async fn put_range(
        &self,
        request: Request<PutRangeRequest>,
    ) -> Result<Response<PutRangeResponse>, Status> {
        let upload = self.upload(&request, request.get_ref().upload)?;
        upload.touch();
        let PutRangeRequest {
            segment,
            offset,
            bytes,
            ..
        } = request.into_inner();
        if bytes.len() as u64 > MAX_RANGE_BYTES {
            return Err(Status::invalid_argument(format!(
                "a range of {} bytes is over the {MAX_RANGE_BYTES}-byte limit",
                bytes.len()
            )));
        }
        let index = segment as usize;
        let declared = upload.declared.get(index).ok_or_else(|| {
            Status::invalid_argument(format!("no segment {segment} in the upload"))
        })?;
        let mut phase = lock(&upload.phase);
        match &mut *phase {
            Phase::Receiving { segments, .. } => {
                let put = segments[index].put(declared.records_len, offset, &bytes)?;
                if put == Put::Resent {
                    tracing::debug!(segment, offset, "resent range acknowledged, not written");
                }
                Ok(Response::new(PutRangeResponse {}))
            }
            Phase::Committing | Phase::Done => Err(Status::failed_precondition(
                "the upload is committing; ranges are no longer accepted",
            )),
            Phase::Dropped => Err(Status::not_found("the upload was dropped")),
        }
    }

    async fn commit(
        &self,
        request: Request<CommitRequest>,
    ) -> Result<Response<CommitResponse>, Status> {
        let upload = self.upload(&request, request.get_ref().upload)?;
        upload.touch();
        {
            let mut phase = lock(&upload.phase);
            if let Phase::Receiving { segments, .. } = &*phase {
                let missing = segments
                    .iter()
                    .zip(&upload.declared)
                    .find_map(|(r, d)| r.first_missing(d));
                if let Some(digest) = missing {
                    // The upload stays open: the daemon may send what is missing.
                    return Err(Status::failed_precondition(format!(
                        "record {digest} was not wholly received"
                    )));
                }
            }
            match std::mem::replace(&mut *phase, Phase::Committing) {
                Phase::Receiving {
                    segments,
                    _budget: budget,
                } => {
                    tokio::spawn(Self::commit_task(
                        Arc::clone(&self.shared.cache),
                        Arc::clone(&upload),
                        segments,
                        budget,
                    ));
                }
                Phase::Dropped => {
                    *phase = Phase::Dropped;
                    return Err(Status::not_found("the upload was dropped"));
                }
                // Committing, or answered: wait for, or return, that answer.
                other => *phase = other,
            }
        }
        let committed = upload.answer().await?;
        Ok(Response::new(CommitResponse {
            committed: committed.iter().map(wire::digest_to_proto).collect(),
        }))
    }

    async fn locate(
        &self,
        request: Request<LocateRequest>,
    ) -> Result<Response<LocateResponse>, Status> {
        let digests = request
            .get_ref()
            .digests
            .iter()
            .map(|d| wire::digest(Some(d)))
            .collect::<Result<Vec<Digest>, Status>>()?;
        let found = self.shared.cache.locate(&digests).await?;
        let locations = digests
            .iter()
            .zip(found)
            .map(|(digest, location)| BlobLocation {
                digest: Some(wire::digest_to_proto(digest)),
                object: location.map(|l| ObjectRef {
                    epoch: l.object.epoch().get(),
                    seq: l.object.seq(),
                }),
                offset: location.map_or(0, |l| l.offset),
            })
            .collect();
        Ok(Response::new(LocateResponse { locations }))
    }

    async fn read_range(
        &self,
        request: Request<ReadRangeRequest>,
    ) -> Result<Response<ReadRangeResponse>, Status> {
        let ReadRangeRequest {
            object,
            offset,
            length,
        } = request.into_inner();
        if length == 0 || length > MAX_RANGE_BYTES {
            return Err(Status::invalid_argument(format!(
                "a read of {length} bytes is outside 1..={MAX_RANGE_BYTES}"
            )));
        }
        let object = object.ok_or_else(|| Status::invalid_argument("no object named"))?;
        let id = ObjectId::new(Epoch::new(object.epoch), object.seq);
        match self
            .shared
            .cache
            .read_object_range(id, offset, length)
            .await?
        {
            Some(bytes) => Ok(Response::new(ReadRangeResponse {
                bytes: bytes.to_vec(),
            })),
            None => Err(Status::not_found(format!(
                "object {}/{} holds no {length} bytes at {offset}",
                object.epoch, object.seq
            ))),
        }
    }
}
