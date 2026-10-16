//! The storage map on the session (`docs/design/architecture-v2.md`, section 6): the map
//! of `Welcome` and of each `MapUpdate` goes into a [`MapHolder`] the routing layer reads,
//! and a generation the holder is waiting for ([`MapHolder::reach`]) is asked of the
//! frontend in a `MapRequest`.
//!
//! A map the daemon cannot read (a value this build does not name, or one the domain
//! refuses) is logged and not taken: the daemon goes on with the map it has, and a
//! frontend that sends such a map has a bug a farm of daemons should not each fail on.

use std::sync::Arc;

use kbf_map::Map;
use kbf_mapsync::MapHolder;
use kbf_proto::kbf::map::v1 as wire;
use kbf_proto::worker::MapRequest;
use tokio::sync::watch;

/// The daemon's end of the map: where pushed maps go, and what the holder asks for.
pub(crate) struct MapLink {
    holder: Arc<MapHolder>,
    asks: watch::Receiver<u64>,
}

impl MapLink {
    pub(crate) fn new(holder: Arc<MapHolder>) -> Self {
        let asks = holder.asks();
        Self { holder, asks }
    }

    /// Takes the map a `Welcome` or a `MapUpdate` carried. A holder keeps the newest, so
    /// a late or repeated map changes nothing; `None` (a server with no map) changes
    /// nothing either.
    pub(crate) fn take(&self, map: Option<wire::Map>) {
        let Some(map) = map else { return };
        match Map::try_from(map) {
            Ok(map) => {
                let generation = map.generation();
                if self.holder.offer(map) {
                    tracing::info!(generation, "storage map taken");
                }
            }
            Err(error) => tracing::warn!(%error, "a storage map the daemon cannot read ignored"),
        }
    }

    /// The request to send now, if the holder waits for a generation it does not have.
    /// Marks the wait seen, so [`Self::asked`] completes again only for a new one.
    pub(crate) fn request(&mut self) -> Option<MapRequest> {
        let at_least = *self.asks.borrow_and_update();
        (at_least > self.holder.generation()).then_some(MapRequest { at_least })
    }

    /// Completes when the holder is asked to reach a generation it was not before.
    async fn asked(&mut self) {
        // The link holds the holder, whose sender lives as long as it does: the wait
        // cannot fail.
        let _ = self.asks.changed().await;
    }
}

/// Completes when `link` has a new wait; never without a link. (A plain future rather
/// than a `Some(()) =` pattern in the select: a pattern arm adds a path nothing runs.)
pub(crate) async fn map_asked(link: Option<&mut MapLink>) {
    match link {
        Some(link) => link.asked().await,
        None => std::future::pending().await,
    }
}
