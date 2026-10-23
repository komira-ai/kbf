//! What the update row and reason tests share: ids and times, the defaults, and the
//! backing state stores.

#![allow(dead_code)]

use kbf_server::update::{Generation, OrderId, Policy};
use kbf_types::{FarmTime, WorkerId};

pub mod harness;
pub mod stores;

/// The time of the look, in seconds. Large, so a test can say a step began hours before.
pub const NOW: u64 = 1_000_000;
/// The generation of the run.
pub const GENERATION: Generation = Generation(7);
/// The store's epoch the orders of these tests are drawn under.
pub const EPOCH: u64 = 1;
/// The activation order a host's row holds (the one the host's current update was given).
pub const ORDER: OrderId = OrderId::new(EPOCH, 41);

/// The farm time `secs` seconds after the epoch.
pub fn t(secs: u64) -> FarmTime {
    FarmTime::from_millis(secs * 1000)
}

/// The worker called `name`.
pub fn id(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// The defaults of the design.
pub fn policy() -> Policy {
    Policy::default()
}
