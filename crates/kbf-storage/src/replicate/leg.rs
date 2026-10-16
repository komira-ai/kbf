//! The call from a primary to its replica: the same ByteStream `Write` a client makes,
//! fed message by message as the primary receives them, so the blob passes through and
//! is never held whole.

use std::sync::Arc;
use std::time::Duration;

use kbf_map::MemberId;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{WriteRequest, WriteResponse};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Request, Response, Status};

use crate::StorageError;
use crate::service::{MAP_GENERATION_HEADER, MAX_MESSAGE_BYTES, lift};

/// The metadata entry that marks a `Write` as a copy another storage node sends: its
/// value is the sender's member id. The receiver stores the blob and forwards nothing.
pub const LEG_HEADER: &str = "kbf-chain-leg";

/// Messages the call holds ahead of the replica: with the one the sender is putting
/// down, the most a leg holds of a blob, whatever its size.
const QUEUE: usize = 2;

/// A replica that did not take a message, or did not commit, in time.
#[derive(Debug, thiserror::Error)]
#[error("the replica did not {what} within {after:?}")]
struct Stalled {
    what: &'static str,
    after: Duration,
}

/// The call to the replica, ended if the leg is dropped.
struct Call(JoinHandle<Result<Response<WriteResponse>, Status>>);

impl Drop for Call {
    /// A write abandoned (the client left, or another part of it failed) ends the call to
    /// the replica, whose half-received blob is dropped uncommitted.
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One blob on its way to the replica.
pub(crate) struct Leg {
    tx: mpsc::Sender<WriteRequest>,
    call: Call,
    stall: Duration,
    commit: Duration,
}

impl Leg {
    /// Starts the `Write` to `channel`, tagged with the map generation the sender
    /// routed by and its own member id.
    pub(crate) fn open(
        channel: Channel,
        generation: u64,
        sender: MemberId,
        stall: Duration,
        commit: Duration,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel(QUEUE);
        let call = tokio::spawn(async move {
            let mut request = Request::new(futures::stream::poll_fn(move |cx| rx.poll_recv(cx)));
            let metadata = request.metadata_mut();
            metadata.insert(MAP_GENERATION_HEADER, MetadataValue::from(generation));
            metadata.insert(LEG_HEADER, MetadataValue::from(sender.0));
            ByteStreamClient::new(channel)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES)
                .write(request)
                .await
        });
        Self {
            tx,
            call: Call(call),
            stall,
            commit,
        }
    }

    /// Hands the next message to the call, waiting while the replica is behind.
    ///
    /// # Errors
    /// Why the call ended, if it did (the replica refused or could not be reached);
    /// [`StorageError::Unavailable`] if it took no message for the stall bound.
    pub(crate) async fn send(&mut self, message: WriteRequest) -> Result<(), StorageError> {
        match tokio::time::timeout(self.stall, self.tx.send(message)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(self.ended().await),
            Err(_) => Err(StorageError::Unavailable(Arc::new(Stalled {
                what: "take a message",
                after: self.stall,
            }))),
        }
    }

    /// Ends the request stream and waits for the replica's answer: it has verified the
    /// blob against its digest and made it durable, or this says why not.
    ///
    /// # Errors
    /// Why the call failed; [`StorageError::Unavailable`] if the replica did not commit
    /// for the commit bound; [`StorageError::Internal`] if it acknowledged other than
    /// `size` bytes.
    pub(crate) async fn finish(self, size: u64) -> Result<(), StorageError> {
        let Self {
            tx,
            mut call,
            commit,
            ..
        } = self;
        drop(tx);
        let answer = tokio::time::timeout(commit, &mut call.0)
            .await
            .map_err(|_| {
                StorageError::Unavailable(Arc::new(Stalled {
                    what: "commit",
                    after: commit,
                }))
            })?;
        match answer {
            Ok(Ok(response)) if u64::try_from(response.get_ref().committed_size) == Ok(size) => {
                Ok(())
            }
            Ok(Err(status)) => Err(lift(&status, None)),
            // An acknowledgement of other than the blob's size, or a call that panicked.
            other => Err(StorageError::Internal(format!(
                "the replica's answer to a {size}-byte write: {:?}",
                other.map(|r| r.map(|response| response.into_inner().committed_size))
            ))),
        }
    }

    /// Why a call that ended before the blob was complete did.
    async fn ended(&mut self) -> StorageError {
        match (&mut self.call.0).await {
            Ok(Err(status)) => lift(&status, None),
            other => StorageError::Internal(format!(
                "the replica answered before the blob was complete: {:?}",
                other.map(|_| ())
            )),
        }
    }
}
