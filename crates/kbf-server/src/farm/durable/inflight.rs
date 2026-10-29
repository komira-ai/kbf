//! The `inflight/<instance>/<action>` row: which operation runs an action now, so a
//! request for an action already queued attaches to that operation, whichever process
//! queued it and whether or not this one saw the write land.
//!
//! The key is the request's dedup key ([`WorkKey`]), the instance name in hexadecimal
//! (the name may hold any character, a `/` too), `/`, and the action's digest as
//! `<hash>/<size>` (the key's dedup text); the value is the operation's id, 8 bytes big-endian. It is written
//! in the transaction that queues the operation, put only if absent, and removed in the
//! one that finishes it ([`Durable::unpin`]). A request that is not joinable
//! ([`Request::joinable`]) has no row: its runs are not interchangeable.
//!
//! The key is the action, not the call, because that is what the scheduler joins on
//! already; a caller's retry after a cut stream or a lost acknowledgement names the same
//! action and so finds the same row, with no request id to carry through the front.

use kbf_types::WorkKey;

use super::*;

pub(super) const INFLIGHT: &str = "inflight/";

/// How many times a submit looks again after a write finds a row in its way.
const LOOKS: usize = 3;

/// What a submit did.
#[derive(Debug)]
pub(super) enum Submitted {
    /// The operation is written, under this id.
    Queued(OperationId),
    /// The store holds an operation that runs the action: the request joins it.
    Joined(Box<StoredOperation>),
}

fn inflight_key(key: &WorkKey) -> String {
    let instance: String = key
        .class
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{INFLIGHT}{instance}/{}", key.dedup)
}

impl Durable {
    /// Queues `request` as operation `id`, unless the store holds an operation that runs
    /// the action: then that one is returned, to join. Safe to repeat: a write whose
    /// outcome the caller never learned has put the row, and the repeat finds the
    /// operation it wrote.
    ///
    /// `id` comes from the block this handle reserved ([`Durable::reserve_operation`]),
    /// where no other frontend numbers. If a row of an earlier writer is in the way of it,
    /// the next id of the block is used; [`Submitted::Queued`] says which.
    ///
    /// # Errors
    /// The store cannot be read or written, or a row in the way does not go
    /// ([`StateError::Conflict`]).
    #[cfg(test)]
    pub(super) fn submit(
        &self,
        id: OperationId,
        request: &Request,
        subject: &Subject,
    ) -> Result<Submitted, StateError> {
        self.submit_keyed(id, request, subject, None, None)
    }

    /// [`Durable::submit`], with the size key and the longest wall time the operation is
    /// given, if any: written in an `opkey/` and an `opwall/` row of the transaction that
    /// queues it.
    pub(super) fn submit_keyed(
        &self,
        mut id: OperationId,
        request: &Request,
        subject: &Subject,
        size_key: Option<u64>,
        max_wall: Option<Duration>,
    ) -> Result<Submitted, StateError> {
        if !request.joinable() {
            self.submitted_keyed(id, request, subject, size_key, max_wall)?;
            return Ok(Submitted::Queued(id));
        }
        let pin = inflight_key(&request.key);
        let mut looks = 0;
        loop {
            let mut cond = Cond::Absent;
            if let Some(row) = self.store.get(&pin)? {
                let held = OperationId(number(&pin, &row)?);
                if let Some(op) = self.stored_operation(held)? {
                    if op.request.key != request.key {
                        return Err(corrupt(&pin, "names an operation of another action"));
                    }
                    return Ok(Submitted::Joined(Box::new(op)));
                }
                // The operation it names is gone: the row is left over, and goes.
                cond = Cond::Version(row.version);
            }
            match self.submitted_pinned(id, request, subject, size_key, max_wall, (&pin, cond)) {
                Ok(()) => return Ok(Submitted::Queued(id)),
                // Another writer queued this action since the look, or a row of an earlier
                // writer is in the way of the id: look again, and try the next id of the
                // block.
                Err(StateError::Conflict(_)) if looks < LOOKS => {
                    looks += 1;
                    id = self.reserve_operation(OperationId(id.0 + 1))?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// The operation `operation` as the store holds it, or `None` if it has no `op/` row.
    /// Its lease is found through its `lease-by-op/` row ([`Durable::lease_of`]).
    fn stored_operation(
        &self,
        operation: OperationId,
    ) -> Result<Option<StoredOperation>, StateError> {
        self.stored_with(operation, None)
    }

    /// [`Durable::stored_operation`], given the lease the caller has read as the
    /// operation's (`None` leaves the lookup to this call).
    pub(super) fn stored_with(
        &self,
        operation: OperationId,
        known: Option<LeaseGrant>,
    ) -> Result<Option<StoredOperation>, StateError> {
        let key = op_key(operation);
        let Some(row) = self.store.get(&key)? else {
            return Ok(None);
        };
        let request = self.request_row(&key, &row)?;
        let action = action_of_request(&key, &request)?;
        let lease = match known {
            Some(grant) => Some(grant),
            None => self.lease_of(operation)?,
        };
        let losses = match self.store.get(&loss_key(operation))? {
            Some(row) => u32::try_from(number("loss", &row)?)
                .map_err(|_| corrupt(&loss_key(operation), "not a loss count"))?,
            None => 0,
        };
        let key = fate_key(operation);
        let fate = match self.store.get(&key)? {
            Some(row) => Some(fate::decode(&row.value).map_err(|why| corrupt(&key, why))?),
            None => None,
        };
        let subject = self.ext.subject(&*self.store, operation)?;
        let read = self.read_size_keys.load(Ordering::SeqCst);
        let size_key = match self.store.get(&opkey_key(operation))?.filter(|_| read) {
            Some(row) => {
                let key = opkey_key(operation);
                Some(opkey::decode(&row.value).map_err(|why| corrupt(&key, why))?)
            }
            None => None,
        };
        let key = opwall_key(operation);
        let max_wall = match self.store.get(&key)? {
            Some(row) => Some(opwall::decode(&row.value).map_err(|why| corrupt(&key, why))?),
            None => None,
        };
        Ok(Some(StoredOperation {
            operation,
            request,
            action,
            lease,
            losses,
            fate,
            subject,
            size_key,
            max_wall,
        }))
    }

    /// `txn`, and the removal of `operation`'s in-flight row if it names `operation`: the
    /// finish that removes the operation's rows takes its pin with it. A row that names
    /// another operation (this one's row was removed and a later operation of the same
    /// action took the key) stays.
    pub(super) fn unpin(&self, txn: Txn, operation: OperationId) -> Result<Txn, StateError> {
        let key = op_key(operation);
        let Some(row) = self.store.get(&key)? else {
            return Ok(txn);
        };
        let request = self.request_row(&key, &row)?;
        let pin = inflight_key(&request.key);
        Ok(match self.store.get(&pin)? {
            Some(row) if number(&pin, &row)? == operation.0 => {
                txn.delete(pin, Cond::Version(row.version))
            }
            _ => txn,
        })
    }
}
