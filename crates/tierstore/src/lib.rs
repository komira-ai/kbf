//! A hot/cold object store over S3-compatible backends.
//!
//! The crate knows nothing of what is stored: objects are immutable byte strings under string
//! keys, and it depends on no crate of the workspace it lives in, so that it can move to a
//! repository of its own. The one exception is the feature `s3` (off by default), which adds
//! `S3Backend`, an adapter over the S3 client of `kbf-objstore`; without it the crate has no
//! such dependency (its tests build `kbf-objstore` either way).
//!
//! - [`Backend`] is the minimal API of an S3-compatible store: a create-only put (it fails
//!   with [`Error::AlreadyExists`] where the key exists, as `If-None-Match: *` does), a put
//!   that replaces an object only if it has a given content tag (`If-Match`, for the few
//!   small objects the crate rewrites), get with an optional byte range, head (size, content
//!   tag and time of last write), delete, and list by prefix, whole or a page at a time. Its
//!   methods are async, as the object store clients of the workspace are, and boxed so that a
//!   backend is used as `dyn Backend`.
//! - [`MemoryBackend`] is an in-memory backend whose times come from a [`TestClock`] that a
//!   test sets by hand, and [`FaultyBackend`] wraps a backend to fail or delay chosen calls
//!   and to count them: both are for tests.
//! - `S3Backend` (feature `s3`, in the module `s3`) is a bucket of an S3-compatible store as
//!   a [`Backend`]: a create-only put is `If-None-Match: *`, a put that replaces is
//!   `If-Match`, a get is a ranged GET, a head gives the `ETag` as the content tag, and a
//!   listing follows the continuation tokens.
//! - [`Proxy`] is a store over a hot and a cold backend that is itself a [`Backend`]. Any
//!   number of proxies (one per node, each with its own id) work over the same two backends,
//!   and none needs state for correctness. A get reads the hot tier, else the cold tier, and
//!   copies what it found in the cold tier back into the hot tier; a head does not. A put is
//!   refused if the key exists in either tier, is written to the hot tier, and is
//!   acknowledged only after an entry for it is durable in a journal object
//!   `_journal/<proxy_id>/<seq>` in the hot tier. A delete removes the object from both tiers
//!   and journals the delete. A listing merges the two tiers. When the hot tier is full the
//!   proxy writes new objects straight to the cold tier.
//! - [`Reconciler`] is the one runner, guarded by a lease object in the hot tier. It tails
//!   the journals, copies each put from the hot tier to the cold tier (in parallel, bounded),
//!   applies the deletes, records how far it got in a checkpoint per proxy, evicts the
//!   oldest copied objects from the hot tier when it is over its high-water mark, deletes old
//!   journal objects, and publishes the hot tier's usage and the copy lag. It never lists
//!   the data prefixes of either tier: it lists `_tiering/proxies/` and the journal prefix of
//!   each proxy, and reads the journal objects, the checkpoints, the lease and the usage.
//! - [`CachedStore`] answers repeated reads of an object's ranges from RAM in front of any
//!   backend, typically a [`Proxy`]. It is a cache of the node it runs on, not state.
//!
//! The keys under `_journal/` and `_tiering/` belong to the proxy and the reconciler: a
//! client put or delete there is refused, and a listing leaves them out.
//!
//! Every proxy, reconciler and cache takes its settings as constructor parameters.

#![forbid(unsafe_code)]

mod backend;
mod cache;
mod clock;
mod error;
mod faulty;
mod journal;
mod lease;
mod memory;
mod reconciler;
#[cfg(feature = "s3")]
pub mod s3;
mod state;
mod tiered;

pub use backend::{Backend, ByteRange, Listed, Meta};
pub use cache::CachedStore;
pub use clock::{Clock, SystemClock};
pub use error::Error;
pub use faulty::{FaultyBackend, Op};
pub use journal::{JournalEntry, JournalOp, decode_journal, encode_journal};
pub use memory::{MemoryBackend, TestClock};
pub use reconciler::{PassReport, Reconciler, ReconcilerConfig};
#[cfg(feature = "s3")]
pub use s3::S3Backend;
pub use tiered::{Proxy, ProxyConfig};
