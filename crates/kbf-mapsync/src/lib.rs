//! The storage map in motion (`docs/design/architecture-v2.md`, section 6).
//!
//! Every process knows one name, the frontend's. The frontend holds the map and pushes it:
//! in `Welcome` when a daemon's session opens, and in a `MapUpdate` on that session each
//! time the map changes. This crate is the two ends of that, with nothing about the wire
//! or the network in it:
//!
//! - [`MapSource`]: what the frontend end implements, the map it holds now and a signal
//!   that it changed. The session handler of `kbf-server` reads it; the registration
//!   change implements it over the state store; [`FakeMapSource`] implements it over a
//!   cell a test sets.
//! - [`MapHolder`]: what a daemon or a storage node keeps the newest pushed map in. It
//!   never goes back: a map of an older or the same generation is refused, so a push that
//!   arrives late, twice or out of order changes nothing.
//!
//! # Lost pushes
//!
//! A push is not relied on. A node that finds it is behind (a storage node answered
//! `STALE_MAP` naming a newer generation) calls [`MapHolder::reach`], which records that
//! generation as wanted ([`MapHolder::asks`]; the session turns it into a `MapRequest`,
//! to which the frontend answers with its current map) and waits for a map that new. A
//! process that is ahead of a node, and a node that was never told, are the node's to
//! wait out the same way.

mod fake;
mod holder;

use std::sync::Arc;

use kbf_map::Map;
use tokio::sync::watch;

pub use fake::FakeMapSource;
pub use holder::MapHolder;

/// The map a frontend holds, and when it changes.
///
/// A source has a map from the start: a frontend that has none does not serve one.
pub trait MapSource: Send + Sync {
    /// The map held now.
    fn current(&self) -> Arc<Map>;

    /// A receiver whose value is the generation of the map held, and which is notified
    /// each time it changes. A caller that wants the map for the tick reads
    /// [`MapSource::current`] after it, so it may see a map newer than the generation it
    /// was woken for, never an older one (a source has stored the map by the time it
    /// announces its generation). A caller that has to miss no change subscribes before
    /// it reads `current`.
    fn changes(&self) -> watch::Receiver<u64>;
}
