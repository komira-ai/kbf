//! The room for uploads: one budget of bytes for all the uploads into packs of a frontend.
//!
//! An upload takes the bytes it claims once its resource name has arrived, before it reads on
//! past the frames that carry the name, and gives them back when it is done, however it ended.
//! An upload that finds the budget spent waits for the uploads ahead of it to give bytes back
//! (in the order they came, so that a large upload is not passed over for ever by small ones),
//! and reads no more meanwhile, so that the client's HTTP/2 window is the only buffer it fills.
//! An upload that is alone over the whole budget could never be admitted, so it is refused at
//! once.
//!
//! The budget counts blob bytes, not the memory of the process: the buffer of an upload may be
//! briefly larger than the bytes it holds while it grows, and a message being cut is buffered
//! on top (up to the room the rest of the blob can fill).
//!
//! An upload takes its bytes once, whole, and needs nothing more to finish, so uploads do not
//! wait on one another in a cycle.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tonic::Status;

use super::metrics::PackMetrics;

/// The bytes that the uploads into packs of a frontend may hold in flight together when
/// nothing says otherwise: 1 GiB.
pub const DEFAULT_INFLIGHT_BYTES: u32 = 1 << 30;

/// The bytes of uploads in flight, up to a cap. Cheap to clone; every clone is the same budget.
#[derive(Clone)]
pub(super) struct Budget {
    cap: u32,
    permits: Arc<Semaphore>,
    metrics: Arc<PackMetrics>,
}

impl Budget {
    /// A budget of `cap` bytes, counted in `metrics`.
    pub(super) fn new(cap: u32, metrics: Arc<PackMetrics>) -> Self {
        Self {
            cap,
            permits: Arc::new(Semaphore::new(cap as usize)),
            metrics,
        }
    }

    /// The bytes of the whole budget.
    pub(super) const fn cap(&self) -> u32 {
        self.cap
    }

    /// `bytes` of the budget, once the uploads ahead have given enough back. They are given
    /// back when the [`Room`] is dropped, and the call waits, taking nothing, until they are
    /// there; a call dropped while it waits takes nothing.
    ///
    /// # Errors
    /// RESOURCE_EXHAUSTED when `bytes` is over the whole budget.
    ///
    /// # Panics
    /// Never: the semaphore of the budget is not closed.
    pub(super) async fn reserve(&self, bytes: u64) -> Result<Room, Status> {
        if bytes > u64::from(self.cap) {
            self.metrics.upload_refusals.add(1);
            tracing::warn!(
                bytes,
                cap = self.cap,
                "an upload is over the bytes that uploads into packs may hold in flight"
            );
            return Err(Status::resource_exhausted(format!(
                "an upload of {bytes} bytes is over the {} bytes that uploads into packs may \
                 hold in flight at once (--pack-inflight-bytes)",
                self.cap
            )));
        }
        // Not over the cap, which is a `u32`.
        let permits = u32::try_from(bytes).unwrap_or(self.cap);
        let semaphore = Arc::clone(&self.permits);
        let taken = Arc::clone(&semaphore).try_acquire_many_owned(permits);
        let permit = if let Ok(permit) = taken {
            permit
        } else {
            self.metrics.upload_waits.add(1);
            tracing::debug!(bytes, "an upload waits for room");
            let waited = semaphore.acquire_many_owned(permits).await;
            waited.expect("the semaphore of a budget is never closed")
        };
        self.metrics.reserved.take(bytes);
        Ok(Room {
            metrics: Arc::clone(&self.metrics),
            bytes,
            _permit: permit,
        })
    }
}

/// Bytes of the budget, given back when this is dropped.
#[derive(Debug)]
pub(super) struct Room {
    metrics: Arc<PackMetrics>,
    bytes: u64,
    // Dropped after the count is lowered (below), so that the count never passes the cap.
    _permit: OwnedSemaphorePermit,
}

impl Drop for Room {
    fn drop(&mut self) {
        self.metrics.reserved.give(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tonic::Code;

    use super::*;

    /// A budget of `cap` bytes and what it counts.
    fn budget(cap: u32) -> (Budget, Arc<PackMetrics>) {
        let metrics = Arc::<PackMetrics>::default();
        (Budget::new(cap, Arc::clone(&metrics)), metrics)
    }

    /// Lets every task that can run run until it waits: the paused clock moves on only when
    /// none can.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    /// Asserts that a reservation that finds the budget spent waits and takes nothing, is
    /// counted as a wait, and goes on when enough is given back, and that the bytes held never
    /// pass the cap.
    #[tokio::test(start_paused = true)]
    async fn a_reservation_waits_until_room_is_given_back() {
        let (budget, metrics) = budget(100);
        let first = budget.reserve(60).await.expect("room");
        let second = budget.reserve(40).await.expect("room");
        assert_eq!(metrics.inflight_bytes(), 100);
        assert_eq!(metrics.upload_waits.get(), 0);
        let waiter = budget.clone();
        let waiting = tokio::spawn(async move { waiter.reserve(30).await });

        settle().await;

        assert!(!waiting.is_finished());
        assert_eq!(metrics.upload_waits.get(), 1);
        assert_eq!(metrics.inflight_bytes(), 100);
        drop(first);
        let third = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("not left waiting")
            .expect("joined")
            .expect("room");
        assert_eq!(metrics.inflight_bytes(), 70);
        drop((second, third));
        assert_eq!(metrics.inflight_bytes(), 0);
        assert_eq!(metrics.inflight_peak_bytes(), 100);
    }

    /// Asserts that a reservation larger than the whole budget is refused RESOURCE_EXHAUSTED
    /// at once, naming the flag and counted, however large (one that fits exactly is
    /// admitted), and takes nothing.
    #[tokio::test(start_paused = true)]
    async fn a_reservation_over_the_whole_budget_is_refused_and_not_queued() {
        let (budget, metrics) = budget(100);
        let exact = budget.reserve(100).await.expect("room");
        drop(exact);

        for over in [101, u64::MAX] {
            let refused = budget.reserve(over).await.expect_err("refused");
            assert_eq!(refused.code(), Code::ResourceExhausted);
            let said = refused.message();
            assert!(said.contains("--pack-inflight-bytes"), "{said}");
        }

        assert_eq!(metrics.upload_refusals.get(), 2);
        assert_eq!(metrics.upload_waits.get(), 0);
        assert_eq!(metrics.inflight_bytes(), 0);
    }

    /// Asserts that a reservation dropped while it waits takes nothing and leaves nothing
    /// behind: once the others give their bytes back, the whole budget can be taken.
    #[tokio::test(start_paused = true)]
    async fn a_reservation_that_gives_up_waiting_leaves_the_budget_whole() {
        let (budget, metrics) = budget(100);
        let held = budget.reserve(80).await.expect("room");
        let waiter = budget.clone();
        let waiting = tokio::spawn(async move { waiter.reserve(50).await });
        settle().await;
        assert!(!waiting.is_finished());

        waiting.abort();
        settle().await;
        drop(held);

        let all = tokio::time::timeout(Duration::from_secs(1), budget.reserve(100))
            .await
            .expect("not left waiting")
            .expect("the whole budget");
        assert_eq!(metrics.inflight_bytes(), 100);
        drop(all);
        assert_eq!(metrics.inflight_bytes(), 0);
    }
}
