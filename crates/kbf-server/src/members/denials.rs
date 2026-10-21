//! The deny list as the operator reads it: every entry with its reason and time
//! (`GET /v1/denied`).

use kbf_map::DenyEntry;
use prost::Message as _;

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
