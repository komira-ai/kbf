//! Finding an operation by its name: the `doneof/<operation>` rows, and the look that
//! answers a client which holds only an operation's REAPI name (`operations/<id>`).
//!
//! The `op/<operation>` row holds the instance name and action of an operation while it is
//! unfinished, and the `done/<operation>` row holds only how it ended: an `ExecuteResponse`
//! names neither. A client that asks for a finished operation by name must be authorized
//! against the instance the operation ran under, and shown the action it ran, so the
//! transaction that finishes an operation also puts a `doneof/<operation>` row beside its
//! `done/` row, with the same lifetime. The value is a version byte (1), the action's size
//! (8 bytes, big-endian), its SHA-256 hash (32 bytes) and the instance name (the rest, UTF-8).
//! The row is a row of its own, so no row of an earlier format changes and a binary that does
//! not know the prefix reads the store as it always did. A result written by such a binary has
//! no `doneof/` row, and its operation is not found by name once the `op/` row is gone.

use kbf_types::{ActionKey, DigestFunction};

use super::*;

pub(super) const DONE_OF: &str = "doneof/";

/// The version byte of a `doneof/` row.
const VERSION: u8 = 1;

pub(crate) fn doneof_key(operation: OperationId) -> String {
    format!("{DONE_OF}{:016x}", operation.0)
}

/// `key` as the value of a `doneof/` row.
pub(crate) fn encode(key: &ActionKey) -> Vec<u8> {
    let mut row = vec![VERSION];
    row.extend(key.action.size_bytes.to_be_bytes());
    row.extend(key.action.hash);
    row.extend(key.instance.as_bytes());
    row
}

/// The action a `doneof/` row holds, or what is wrong with the row.
pub(crate) fn decode(row: &[u8]) -> Result<ActionKey, &'static str> {
    let [VERSION, rest @ ..] = row else {
        return Err("not a version 1 action row");
    };
    let cut = "an action row is cut short";
    let (size, rest) = rest.split_first_chunk::<8>().ok_or(cut)?;
    let (hash, instance) = rest.split_first_chunk::<32>().ok_or(cut)?;
    let text = std::str::from_utf8(instance);
    let instance = text.map_err(|_| "an instance name is not UTF-8")?;
    Ok(ActionKey {
        instance: instance.to_owned(),
        action: Digest::new(DigestFunction::Sha256, *hash, u64::from_be_bytes(*size)),
    })
}

impl Durable {
    /// `txn`, and the put of the `doneof/` row of `operation`, which `ending` is about to
    /// finish: the action its `op/` row holds, kept as long as the result. An operation with
    /// no `op/` row adds nothing, since the request is not known.
    ///
    /// # Errors
    /// The store cannot be read, or the `op/` row is not a request.
    pub(super) fn noting(
        &self,
        txn: Txn,
        operation: OperationId,
        ending: &Ending,
    ) -> Result<Txn, StateError> {
        let key = op_key(operation);
        let Some(row) = self.store.get(&key)? else {
            return Ok(txn);
        };
        let request = self.request_row(&key, &row)?;
        let action = action_of_request(&key, &request)?;
        Ok(self.ext.ended(txn, operation, &action, ending.ttl()))
    }

    /// The action `operation` runs or ran, and where it stands, for a client that asks for it
    /// by name. `None` if the store has no trace of the operation (it never existed, or its
    /// result is past its retention), and for a result with no `doneof/` row, which the store
    /// cannot say the instance of.
    ///
    /// The action is read first: an operation finishes in one transaction that removes its
    /// `op/` row and puts its `doneof/` row, so one of the two is there from submit until the
    /// result expires, and the look of where it stands that follows can only find it further
    /// along.
    ///
    /// # Errors
    /// The store cannot be read, or a row read is not one the scheduler wrote.
    pub(in crate::farm) fn named_operation(
        &self,
        operation: OperationId,
    ) -> Result<Option<(ActionKey, Progress)>, StateError> {
        let Some(key) = self.action_of(operation)? else {
            return Ok(None);
        };
        let progress = self.progress(operation)?;
        Ok((!progress.is_gone()).then_some((key, progress)))
    }

    /// The action of `operation`, from its `op/` row while it is unfinished and from its
    /// `doneof/` row after.
    fn action_of(&self, operation: OperationId) -> Result<Option<ActionKey>, StateError> {
        let unfinished = op_key(operation);
        if let Some(row) = self.store.get(&unfinished)? {
            let request = self.request_row(&unfinished, &row)?;
            return action_of_request(&unfinished, &request).map(Some);
        }
        self.ext.action_of(&*self.store, operation)
    }
}
