//! The methods of a started storage node, [`StorageHandle`]: what it holds and how it
//! serves and stops.

use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Hold, VerifiedBlob};
use kbf_mapsync::MapHolder;
use kbf_objstore::StillHeld;
use kbf_storage::Storage;
use kbf_storage::repair::Repairs;
use kbf_storage::service::CatchUp;

use super::{COLD_RELEASE_DEADLINE, StorageHandle};
use crate::serve::ServeError;

impl StorageHandle {
    /// Holds `blob` in this node's RAM without a write (`Cache::hold`), and says what
    /// became of it (`None`: the node's cache is gone). For the tests of the node's wiring
    /// (`--ram-ack-bytes` reaches the cache, and the stop drains what it holds); a write
    /// reaches the set through `StreamedPut::commit` with `Held`.
    #[doc(hidden)]
    pub fn hold(&self, blob: &VerifiedBlob) -> Option<Hold> {
        (self.holder)(blob)
    }

    /// The storage the listener serves, itself, with none of the service's checks: what a
    /// test writes and reads without a connection. It holds the node's cache, so a test that
    /// waits for [`StorageHandle::serve_checked`] to release the cold store drops it first.
    #[doc(hidden)]
    #[must_use]
    pub fn storage(&self) -> Arc<dyn Storage> {
        Arc::clone(&self.storage)
    }

    /// The copies this node owes: its repair journal. `None` for a node that does not
    /// replicate (no `--frontend`, or one that admits callers in plain text).
    #[must_use]
    pub fn repairs(&self) -> Option<Arc<Repairs>> {
        self.repairs.clone()
    }

    /// The storage map this node holds: what the frontend sent it when it registered and
    /// at each heartbeat that found it behind. Empty for a node with no `--frontend`.
    #[must_use]
    pub fn maps(&self) -> Arc<MapHolder> {
        Arc::clone(&self.maps)
    }

    /// Whether the node is rebuilding its index from the store, which it does at a start
    /// over a metadata log that never finished a rebuild (a new or wiped one) when it
    /// registers with `--frontend`. It serves meanwhile, answering a miss UNAVAILABLE;
    /// [`CatchUp::finished`] completes when the rebuild is done.
    #[must_use]
    pub fn catch_up(&self) -> CatchUp {
        self.catch_up.clone()
    }

    /// The line the node prints once it is bound:
    /// `kbf-storage <version> storage=<addr> log=<id>`.
    #[must_use]
    pub fn start_line(&self) -> &str {
        &self.start_line
    }

    /// Serves until the shutdown future given to [`start_storage`] completes and the
    /// calls in flight end; no new call is taken once it has completed. It then writes
    /// the blobs held in RAM to the disk and flushes the blocks staged on the hot store to
    /// the cold store, both within `--ram-drain-deadline` (`RamDrain::after`); what the
    /// deadline leaves is logged, and is on the hot store's disk for the next start.
    /// With a `file://` cold store it then waits for the store's lock to be released, so a
    /// start over the same directories can follow at once: the node's tasks are aborted
    /// and the cache they and the service held is closed, but its own background tasks and
    /// writer threads end after it is dropped, not before. The wait is bounded by
    /// [`COLD_RELEASE_DEADLINE`]; past it this logs one warning (the store's directory and
    /// how many handles are alive) and returns, because the process's exit releases the
    /// lock whatever still holds it, and the units run this with `TimeoutStopSec=infinity`,
    /// where a wait with no end would be a stop with none.
    ///
    /// The other serve path (`start::serve`) waits for nothing: the process exits when it
    /// returns, and its exit frees the lock.
    ///
    /// The tasks that hold handles on the cold store, to look at when the warning shows:
    /// the cache's touch flusher (`kbf-front`, `cache/touches.rs`) and its write-pool
    /// threads (`cache/pool.rs`).
    ///
    /// # Errors
    /// The listener fails while serving.
    pub async fn serve(self) -> Result<(), ServeError> {
        let (served, released) = self.serve_checked(COLD_RELEASE_DEADLINE).await;
        if let Err(held) = released {
            tracing::warn!(
                dir = %held.dir.display(),
                clones = held.clones,
                limit = ?held.limit,
                "the cold store is still open after serving ended; its lock is released when the process exits"
            );
        }
        served
    }

    /// [`StorageHandle::serve`] without the warning, saying whether the cold store was
    /// released within `limit` (on the runtime's clock): what the tests call, so a
    /// handle leaked by the node fails a test instead of hanging it or passing unseen.
    /// The result of serving comes first.
    #[doc(hidden)]
    pub async fn serve_checked(
        self,
        limit: Duration,
    ) -> (Result<(), ServeError>, Result<(), StillHeld>) {
        let Self {
            addr: _,
            metrics: _,
            start_line: _,
            storage,
            serving,
            _flusher,
            _background,
            _fill,
            cold_released,
            maps,
            catch_up,
            repairs,
            _joiner,
            _repair,
            _metrics,
            holder: _,
        } = self;
        let served = serving.await;
        // Everything the handle holds goes, so what is left holding the stores is not it.
        drop((
            _flusher,
            _background,
            _fill,
            storage,
            maps,
            catch_up,
            repairs,
            _joiner,
            _repair,
            _metrics,
        ));
        let released = match cold_released {
            Some(released) => released.wait_within(limit).await,
            None => Ok(()),
        };
        (served, released)
    }
}
