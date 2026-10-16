//! Writes across storage nodes (`docs/design/architecture-v2.md`, sections 6 and 8): which
//! map a node judges a call by, and what it answers a caller whose map is not the
//! current one.
//!
//! **The map a node holds.** A node learns the current map from the frontend (the
//! registration stream's `Welcome` and `MapUpdate`, a separate change), and this crate
//! does not: it asks a [`MapSource`], which says what the node holds now. A node with no
//! source, or whose source has no map yet, is a one-member farm and judges nothing.
//!
//! **STALE_MAP.** Every call carries the generation of the map its caller routed by
//! (`kbf.storage.v1`'s `map_generation`, or the `kbf-map-generation` entry on the standard
//! calls). A write to a node that holds a different map is not served:
//! - a caller behind the node (an older generation, or 0, "no map seen") gets
//!   [`StorageError::StaleMap`] with the node's generation, so it can fetch that map and
//!   try again: FAILED_PRECONDITION with the generation in the `kbf-map-generation`
//!   trailer;
//! - a caller ahead of the node (the node has not been told of the generation the caller
//!   has) makes the node fetch it: [`MapSource::refetch`], waited for at most
//!   [`REFETCH_WAIT`]. If the map arrives the call goes on under it (served if it is the
//!   caller's generation, STALE_MAP if the node is now past it). If it does not, the call
//!   gets UNAVAILABLE: the caller's map may be the right one, and this node cannot tell,
//!   so it neither serves the call nor claims the caller is stale. A lost push is
//!   harmless: the node learns of the map, and the same call then succeeds.
//!
//! The check runs on every call (the blob calls, the action-cache calls and the segment
//! calls) before it reads a byte or looks at what the node holds, so no answer to a stale
//! caller, not even "already stored", is given under a placement the node no longer
//! holds.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kbf_map::Map;

use crate::StorageError;

/// The map a node holds now.
pub trait MapSource: Send + Sync {
    /// The current map, or `None` before the node has one (a farm of one member).
    fn current(&self) -> Option<Arc<Map>>;

    /// The map of generation `at_least` or a newer one, fetched from a frontend, or
    /// `None` if the node cannot get one. Called when a caller routed by a generation the
    /// node has not been told of; the caller of this method bounds the wait
    /// ([`REFETCH_WAIT`]), so an implementation may wait for the next push or ask.
    ///
    /// The default is a node that learns its map only from pushes: it fetches nothing.
    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Option<Arc<Map>>> {
        let _ = at_least;
        Box::pin(async { None })
    }
}

/// How long a call routed by a map this node has not been told of waits for the node to
/// fetch it.
pub const REFETCH_WAIT: Duration = Duration::from_secs(2);

/// This node is at a map generation older than the one its caller routed by.
#[derive(Debug, thiserror::Error)]
#[error(
    "this node holds map generation {current} and the caller routed by {requested}: the \
     node has not been told of it yet"
)]
struct NotYetSeen {
    requested: u64,
    current: u64,
}

/// Whether a call routed by map generation `requested` may be served by the node `source`
/// speaks for: always, for a node with no map yet. A caller ahead of the node makes it
/// fetch the map, waiting at most `wait`.
///
/// # Errors
/// [`StorageError::StaleMap`] if `requested` is older than the node's generation (0 is
/// older than every map's); [`StorageError::Unavailable`] if it is newer and the node
/// could not get that map in time.
pub(crate) async fn check(
    source: &dyn MapSource,
    requested: u64,
    wait: Duration,
) -> Result<(), StorageError> {
    let Some(held) = source.current() else {
        return Ok(());
    };
    if requested <= held.generation() {
        return check_generation(&held, requested);
    }
    let fetched = tokio::time::timeout(wait, source.refetch(requested))
        .await
        .ok()
        .flatten();
    check_generation(&fetched.unwrap_or(held), requested)
}

/// Whether a call routed by map generation `requested` may be served by a node holding
/// `map`.
///
/// # Errors
/// [`StorageError::StaleMap`] if `requested` is older than the map's generation (0 is
/// older than every map's); [`StorageError::Unavailable`] if it is newer.
pub(crate) fn check_generation(map: &Map, requested: u64) -> Result<(), StorageError> {
    let current = map.generation();
    if requested < current {
        return Err(StorageError::StaleMap { current });
    }
    if requested > current {
        return Err(StorageError::Unavailable(Arc::new(NotYetSeen {
            requested,
            current,
        })));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use kbf_map::{Map, Member, MemberId, MemberState, Rule};

    use super::*;

    /// Catches an off-by-one at the generation boundary, and 0 ("no map seen") treated as
    /// current: behind is stale, equal is served, ahead is unavailable.
    #[test]
    fn behind_is_stale_equal_is_served_ahead_is_unavailable() {
        let member = Member {
            id: MemberId(1),
            addr: "127.0.0.1:1".to_owned(),
            zone: String::new(),
            weight: 1,
            state: MemberState::Active,
        };
        let map = Map::new(7, vec![member], Rule::Rendezvous, 1, 1, Vec::new()).expect("map");
        for behind in [0, 1, 6] {
            assert!(matches!(
                check_generation(&map, behind),
                Err(StorageError::StaleMap { current: 7 })
            ));
        }
        assert!(check_generation(&map, 7).is_ok());
        for ahead in [8, u64::MAX] {
            assert!(matches!(
                check_generation(&map, ahead),
                Err(StorageError::Unavailable(_))
            ));
        }
    }
}
