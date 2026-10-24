//! The farm's state store: the trait the scheduler writes every change through, an
//! in-memory fake, and the conformance suite every backend must pass.
//!
//! The contract is section 4.1 of `docs/design/architecture-v2.md`:
//!
//! - **C1** compare-and-set per key with a version ([`Cond`], [`Version`]);
//! - **C2** atomic multi-key transactions ([`Txn`]);
//! - **C3** strictly monotonic counters ([`Op::Next`]);
//! - **C4** time-to-live keys expired by the store's own [`Clock`], observable in the
//!   feed ([`Event::Expire`]);
//! - **C5** ordered prefix scans and a change feed a poller resumes from a [`Revision`]
//!   without skipping a row ([`Store::changes`]);
//! - **C6** an acknowledgement is durable across restart;
//! - **C7** group commit ([`Store::apply_batch`], [`GROUP_COMMIT_WINDOW`]);
//! - **C8** a bounded, ordered range scan under a prefix, between two bounds, forward or
//!   in reverse, that pages with a [`ResumeToken`] ([`Store::scan_range`], [`ScanRange`]).
//!
//! **Shared writers**: frontends that share a store take no generation. [`share`] leaves
//! the row [`GENERATION`] holding [`SHARED_WRITERS`], which no generation can be read from,
//! so an older build that takes generations is refused by the store and none is started
//! beside the sharing frontends.
//!
//! With the `postgres` feature, [`postgres`] is the Postgres backend.
//!
//! **The pack index** ([`pack_index`], [`PackIndex`]) records where each blob lies inside a
//! pack, in tables of the Postgres schema that are outside the key/value and feed tables and
//! outside the commit head. It is a separate typed API beside [`Store`]: a Postgres store
//! opens one with `postgres::PostgresPackIndex`, and `pack_index::memory` (feature
//! `test-support`) is the in-memory index; the same conformance clauses run against both.
//!
//! With the `test-support` feature (and in this crate's own unit tests) `conformance`
//! states each clause as a function generic over a `conformance::Backend` factory, and
//! `fake` is the in-memory backend, with a deliberately broken variant per clause that
//! the suite must reject. A default build has neither: a crate that tests against the
//! fake enables the feature in its `[dev-dependencies]`.

pub mod clock;
#[cfg(any(test, feature = "test-support"))]
pub mod conformance;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod pack_index;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(any(test, feature = "test-support"))]
pub mod recording;
mod shared;
pub mod sqlite;
mod store;
mod types;

use std::time::Duration;

pub use clock::{Clock, ManualClock, StoreTime, SystemClock};
pub use pack_index::{
    BlobDigest, Location, MAX_BATCH, NewPack, PackBlob, PackIndex, PackIndexError, PackInfo,
    Recorded,
};
pub use shared::{GENERATION, SHARED_WRITERS, share};
pub use store::Store;
pub use types::{
    Change, Changes, Commit, CommitStats, Cond, Conflict, Direction, Event, Op, ResumeToken,
    Revision, ScanPage, ScanRange, StoreError, Txn, Version, Versioned,
};

/// The longest a write may wait for the group it commits with (target 5 ms).
pub const GROUP_COMMIT_WINDOW: Duration = Duration::from_millis(5);

/// The write rate the store must sustain in bursts: a burst of 300 actions a second at
/// about five durable writes each.
pub const TARGET_WRITES_PER_SECOND: u64 = 1500;
