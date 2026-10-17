//! Where the map push (`Welcome.map`, `MapUpdate`, `STALE_MAP`) reads the current map.
//! A trait, so the push protocol does not depend on how the frontend keeps membership;
//! [`crate::members::Members::map_feed`] is the implementation over the state store.

use std::sync::Arc;

use kbf_map::Map;
use tokio::sync::watch;

/// The current storage map and a way to wait for the next one.
pub trait MapFeed: Send + Sync {
    /// The newest map this frontend knows.
    fn current(&self) -> Arc<Map>;

    /// The generation of [`MapFeed::current`]; the value changes on every bump.
    fn changes(&self) -> watch::Receiver<u64>;
}
