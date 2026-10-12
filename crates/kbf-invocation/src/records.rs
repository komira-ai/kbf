//! Every record, built from the event stream: which record a call goes to (section
//! 2.4), and which rows an operation's lease events reach (section 3.2).

use std::collections::BTreeMap;

use kbf_types::{LeaseId, OperationId};

use crate::event::{
    Call, CallKind, Event, ExecuteAnswer, Finished, InvocationId, LeaseEnd, Outcome, Owner,
    WallTime,
};
use crate::record::{
    ActionRow, Attempt, AttemptEnd, How, Invocation, REASON_CAP, ROW_CAP, RecordKey, capped,
};

/// Every invocation record, and the operations their Execute calls wait on.
///
/// [`Records::apply`] is the only way in: the records are a function of the events
/// applied, in order. Nothing here reads a clock or does I/O, and nothing is ever
/// removed; the memory bound and eviction of section 3.5 are the recorder's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Records {
    row_cap: usize,
    invocations: BTreeMap<RecordKey, Invocation>,
    /// Each id's record keys: the owner's first, then each collision in the order seen.
    ids: BTreeMap<InvocationId, Vec<RecordKey>>,
    operations: BTreeMap<OperationId, Operation>,
}

/// An operation some record waits on.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Operation {
    /// When it was last queued: submitted, or requeued.
    queued: WallTime,
    /// Its leases, in grant order.
    attempts: Vec<Attempt>,
    /// The calls waiting on it: each one's record, its row (`None` past the row cap)
    /// and the REAPI operation name it was given.
    waiters: Vec<Waiter>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Waiter {
    key: RecordKey,
    row: Option<usize>,
    name: String,
}

impl Waiter {
    /// The waiter's row in `inv`, while the row still shows this call: a later Execute
    /// of the same action replaces the row, and this operation's events no longer
    /// reach it.
    fn row<'a>(&self, inv: &'a mut Invocation) -> Option<&'a mut ActionRow> {
        let row = &mut inv.rows[self.row?];
        (row.operation.as_deref() == Some(self.name.as_str())).then_some(row)
    }
}

impl Default for Records {
    fn default() -> Self {
        Self::new()
    }
}

impl Records {
    /// No records, each to hold up to [`ROW_CAP`] rows.
    #[must_use]
    pub const fn new() -> Self {
        Self::with_row_cap(ROW_CAP)
    }

    /// No records, each to hold up to `row_cap` rows.
    #[must_use]
    pub const fn with_row_cap(row_cap: usize) -> Self {
        Self {
            row_cap,
            invocations: BTreeMap::new(),
            ids: BTreeMap::new(),
            operations: BTreeMap::new(),
        }
    }

    /// The record under `key`.
    #[must_use]
    pub fn get(&self, key: &RecordKey) -> Option<&Invocation> {
        self.invocations.get(key)
    }

    /// The record under `key`, to mark it incomplete.
    pub fn get_mut(&mut self, key: &RecordKey) -> Option<&mut Invocation> {
        self.invocations.get_mut(key)
    }

    /// Applies one event.
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Call(call) => self.call(&call),
            Event::Command {
                id,
                owner,
                action,
                command,
            } => {
                if let Some(key) = self.find(&id, &owner) {
                    let inv = self.record(&key);
                    if let Some(i) = inv.row_of(&action) {
                        inv.rows[i].command = Some(command);
                    }
                }
            }
            Event::Granted {
                operation,
                lease,
                worker,
                at,
            } => {
                if let Some(op) = self.operations.get_mut(&operation) {
                    op.attempts.push(Attempt {
                        lease,
                        worker,
                        queued: op.queued,
                        granted: at,
                        started: None,
                        ended: None,
                        end: None,
                    });
                    self.spread(operation, at);
                }
            }
            Event::Started {
                operation,
                lease,
                at,
            } => {
                if let Some(op) = self.operations.get_mut(&operation) {
                    if let Some(a) = lease_of(op, lease) {
                        a.started = Some(at);
                    }
                    self.spread(operation, at);
                }
            }
            Event::Requeued {
                operation,
                lease,
                reason,
                at,
            } => {
                if let Some(op) = self.operations.get_mut(&operation) {
                    if let Some(a) = lease_of(op, lease) {
                        a.ended = Some(at);
                        a.end = Some(AttemptEnd::Requeued(reason));
                    }
                    op.queued = at;
                    self.spread(operation, at);
                }
            }
            Event::Finished(finished) => self.finish(finished),
        }
    }

    /// The record under `key`, which this holds: every key in `ids` and in an
    /// operation's waiters names a record, and none is removed.
    fn record(&mut self, key: &RecordKey) -> &mut Invocation {
        self.invocations
            .get_mut(key)
            .expect("every key Records hands out names a record it holds")
    }

    /// The key of the record of `id` whose owner is `owner`, if there is one.
    fn find(&self, id: &InvocationId, owner: &Owner) -> Option<RecordKey> {
        self.ids
            .get(id)?
            .iter()
            .find(|k| self.invocations[*k].owner == *owner)
            .cloned()
    }

    /// The key of the record `call` goes to, made if it is the first call of its id
    /// from its caller. A caller other than the owner gets `<id>~<n>`, and the owner's
    /// record lists it.
    fn route(&mut self, call: &Call) -> RecordKey {
        if let Some(key) = self.find(&call.id, &call.owner) {
            return key;
        }
        let keys = self.ids.entry(call.id.clone()).or_default();
        let owner = keys.first().cloned();
        let key = match owner {
            None => RecordKey::owner(call.id.clone()),
            Some(_) => RecordKey::collision(call.id.clone(), keys.len() + 1),
        };
        keys.push(key.clone());
        if let Some(owner) = owner {
            self.record(&owner).collisions.push(key.clone());
        }
        self.invocations
            .insert(key.clone(), Invocation::new(key.clone(), call));
        key
    }

    fn call(&mut self, call: &Call) {
        let key = self.route(call);
        let cap = self.row_cap;
        let inv = self
            .invocations
            .get_mut(&key)
            .expect("route returns the key of a record it holds");
        inv.seen(call);
        match &call.kind {
            CallKind::Uploaded(n) => inv.counts.bytes_uploaded += n,
            CallKind::Downloaded(n) => inv.counts.bytes_downloaded += n,
            CallKind::FindMissingBlobs { asked, missing } => {
                inv.counts.blobs_asked += asked;
                inv.counts.blobs_missing += missing;
            }
            CallKind::GetActionResult { hit: None, .. } => inv.counts.ac_miss += 1,
            CallKind::GetActionResult {
                action,
                hit: Some(listing),
            } => {
                inv.counts.ac_hit += 1;
                // A hit adds a row only for an action the record has not seen.
                if inv.row_of(action).is_none() {
                    let mut row = ActionRow::new(*action, call, How::CacheHit, None);
                    row.outcome = Some(Outcome::Ok {
                        exit_code: listing.exit_code,
                    });
                    row.set_listing(listing.clone());
                    inv.push_row(cap, row);
                }
            }
            CallKind::Execute { action, answer } => {
                inv.counts.execute += 1;
                let (how, name, operation) = match answer {
                    ExecuteAnswer::Cached { name, result } => {
                        inv.counts.cache_hit_execute += 1;
                        (How::CacheHitExecute, name, Err(result))
                    }
                    ExecuteAnswer::Joined { name, operation } => {
                        inv.counts.joined += 1;
                        (How::Joined, name, Ok(*operation))
                    }
                    ExecuteAnswer::Queued { name, operation } => {
                        inv.counts.ran += 1;
                        (How::Ran, name, Ok(*operation))
                    }
                };
                // A row shows the last Execute of its action: a later one replaces it.
                let row = ActionRow::new(*action, call, how, Some(name.clone()));
                let index = match inv.row_of(action) {
                    Some(i) => {
                        inv.rows[i] = row;
                        Some(i)
                    }
                    None => inv.push_row(cap, row),
                };
                // A cached answer is the action's end; any other waits on an operation.
                let operation = match operation {
                    Ok(operation) => operation,
                    Err(result) => {
                        inv.counts.failed += u64::from(result.exit_code != 0);
                        if let Some(i) = index {
                            let row = &mut inv.rows[i];
                            row.outcome = Some(Outcome::Ok {
                                exit_code: result.exit_code,
                            });
                            row.set_listing((**result).clone());
                        }
                        return;
                    }
                };
                inv.in_flight += 1;
                let op = self
                    .operations
                    .entry(operation)
                    .or_insert_with(|| Operation {
                        queued: call.at,
                        attempts: Vec::new(),
                        waiters: Vec::new(),
                    });
                // A joined action starts with its twin's attempts so far.
                if let Some(i) = index {
                    inv.rows[i].attempts.clone_from(&op.attempts);
                }
                op.waiters.push(Waiter {
                    key,
                    row: index,
                    name: name.clone(),
                });
            }
        }
    }

    /// Copies `operation`'s attempts into every row that waits on it, and marks each
    /// waiting record seen at `at`.
    fn spread(&mut self, operation: OperationId, at: WallTime) {
        let op = &self.operations[&operation];
        for w in &op.waiters {
            let inv = self
                .invocations
                .get_mut(&w.key)
                .expect("a waiter's record is held");
            inv.touch(at);
            if let Some(row) = w.row(inv) {
                row.attempts.clone_from(&op.attempts);
            }
        }
    }

    fn finish(&mut self, f: Finished) {
        let Some(op) = self.operations.get_mut(&f.operation) else {
            return;
        };
        if let Some((lease, end)) = &f.lease
            && let Some(a) = lease_of(op, *lease)
        {
            a.ended = Some(f.at);
            a.end = Some(match end {
                LeaseEnd::Accepted => AttemptEnd::Accepted,
                LeaseEnd::Failed(failure, reason) => {
                    AttemptEnd::Failed(*failure, capped(reason, REASON_CAP))
                }
            });
        }
        self.spread(f.operation, f.at);
        let op = self.operations.remove(&f.operation).expect("checked above");
        let outcome = match f.outcome {
            Outcome::Failed { code, message } => Outcome::Failed {
                code,
                message: capped(&message, REASON_CAP),
            },
            Outcome::Refused(reason) => Outcome::Refused(capped(&reason, REASON_CAP)),
            other => other,
        };
        for w in op.waiters {
            let inv = self.record(&w.key);
            inv.in_flight -= 1;
            inv.counts.failed += u64::from(outcome.is_failure());
            if let Some(row) = w.row(inv) {
                row.outcome = Some(outcome.clone());
                if let Some(listing) = &f.result {
                    row.set_listing(listing.clone());
                }
            }
        }
    }
}

/// `op`'s attempt under `lease`.
fn lease_of(op: &mut Operation, lease: LeaseId) -> Option<&mut Attempt> {
    op.attempts.iter_mut().find(|a| a.lease == lease)
}
