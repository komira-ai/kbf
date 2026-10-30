//! The eviction pass, the record of use, and the loop that runs them.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

use super::disk::Space;
use super::layout::data_path;
use super::metrics::{Evicted, bump};
use super::{Inner, LocalCache, locked, percent_of};

/// What a pass of the eviction did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    /// The objects removed.
    pub evicted: usize,
    /// The bytes of those objects.
    pub freed: u64,
    /// The objects skipped because another name holds them (a link count over one).
    pub pinned: usize,
}

/// Clears the flag of a running pass when the pass ends, however it ends.
struct Running<'a>(&'a AtomicBool);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Inner {
    /// Writes the time of the last use of each object used since the last call to its file, so
    /// that the order of use outlives the process. One call to the system for each object, however
    /// often it was used.
    pub(super) fn flush_touches(&self) {
        let touched = locked(&self.index).take_touched();
        for (hex, at) in touched {
            // An object that was evicted since it was used has no file to stamp.
            let _ = self.disk.touch(&data_path(&self.config.path, &hex), at);
        }
    }

    /// One pass: removes the least recently used objects until the cache is down to its low water
    /// mark, if it is above its high water mark, and until the free space is back at the floor, if
    /// it is below it. Does nothing if a pass is running.
    pub(super) fn evict_pass(&self) -> Pass {
        if self.evicting.swap(true, Ordering::SeqCst) {
            return Pass::default();
        }
        let _running = Running(&self.evicting);
        self.flush_touches();
        let (held, order) = {
            let index = locked(&self.index);
            (index.bytes(), index.oldest_first())
        };
        let config = &self.config;
        let budget_need = if held > percent_of(config.budget, config.high_water) {
            held.saturating_sub(percent_of(config.budget, config.low_water))
        } else {
            0
        };
        let nothing = Space { total: 0, free: 0 };
        let space = self.disk.space(&config.path).unwrap_or(nothing);
        let floor_need = percent_of(space.total, config.free_floor).saturating_sub(space.free);
        let need = budget_need.max(floor_need);
        let mut pass = Pass::default();
        for (hex, size) in order {
            if pass.freed >= need {
                break;
            }
            match self.disk.links(&data_path(&config.path, &hex)) {
                Ok(links) if links > 1 => {
                    bump(&self.counters.pinned_skips);
                    pass.pinned += 1;
                    continue;
                }
                Ok(_) => {}
                Err(_) => {
                    // The file is gone already: forget it, which frees nothing.
                    drop(locked(&self.index).remove(&hex));
                    continue;
                }
            }
            // The first bytes freed are for the budget, the rest for the floor.
            let why = if pass.freed < budget_need {
                Evicted::Budget
            } else {
                Evicted::Floor
            };
            self.drop_entry(&hex);
            self.counters.evicted(why);
            pass.evicted += 1;
            pass.freed += size;
        }
        pass
    }
}

impl LocalCache {
    /// One pass of the eviction now (see the module docs).
    #[must_use]
    pub fn evict(&self) -> Pass {
        self.inner.evict_pass()
    }

    /// Runs a pass every `interval` until `halt` is notified, then writes the record of use.
    /// This, and the fills the misses start, are all the cache does on its own.
    pub async fn maintain(&self, interval: Duration, halt: Arc<Notify>) {
        loop {
            tokio::select! {
                biased;
                () = halt.notified() => break,
                () = tokio::time::sleep(interval) => {}
            }
            let inner = Arc::clone(&self.inner);
            let pass = tokio::task::spawn_blocking(move || inner.evict_pass());
            pass.await.expect("a pass does not panic");
        }
        self.inner.flush_touches();
    }
}
