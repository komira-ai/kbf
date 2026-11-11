//! The deny list as the operator reads it: every entry with its reason and time
//! (`GET /v1/denied`), and as the worker listener checks it: the snapshot the newest read of
//! the rows left ([`Members::refresh_denied`]).

use std::sync::{Arc, PoisonError};

use kbf_map::DenyEntry;
use prost::Message as _;
use tokio::time::Instant;

use super::{DENY, DenyRow, Members, MembershipError, denied_entry};

/// One `deny/` row: the entry, why it was added and when (the store's clock, in
/// milliseconds since its origin).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denial {
    /// The entry.
    pub entry: DenyEntry,
    /// The operator's reason.
    pub reason: String,
    /// When it was added.
    pub at_ms: u64,
}

impl Members {
    /// The deny list as the rows give it now, each entry with its reason and time, in the
    /// order [`DenyEntry`] sorts. Not the map's, which lags the rows by up to a refresh and
    /// holds no reasons.
    ///
    /// # Errors
    /// The store failing; [`MembershipError::Corrupt`] for a `deny/` row that is no entry
    /// or whose value does not decode (the list is not guessed at: an entry dropped would
    /// let its caller back in).
    pub fn denials(&self) -> Result<Vec<Denial>, MembershipError> {
        let mut found = std::collections::BTreeMap::new();
        for (key, row) in self.store.scan_prefix(DENY)? {
            let entry = denied_entry(&key)?;
            let value = DenyRow::decode(row.value.as_slice()).map_err(|_| {
                MembershipError::Corrupt(format!("deny row {key:?} does not decode"))
            })?;
            found.insert(entry, value);
        }
        Ok(found
            .into_iter()
            .map(|(entry, row)| Denial {
                entry,
                reason: row.reason,
                at_ms: row.at_ms,
            })
            .collect())
    }
}

/// The deny rows a read found, and when that read began (on the process clock).
pub(crate) type DeniedSnapshot = (Arc<Vec<DenyEntry>>, Instant);

impl Members {
    /// Reads the `deny/` rows and keeps them as the snapshot the worker listener checks
    /// against ([`Members::denied_snapshot`]), and nothing else: one read of the rows, no
    /// write, so it does not wait behind a transaction of this or another frontend (the
    /// map's compare-and-set, the reap) as [`Members::refresh`] can.
    ///
    /// # Errors
    /// As [`Members::denied`]; the earlier snapshot is kept then.
    pub(crate) fn refresh_denied(&self) -> Result<(), MembershipError> {
        let began = Instant::now();
        let rows = self.denied()?;
        self.note_denied(rows, began);
        Ok(())
    }

    /// Keeps `rows`, read by a read that began at `began`, as the snapshot, unless the
    /// snapshot is from a read that began later: two readers that finish out of order
    /// leave the newer read's rows.
    pub(crate) fn note_denied(&self, rows: Vec<DenyEntry>, began: Instant) {
        let mut seen = self
            .denied_seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if seen.as_ref().is_none_or(|(_, at)| *at <= began) {
            *seen = Some((Arc::new(rows), began));
        }
    }
}
