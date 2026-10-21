//! The reads a server makes of its state store by itself as it starts, for a test that arms a
//! one-shot injection of [`Hooked`] (`fail_gets`, `panic_gets`, `after_get`) for the request it
//! sends next.
//!
//! An injection is met by the next read of the store, whoever makes it, and the server's own
//! tasks read through the same handle as the handlers under test, so nothing tells their reads
//! apart from a handler's: one between the injection and the request would meet it, and the
//! request would get a healthy store. The tasks that read the store by themselves look once,
//! at once, as the server starts (the first tick of an interval is immediate), and then a
//! long way off:
//!
//! - the update controller, every `Listeners::tick` (a test of this kind gives it a tick that
//!   does not come within the test): `update/policy` and `update/desired`, and more rows
//!   once a release is desired, `update/desired` again among them;
//! - the offers' poll, with a release store and a state store, every tick: `update/desired`;
//! - the membership sweeper, every beat (a third of the member ttl): `map`, `member/` and
//!   last `deny/`.
//!
//! A test waits for these ([`Looks::done`]) before it arms an injection, and holds the
//! numbers it waits for with a test of its own that the server reads nothing more.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::hooked::Hooked;

/// The counts of the reads of `update/desired` and of `deny/`: the last read of the
/// controller's look is one of `update/desired`, and the last of the sweeper's refresh is
/// the one of `deny/`. How many of the first a start makes depends on the setup (a release
/// offered before the start, a release store for the poll), so each test names its number.
pub struct Looks {
    desired: Arc<AtomicUsize>,
    deny: Arc<AtomicUsize>,
}

impl Looks {
    /// Counts from now on: call it before the server starts.
    pub fn count(hooked: &Hooked) -> Self {
        Self {
            desired: hooked.count("update/desired"),
            deny: hooked.count("deny/"),
        }
    }

    /// The reads of `(update/desired, deny/)` so far.
    pub fn seen(&self) -> (usize, usize) {
        (
            self.desired.load(Ordering::SeqCst),
            self.deny.load(Ordering::SeqCst),
        )
    }

    /// Waits until `want` reads of `(update/desired, deny/)` are made: the server's own reads
    /// at start are done. Armed before that, an injection would be met by one of them and not
    /// by the request it is for.
    pub async fn done(&self, want: (usize, usize)) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while self.seen().0 < want.0 || self.seen().1 < want.1 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the server read (update/desired, deny/) {:?} times at start, not {want:?}",
                self.seen()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}
