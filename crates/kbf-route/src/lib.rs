//! Calls to the storage nodes of a farm, routed by the storage map
//! (`docs/design/architecture-v2.md`, sections 6 to 8). The frontend and the daemon use
//! one [`Router`]: it answers which node holds a blob, reads it from the first owner that
//! does, and sends a write to the owner that takes it first.
//!
//! # What the router does
//!
//! - **Reads** ([`Router::read_range`], [`Router::read_blob`], [`Router::action_result`]):
//!   owners in rank order, then (while a join or a drain settles) the previous
//!   generation's. Only the answer "I do not hold it" moves on to the next owner without
//!   remembering a failure; a blob is `NOT_FOUND` only if every owner said so, and
//!   `UNAVAILABLE` if one could not. A read that breaks after its first byte goes on from
//!   the bytes already passed ([`MAX_RESUMES`]). Bytes are passed on chunk by chunk; the
//!   router holds no blob but the one [`Router::read_blob`] is asked for.
//! - **Read repair**: a read answered by an owner after an earlier owner of the same
//!   generation said it did not hold the blob has found a copy missing. A
//!   [`ReadRepair`] hook is told, at most as often as a [`RepairLimit`] allows.
//! - **One read per blob** for callers that ask at the same time, whichever owner
//!   answers ([`Router::read_blob`]).
//! - **FindMissing** ([`Router::find_missing`]) asks each digest's primary only, one call
//!   per primary, and puts the answers back in the order asked.
//! - **Writes** ([`Router::put`], [`Router::commit_action_record`]) go to the first
//!   write owner that is not `Down` in the map, and to that one only: the router does not
//!   write to the others or retry on another owner (the storage side's chain replication
//!   is av2-11).
//! - **`STALE_MAP`**: a node that holds a newer map answers [`StorageError::StaleMap`].
//!   The router fetches the newer map through the [`MapSource`] and runs the call again,
//!   once; a second answer, or a fetch that brings no newer map, is `UNAVAILABLE`.
//!
//! # What it does not do
//!
//! It is not a [`kbf_storage::Storage`]: that trait's `prepare_action_result` carries no
//! action digest, so it has no owner to go to until av2-12 decides how an entry's closure
//! is checked across owners. Writes take the blob whole (`Router::put`), as the storage
//! trait does today; a streamed write is the storage trait's to gain.
//!
//! # TLS
//!
//! [`TlsNodes`] verifies each node against its own name from the map and has no way to be
//! given one name for all of them.

mod find;
mod flights;
mod nodes;
mod read;
mod repair;
mod resume;
mod router;
mod source;
mod write;

pub use nodes::{Nodes, TlsNodes};
pub use repair::{ReadRepair, RepairLimit};
pub use resume::MAX_RESUMES;
pub use router::{Placement, Router};
pub use source::{MapSource, MapView, RefetchError};
