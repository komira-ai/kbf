//! The `inflight/<instance>/<action>` row: which operation runs an action now, so a
//! request for an action already queued attaches to that operation, whichever process
//! queued it and whether or not this one saw the write land.
//!
//! The key is the request's dedup key ([`ActionKey`]), the instance name in hexadecimal
//! (the name may hold any character, a `/` too), `/`, and the action's digest as
//! `<hash>/<size>`; the value is the operation's id, 8 bytes big-endian. It is written
//! in the transaction that queues the operation, put only if absent, and removed in the
//! one that finishes it ([`Durable::unpin`]). A request that is not joinable
//! ([`Request::joinable`]) has no row: its runs are not interchangeable.
//!
//! The key is the action, not the call, because that is what the scheduler joins on
//! already; a caller's retry after a cut stream or a lost acknowledgement names the same
//! action and so finds the same row, with no request id to carry through the front.

use kbf_types::ActionKey;

use super::*;

const INFLIGHT: &str = "inflight/";

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

fn inflight_key(key: &ActionKey) -> String {
    let instance: String = key
        .instance
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{INFLIGHT}{instance}/{}", key.action)
}

impl Durable {
    /// Queues `request` as operation `id`, unless the store holds an operation that runs
    /// the action: then that one is returned, to join. Safe to repeat: a write whose
    /// outcome the caller never learned has put the row, and the repeat finds the
    /// operation it wrote.
    ///
    /// If the store's counter is ahead of `id` (another writer took the id), the next
    /// free one is used; [`Submitted::Queued`] says which.
    ///
    /// # Errors
    /// The store cannot be read or written, or a row in the way does not go
    /// ([`StateError::Conflict`]).
    pub(super) fn submit(
        &self,
        mut id: OperationId,
        request: &Request,
        subject: &Subject,
    ) -> Result<Submitted, StateError> {
        if !request.joinable() {
            self.submitted(id, request, subject)?;
            return Ok(Submitted::Queued(id));
        }
        let pin = inflight_key(&request.key);
        let mut taken = Err(StateError::Missing("an attempt".to_owned()));
        for _ in 0..LOOKS {
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
            taken = self.submitted_pinned(id, request, subject, (&pin, cond));
            match taken {
                Ok(()) => return Ok(Submitted::Queued(id)),
                // Another writer took the id, or queued this action since the look.
                Err(StateError::Conflict(_)) => {
                    id = OperationId(self.counter(NEXT_OP)?.max(id.0));
                }
                Err(_) => break,
            }
        }
        taken.map(|()| Submitted::Queued(id))
    }

    /// The operation `operation` as the store holds it, or `None` if it has no `op/` row.
    fn stored_operation(
        &self,
        operation: OperationId,
    ) -> Result<Option<StoredOperation>, StateError> {
        let key = op_key(operation);
        let Some(row) = self.store.get(&key)? else {
            return Ok(None);
        };
        let request = request_row(&key, &row)?;
        let mut lease = None;
        for (key, row) in self.store.scan_prefix("lease/")? {
            if let FarmInput::Committed(ControlRecord::Lease(grant)) = decode(&key, &row.value)?
                && grant.operation == operation
            {
                lease = Some(grant);
            }
        }
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
        let key = subject_key(operation);
        let subject = match self.store.get(&key)? {
            Some(row) => Some(subject::decode(&row.value).map_err(|why| corrupt(&key, why))?),
            None => None,
        };
        Ok(Some(StoredOperation {
            operation,
            request,
            lease,
            losses,
            fate,
            subject,
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
        let request = request_row(&key, &row)?;
        let pin = inflight_key(&request.key);
        Ok(match self.store.get(&pin)? {
            Some(row) if number(&pin, &row)? == operation.0 => {
                txn.delete(pin, Cond::Version(row.version))
            }
            _ => txn,
        })
    }
}
