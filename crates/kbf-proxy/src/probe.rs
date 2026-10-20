//! Readiness: `/readyz` is the proxy's own answer, and the proxy has one writer for it, a
//! poll of the active child (design note decision C10).
//!
//! Once a second the poll asks the active child's operator listener `GET /readyz` over plain
//! HTTP/1.1 on loopback, and gives the child 3 s to answer. `/readyz` is 200 only while the
//! gate has an active slot and a poll of that very slot, in that very generation, answered 200
//! within the last 5 s. Nothing else sets it, a supervisor cannot, and it never feeds back:
//! a failed poll does not close a stream, move the active slot or refuse a new stream, since
//! a child that answers late (a quiesce, a Postgres takeover) is still the child its streams
//! are bound to.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{MissedTickBehavior, interval, timeout};

use crate::core::{Core, now};
use crate::pool::{Connector, upstream_addr};
use crate::route::Target;
use crate::slot::{ActiveSlot, Slot};

/// How often the active child is polled.
pub(crate) const POLL_EVERY: Duration = Duration::from_secs(1);
/// How long a child has to answer one poll, connecting included.
pub(crate) const POLL_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a passing poll keeps `/readyz` at 200.
pub(crate) const FRESH_FOR: Duration = Duration::from_secs(5);

/// The request of a poll: the child's own readiness path, HTTP/1.1, closed after the answer.
const REQUEST: &[u8] = b"GET /readyz HTTP/1.1\r\nHost: kbf-proxy\r\nConnection: close\r\n\r\n";
/// The start of a 200 status line; the poll reads no more of the answer than this.
const OK_LINE: &[u8] = b"HTTP/1.1 200";

/// The last poll that passed: of which child, and when.
#[derive(Clone, Copy, Debug)]
struct Passed {
    slot: Slot,
    generation: u64,
    at: Instant,
}

/// What `/readyz` says, and the connector the poll dials with.
pub(crate) struct Readiness {
    connector: Connector,
    passed: Mutex<Option<Passed>>,
}

impl Readiness {
    pub(crate) fn new(connector: Connector) -> Self {
        Self {
            connector,
            passed: Mutex::new(None),
        }
    }

    /// Whether `/readyz` is 200 at `at`: `slots` has an active slot, and the last passing poll
    /// was of that slot's current child (a slot that was retired and opened again is a new
    /// child, and inherits nothing) no more than [`FRESH_FOR`] ago. The active slot and its
    /// generation are two reads: a flip between them can answer 503 for a child that is ready,
    /// never 200 across a changed child.
    pub(crate) fn is_ready(&self, slots: &ActiveSlot, at: Instant) -> bool {
        let Some(active) = slots.active() else {
            return false;
        };
        let passed = *self.passed.lock().unwrap_or_else(PoisonError::into_inner);
        passed.is_some_and(|p| {
            p.slot == active
                && p.generation == slots.generation(active)
                && at.saturating_duration_since(p.at) <= FRESH_FOR
        })
    }

    pub(crate) fn pass(&self, slot: Slot, generation: u64, at: Instant) {
        *self.passed.lock().unwrap_or_else(PoisonError::into_inner) = Some(Passed {
            slot,
            generation,
            at,
        });
    }

    /// One poll of `addr`: true only for a 200 within [`POLL_TIMEOUT`]. A refused connection,
    /// a hang, a reset, a 503 and anything that is not HTTP are all "not ready".
    async fn ask(&self, addr: SocketAddr) -> bool {
        let asked = async {
            let mut io = (self.connector)(addr).await.ok()?;
            io.write_all(REQUEST).await.ok()?;
            let mut line = [0_u8; OK_LINE.len()];
            io.read_exact(&mut line).await.ok()?;
            Some(line == *OK_LINE)
        };
        timeout(POLL_TIMEOUT, asked)
            .await
            .ok()
            .flatten()
            .unwrap_or(false)
    }
}

/// Polls the active child every [`POLL_EVERY`] until the task is dropped. The generation is
/// read before the question, so a child that is replaced while it is asked is recorded under
/// the old generation, which [`Readiness::is_ready`] no longer matches.
pub(crate) async fn poll(core: Arc<Core>) {
    let mut tick = interval(POLL_EVERY);
    // A poll that took its 3 s is not made up for with a burst.
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Some(slot) = core.slots.active() else {
            continue;
        };
        let generation = core.slots.generation(slot);
        let addr = upstream_addr(core.table(), slot, Target::Operator);
        if core.ready.ask(addr).await {
            core.ready.pass(slot, generation, now());
        }
    }
}
