//! The deny list as the operator reads it: every entry with its reason and time, and the
//! check that this frontend can still write (`GET /v1/denied`, `POST /v1/backups/state`).

use kbf_map::DenyEntry;
use kbf_state::Txn;
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

    /// Whether this handle can still write: a transaction that changes nothing, which a
    /// store handle that another writer has fenced out refuses ([`kbf_state::Fenced`]
    /// judges every transaction, an empty one included).
    ///
    /// # Errors
    /// [`MembershipError::Store`], [`kbf_state::StoreError::Fenced`] among them, when it
    /// cannot.
    pub fn check_writer(&self) -> Result<(), MembershipError> {
        self.store
            .txn(Txn::new())?
            .map(drop)
            .map_err(|_| MembershipError::Contended)
    }
}
