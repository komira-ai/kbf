//! Readiness of the members of a pool: one task per member asks its operator listener
//! `GET /readyz` every second, with the question the slot-mode poll asks
//! ([`crate::probe::Readiness::ask`]: a 200 within 3 s is a pass, anything else is not), and
//! records the answer on the member.
//!
//! The answer is the member's whole readiness: a pass makes it ready at once, a failed poll
//! makes it not ready at once, and a stream that could not connect to it makes it not ready
//! until the next pass ([`crate::upstreams::Upstreams::failed`]). Readiness only steers the
//! next pick toward the members that are ready; it never closes a stream the member has.

use std::sync::Arc;

use tokio::time::{MissedTickBehavior, interval};

use crate::core::Core;
use crate::pool::member_addr;
use crate::probe::POLL_EVERY;
use crate::route::Target;
use crate::upstreams::Upstreams;

/// Polls member `index` of `members` every [`POLL_EVERY`] until the task is dropped.
pub(crate) async fn poll(core: Arc<Core>, members: Arc<Upstreams>, index: usize) {
    let addr = member_addr(members.base(index), Target::Operator);
    let mut tick = interval(POLL_EVERY);
    // A poll that took its 3 s is not made up for with a burst.
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        members.set_ready(index, core.ready.ask(addr).await);
    }
}
