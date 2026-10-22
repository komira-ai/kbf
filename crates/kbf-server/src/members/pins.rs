//! The pins: the maps the last map bumps replaced, kept in the state store for the dwell.
//!
//! `handoff/pin/<generation, 20 digits>` holds the map that the bump to the next generation
//! replaced, written in the same transaction as the bump and lapsing after the dwell. At
//! most [`RETAINED_MAPS`] are kept: a bump deletes the pin that falls out of the cap. A
//! frontend that restarted, or that another frontend's bumps skipped past, reads them to
//! have the maps a blob written before a change is found by ([`Members::view`]). The cap is
//! also the bound on handoffs: a join or a drain that would be one too many is refused.

use kbf_map::Map;
use kbf_mapsync::RETAINED_MAPS;

use super::{Members, MembershipError, decode_map};

/// The prefix of the pins.
pub(super) const PINS: &str = "handoff/pin/";

pub(super) fn pin_key(generation: u64) -> String {
    format!("{PINS}{generation:020}")
}

impl Members {
    /// The pins the store holds, decoded: the maps the last bumps replaced, for as long as
    /// the dwell keeps them. Expired pins are not read.
    pub(super) fn pins(&self) -> Result<Vec<Map>, MembershipError> {
        self.store
            .scan_prefix(PINS)?
            .iter()
            .map(|(_, stored)| decode_map(stored))
            .collect()
    }

    /// [`MembershipError::PinsFull`] when the pins are at the cap: a join or a drain now would
    /// be a handoff too many.
    pub(super) fn pins_full(&self) -> Result<(), MembershipError> {
        let held = self.store.scan_prefix(PINS)?.len();
        if held >= RETAINED_MAPS {
            return Err(MembershipError::PinsFull {
                held,
                cap: RETAINED_MAPS,
            });
        }
        Ok(())
    }
}
