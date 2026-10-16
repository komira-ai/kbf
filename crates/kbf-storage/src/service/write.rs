//! Taking a `Write` into the store: streamed where the store can, forwarded to the
//! replica by the primary of a chain, whole in memory only where nothing else is
//! possible.

use bytes::Bytes;
use kbf_map::MemberId;
use kbf_proto::google::bytestream::WriteRequest;
use kbf_types::Digest;
use tonic::{Status, Streaming};

use super::StorageService;
use crate::replicate::{Leg, Plan, Staging, View};
use crate::{PutStream, StorageError, StreamedPut};

/// What a streamed write takes from [`super::Config::write_bytes_in_flight`]: room for the
/// few chunks it holds (the writer's queue, the replica's, and the one in hand), not for
/// the blob.
const STREAM_WINDOW: u64 = 4 << 20;

impl StorageService {
    /// The map a write routed by generation `requested` is judged by: `None` for a node
    /// outside a chain, or with no map yet.
    ///
    /// # Errors
    /// STALE_MAP, or UNAVAILABLE for a caller ahead of the node ([`crate::replicate`]).
    pub(super) fn view(&self, requested: u64) -> Result<Option<View>, StorageError> {
        match &self.chain {
            Some(chain) => chain.view(requested),
            None => Ok(None),
        }
    }

    /// What this node does with a write of `digest` under `view`.
    pub(super) fn plan(
        &self,
        view: Option<&View>,
        digest: &Digest,
        leg_from: Option<MemberId>,
    ) -> Result<Plan, StorageError> {
        match (&self.chain, view) {
            (Some(chain), Some(view)) => chain.plan(view, digest, leg_from),
            _ if leg_from.is_some() => Err(StorageError::Invalid(
                "a copy was sent to a node that is in no chain".into(),
            )),
            _ => Ok(Plan::Local),
        }
    }

    /// Stores the blob `digest`, of which `first` is the first message and `rest` the
    /// others, as `plan` says.
    pub(super) async fn write_blob(
        &self,
        digest: Digest,
        first: WriteRequest,
        mut rest: Streaming<WriteRequest>,
        view: Option<&View>,
        leg_from: Option<MemberId>,
        plan: Plan,
    ) -> Result<(), Status> {
        let size = digest.size_bytes;
        let put = self.open_stream(size).await?;
        let (put, leg) = match (&plan, put, &self.chain, view) {
            (Plan::Primary { replica }, Some(put), Some(chain), Some(view)) => {
                (Some(put), Some(chain.open_leg(view, replica)?))
            }
            (Plan::Primary { .. }, None, _, _) => {
                return Err(Status::failed_precondition(
                    "this node's store cannot write a blob as it arrives, and a chain's primary must \
                     pass the blob on without holding it whole",
                ));
            }
            (_, put, _, _) => (put, None),
        };
        let Some(put) = put else {
            return self.write_buffered(digest, first, rest).await;
        };
        let room = size.min(STREAM_WINDOW);
        let room =
            u32::try_from(room).map_or(self.write_room_bytes, |r| r.min(self.write_room_bytes));
        // The semaphore is never closed.
        let _room = self.write_room.acquire_many(room).await.ok();
        let staged = receive(digest, put, first, &mut rest, leg).await?;
        if let (Some(chain), Some(view)) = (&self.chain, view) {
            chain.confirm(view, &digest, leg_from, &plan)?;
        }
        staged.commit().await?;
        Ok(())
    }

    /// The store's way to write a blob of `size` bytes as it arrives, if it has one.
    async fn open_stream(&self, size: u64) -> Result<Option<Box<dyn PutStream>>, StorageError> {
        let storage = std::sync::Arc::clone(&self.storage);
        tokio::task::spawn_blocking(move || storage.stream(size))
            .await
            .map_err(|e| StorageError::Internal(format!("opening a blob stream: {e}")))
    }

    /// Holds the blob whole until it is complete, then stores it with
    /// [`crate::Storage::put`]: for a store that cannot write as bytes arrive, outside
    /// a chain.
    async fn write_buffered(
        &self,
        digest: Digest,
        first: WriteRequest,
        mut rest: Streaming<WriteRequest>,
    ) -> Result<(), Status> {
        let size = digest.size_bytes;
        let room =
            u32::try_from(size).map_or(self.write_room_bytes, |s| s.min(self.write_room_bytes));
        // The semaphore is never closed.
        let _room = self.write_room.acquire_many(room).await.ok();
        let mut received = Vec::with_capacity(usize::try_from(size).unwrap_or_default());
        let mut message = first;
        loop {
            if (received.len() + message.data.len()) as u64 > size {
                return Err(more_than(&digest));
            }
            received.extend_from_slice(&message.data);
            if message.finish_write {
                break;
            }
            message = next(&mut rest).await?;
        }
        let stored = self.storage.put(received.into()).await?;
        if stored != digest {
            return Err(StorageError::DigestMismatch {
                claimed: digest,
                actual: stored,
            }
            .into());
        }
        Ok(())
    }
}

fn more_than(digest: &Digest) -> Status {
    Status::invalid_argument(format!(
        "more than the {} bytes {digest} names",
        digest.size_bytes
    ))
}

/// The next message of a write that has not finished.
async fn next(stream: &mut Streaming<WriteRequest>) -> Result<WriteRequest, Status> {
    stream
        .message()
        .await?
        .ok_or_else(|| Status::invalid_argument("write stream ended without finish_write"))
}

/// Streams the blob `digest` into `put`, and on to `leg` if there is one, a message at
/// a time. The blob is checked against its digest as it passes, and the replica's
/// answer awaited: the result is a blob written and verified here and held by the
/// replica, not yet present on this node ([`StreamedPut::commit`]).
async fn receive(
    digest: Digest,
    put: Box<dyn PutStream>,
    first: WriteRequest,
    rest: &mut Streaming<WriteRequest>,
    mut leg: Option<Leg>,
) -> Result<Box<dyn StreamedPut>, Status> {
    let mut staging = Staging::start(put, digest);
    let mut received = 0;
    let mut message = first;
    loop {
        received += message.data.len() as u64;
        if received > digest.size_bytes {
            return Err(more_than(&digest));
        }
        let finish = message.finish_write;
        let data = Bytes::from(std::mem::take(&mut message.data));
        staging.push(data.clone()).await?;
        if let Some(leg) = &mut leg {
            leg.send(WriteRequest {
                data: data.to_vec(),
                ..message
            })
            .await?;
        }
        if finish {
            break;
        }
        message = next(rest).await?;
    }
    let (staged, replica) = tokio::join!(staging.finish(), async {
        match leg {
            Some(leg) => leg.finish(digest.size_bytes).await,
            None => Ok(()),
        }
    });
    let staged = staged?;
    replica?;
    Ok(staged)
}
