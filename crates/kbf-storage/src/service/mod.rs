//! The storage node's gRPC service, over any [`Storage`], and the client that calls it
//! ([`RemoteStorage`], which is a [`Storage`] again, so a caller keeps its
//! `Arc<dyn Storage>`).
//!
//! **The wire.** Blob calls are the standard protocols, because every REAPI client and
//! the daemon's transfer engine (`kbf-transfer`) already speak them:
//!
//! - `ContentAddressableStorage.FindMissingBlobs`. The batch calls and `GetTree` answer
//!   UNIMPLEMENTED: a frontend reaches single blobs through ByteStream, and the batch
//!   shapes are the frontend's to split.
//! - `ByteStream.Read`, ranged and streamed in chunks of at most
//!   [`crate::READ_CHUNK_BYTES`] from [`Storage::read_range`]; a blob is never collected.
//!   `Write`, whose bytes are held until they are complete (see [`Config`]), and
//!   `QueryWriteStatus`.
//! - `kbf.worker.v1.Segments`, the handlers of [`Storage::segments`].
//!
//! and `kbf.storage.v1.ActionStore` (`Get`, `Prepare`, `Commit`) is what they have no
//! message for. Errors are [`StorageError`]'s gRPC codes (`From<StorageError> for
//! Status`); the one exception is an action-cache write refusal, which `Prepare` and
//! `Commit` answer in the message, since a status cannot carry the blob it names.
//!
//! **The map generation** (section 6) rides on every call: a field of every
//! `kbf.storage.v1` request, and the [`MAP_GENERATION_HEADER`] metadata entry on the
//! standard calls. [`RemoteStorage`] sends it. A node with no map ignores it; one given a
//! [`crate::replicate::Chain`] ([`StorageService::with_chain`]) refuses a `Write` from a
//! caller whose generation is not the map's: STALE_MAP for one behind it, UNAVAILABLE for
//! one ahead of it. The other calls do not read it yet.
//!
//! **Who may call** is [`Admission`]: the role in the caller's certificate against the
//! endpoint's table. Roles are the part of this unit that is partly a seam; the module
//! docs of [`admission`] say which.
//!
//! **How `Write` takes a blob in.** Where the store offers a [`crate::PutStream`]
//! ([`Storage::stream`]), each message is appended to it as it arrives and the blob is
//! never held whole; the primary of a chain streams it on to the replica too
//! ([`crate::replicate`]). Where it does not (no store hands one out yet), a node that is
//! in no chain holds the blob until it is complete, within
//! [`Config::write_bytes_in_flight`].

mod actions;
mod admission;
mod bytestream;
mod cas;
mod client;
mod generation;
mod segments;
#[cfg(test)]
pub(crate) mod tests;
mod wire;
mod write;

use std::sync::Arc;

use kbf_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorageServer;
use kbf_proto::storage::action_store_server::ActionStoreServer;
use kbf_proto::worker::segments_server::{Segments, SegmentsServer};
use tokio::sync::Semaphore;
use tonic::service::Routes;

use crate::Storage;
use crate::replicate::Chain;

pub use admission::{Admission, Endpoint, PeerRole, ROLE_URI_PREFIX, role_of};
pub use client::RemoteStorage;
pub(crate) use client::lift;

/// The metadata entry that carries the map generation on the standard calls.
pub const MAP_GENERATION_HEADER: &str = "kbf-map-generation";

/// The largest gRPC message the service decodes and sends: a full 16 MiB batch and room
/// around it, `kbf-front`'s `MAX_MESSAGE_BYTES` (a test pins the two equal).
pub const MAX_MESSAGE_BYTES: usize = (16 << 20) + (1 << 20);

/// The largest blob a `Write` accepts, `kbf-front`'s `MAX_BLOB_BYTES` (pinned the same).
pub const MAX_BLOB_BYTES: u64 = 1 << 30;

/// The most digests one `FindMissingBlobs` may name. `kbf-front` sets none beyond the
/// message size, and a full message could name 262 144 of them (17 MiB over the 68 bytes
/// of the smallest digest), each a lookup in the store, so the node bounds the work of
/// one call below that: 65 536, 64 times the 1 000 a transfer engine asks about by
/// default. A call over it is INVALID_ARGUMENT, not RESOURCE_EXHAUSTED: REAPI uses that
/// code for a server out of space, and a client that reads it retries; this refusal
/// cannot pass by retrying, only by splitting the call.
pub const MAX_FIND_MISSING_DIGESTS: usize = 1 << 16;

/// What the service is set up with.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// How many bytes of blobs `Write` calls hold at once. A `Write` takes room for the
    /// blob it declares before it reads past the first message, and waits for it (never
    /// refusing for it), so concurrent uploads cannot hold more than this, whatever
    /// their number. A blob larger than the bound takes all of it.
    pub write_bytes_in_flight: u32,
}

/// The service: every call of the module docs, over one [`Storage`].
pub struct StorageService {
    storage: Arc<dyn Storage>,
    segments: Arc<dyn Segments>,
    admission: Admission,
    chain: Option<Arc<Chain>>,
    write_room: Semaphore,
    write_room_bytes: u32,
}

impl StorageService {
    /// The service over `storage`, admitting callers as `admission` says.
    #[must_use]
    pub fn new(storage: Arc<dyn Storage>, admission: Admission, config: Config) -> Self {
        Self {
            segments: storage.segments(),
            storage,
            admission,
            chain: None,
            write_room: Semaphore::new(config.write_bytes_in_flight as usize),
            write_room_bytes: config.write_bytes_in_flight,
        }
    }

    /// The service as a node of a chain ([`crate::replicate`]): it judges writes by the
    /// chain's map and, as the primary of a blob, copies it to the replica before it
    /// answers.
    #[must_use]
    pub fn with_chain(mut self, chain: Arc<Chain>) -> Self {
        self.chain = Some(chain);
        self
    }

    /// The service as routes, ready to be added to a tonic server.
    #[must_use]
    pub fn routes(self) -> Routes {
        let this = Arc::new(self);
        Routes::new(
            ContentAddressableStorageServer::from_arc(Arc::clone(&this))
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ByteStreamServer::from_arc(Arc::clone(&this))
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            SegmentsServer::from_arc(Arc::clone(&this))
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(
            ActionStoreServer::from_arc(this)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
    }
}
