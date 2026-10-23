//! What the update row tests share: times and the backing state stores.

#![allow(dead_code)]

use kbf_types::FarmTime;

pub mod harness;
pub mod stores;

/// The farm time `secs` seconds after the epoch.
pub fn t(secs: u64) -> FarmTime {
    FarmTime::from_millis(secs * 1000)
}
