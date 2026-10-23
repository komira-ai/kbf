//! The host and place rows of the `update/*` family, which an older build left in a state
//! store: readable, never written.
//!
//! Earlier builds drove rolling updates from the frontend: one `update/host/<id>` row for
//! each host's phase and one `update/slot` row for the places in use. Updates are OS packages
//! now and nothing writes or reads these rows, but a store written by an older build still
//! holds them, and this build must open it and read what is there. This module is the
//! decoding half of what those builds had, as it was: the same protobuf messages and the same
//! refusals (a number this build does not know is a corrupt row, never a guess). It writes
//! nothing. The decoder can go once every reader of the state store skips a kind it does not
//! know.
//!
//! The rest of the family is still live: `update/desired`, `update/policy` and
//! `update/proposed/<commit>` are read and written by [`crate::update::Rows`], and
//! `update/generation` is its counter. The order counters `update/order/<epoch>` hold no
//! message and are not decoded. The whole family stays fenced (`crate::write_rules`).

use kbf_state::{Store, StoreError};
use prost::Message as _;

/// The prefix every update row has.
pub const PREFIX: &str = "update/";

const SLOT: &str = "update/slot";
const HOSTS: &str = "update/host/";

/// Why the rows could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A row's bytes are not a row this build reads.
    #[error("the row {key} cannot be read: {why}")]
    Corrupt {
        /// The row.
        key: String,
        /// What is wrong with it.
        why: String,
    },
}

/// `update/host/<id>`: where a host was in a run. `phase` is 1 to 14; `reason` is the hold
/// reason (1 to 12) of phase 13 and 0 for every other phase; `source` is 0 (fetch) or 1 (staged).
#[derive(Clone, PartialEq, prost::Message)]
pub struct Host {
    /// The phase.
    #[prost(uint32, tag = "1")]
    pub phase: u32,
    /// The order number the host was given.
    #[prost(uint64, tag = "2")]
    pub order_id: u64,
    /// The generation of the run that started the host.
    #[prost(uint64, tag = "3")]
    pub generation: u64,
    /// When the phase was entered, in milliseconds.
    #[prost(uint64, tag = "4")]
    pub since_ms: u64,
    /// The boot counter at the mark, if one was taken.
    #[prost(uint64, optional, tag = "5")]
    pub boot_before: Option<u64>,
    /// The hold reason of a held host.
    #[prost(uint32, tag = "6")]
    pub reason: u32,
    /// The epoch the order number was drawn under.
    #[prost(uint64, tag = "7")]
    pub order_epoch: u64,
    /// The release the run drove the host to.
    #[prost(string, tag = "8")]
    pub commit: String,
    /// Where that release came from.
    #[prost(uint32, tag = "9")]
    pub source: u32,
}

/// One place held in `update/slot`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Holder {
    /// The host; never empty in a row.
    #[prost(string, tag = "1")]
    pub host: String,
    /// The order number it holds the place under.
    #[prost(uint64, tag = "2")]
    pub order_id: u64,
    /// The epoch of that order number.
    #[prost(uint64, tag = "3")]
    pub epoch: u64,
}

/// `update/slot`: the places in use. No host holds two.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Slot {
    /// The storage place.
    #[prost(message, optional, tag = "1")]
    pub storage: Option<Holder>,
    /// The other places.
    #[prost(message, repeated, tag = "2")]
    pub hosts: Vec<Holder>,
}

/// One update row, decoded.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    /// `update/slot`.
    Slot(Slot),
    /// `update/host/<id>`.
    Host {
        /// The host's name, the rest of the key.
        id: String,
        /// Its row.
        row: Host,
    },
}

impl Slot {
    /// Every place held, storage first.
    pub fn holders(&self) -> impl Iterator<Item = &Holder> {
        self.storage.iter().chain(&self.hosts)
    }
}

/// The source number a row may hold: 0 is a fetch, and is also what a row written before the
/// field existed reads as, 1 is staged.
fn check_source(code: u32) -> Result<(), String> {
    if code <= 1 {
        Ok(())
    } else {
        Err(format!("source {code} is not one this build knows"))
    }
}

fn check_host(host: &Host) -> Result<(), String> {
    match (host.phase, host.reason) {
        (1..=12 | 14, 0) | (13, 1..=12) => {}
        (13, reason) => return Err(format!("hold reason {reason} is not one this build knows")),
        (1..=12 | 14, reason) => {
            return Err(format!(
                "phase {} carries the hold reason {reason}",
                host.phase
            ));
        }
        (phase, _) => return Err(format!("phase {phase} is not one this build knows")),
    }
    check_source(host.source)
}

fn check_slot(slot: &Slot) -> Result<(), String> {
    if slot.holders().any(|holder| holder.host.is_empty()) {
        return Err("a holder has no host".to_owned());
    }
    let mut names: Vec<&str> = slot.holders().map(|holder| holder.host.as_str()).collect();
    let held = names.len();
    names.sort_unstable();
    names.dedup();
    if names.len() == held {
        Ok(())
    } else {
        Err("a host holds two places".to_owned())
    }
}

/// The row `key` holds in `bytes`, or `None` for a key that is not one of the message rows
/// (the counters, and any key a later build adds under `update/`).
///
/// # Errors
/// [`ReadError::Corrupt`]: the bytes are not a message, or name a phase, reason or source this
/// build does not know, or break one of the rules of the row.
pub fn decode(key: &str, bytes: &[u8]) -> Result<Option<Row>, ReadError> {
    let corrupt = |why: String| ReadError::Corrupt {
        key: key.to_owned(),
        why,
    };
    let message = |e: prost::DecodeError| corrupt(e.to_string());
    let row = if key == SLOT {
        let slot = Slot::decode(bytes).map_err(message)?;
        check_slot(&slot).map_err(corrupt)?;
        Row::Slot(slot)
    } else if let Some(id) = key.strip_prefix(HOSTS) {
        let row = Host::decode(bytes).map_err(message)?;
        check_host(&row).map_err(corrupt)?;
        Row::Host {
            id: id.to_owned(),
            row,
        }
    } else {
        return Ok(None);
    };
    Ok(Some(row))
}

/// Every update row `store` holds that has a message, by key. A store written by a build that
/// never kept update rows gives none.
///
/// # Errors
/// [`ReadError`]: the store failed, or a row cannot be read.
pub fn read_all(store: &dyn Store) -> Result<Vec<(String, Row)>, ReadError> {
    let mut rows = Vec::new();
    for (key, stored) in store.scan_prefix(PREFIX)? {
        if let Some(row) = decode(&key, &stored.value)? {
            rows.push((key, row));
        }
    }
    Ok(rows)
}
