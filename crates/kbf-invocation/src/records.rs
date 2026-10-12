//! Every record, built from the event stream: which record a call goes to (section
//! 2.4), which rows an operation's lease events reach (section 3.2), and the memory
//! bound of section 3.5, stage 1.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use kbf_types::{Digest, LeaseId, OperationId};

use crate::event::{
    Call, CallKind, Event, ExecuteAnswer, Finished, InvocationId, LeaseEnd, Outcome, Owner,
    WallTime,
};
use crate::record::{
    ActionRow, Attempt, AttemptEnd, How, Invocation, REASON_CAP, ROW_CAP, RecordKey, State, capped,
};

/// How many dropped calls of records not made yet [`Records`] remembers, so that the
/// record each would have made starts incomplete (see
/// [`Records::mark_incomplete_call`]).
const UNBORN_CAP: usize = 1024;

const HELD: &str = "every key Records hands out names a record it holds";

/// Every invocation record, and the operations their Execute calls wait on.
///
/// [`Records::apply`] is the way in: the records are a function of the events applied,
/// in order, and of the drops marked between them. Nothing here reads a clock or does
/// I/O: "now" is the latest time an event carried.
///
/// **The memory bound** (section 3.5, stage 1): the records' [`Invocation::bytes`]
/// together stay within the bound given to [`Records::with_bound`]. A row is taken
/// only if it fits; when it does not, and whenever the records are over the bound,
/// records are removed in this order:
///
/// 1. minted one-action records (section 2.3), least recently seen first;
/// 2. idle records ([`State::Idle`]), least recently seen first. A record with an
///    operation in flight is never idle, however long that operation runs;
/// 3. if only open records remain, nothing is removed: the largest open record stops
///    taking rows (its actions are still counted, and it says `rows_capped`), and so
///    does the record whose row did not fit.
///
/// Rows already taken still fill in (attempts, outcome, listing) once a record is
/// closed, so the records can pass the bound by what their rows in flight add.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Records {
    row_cap: usize,
    /// The memory bound, in bytes.
    bound: usize,
    /// The records' [`Invocation::bytes`] together, as last counted.
    bytes: usize,
    /// The latest time an event carried.
    now: WallTime,
    invocations: BTreeMap<RecordKey, Invocation>,
    /// Each id's record keys: the owner's first, then each collision in the order seen.
    ids: BTreeMap<InvocationId, Family>,
    operations: BTreeMap<OperationId, Operation>,
    /// Every record by when it was last seen, oldest first.
    by_seen: BTreeSet<(WallTime, RecordKey)>,
    /// The minted records by when they were last seen, oldest first.
    minted_by_seen: BTreeSet<(WallTime, RecordKey)>,
    /// Callers whose dropped call named no record yet: the record their next call
    /// makes starts incomplete. The oldest is forgotten past [`UNBORN_CAP`].
    unborn: VecDeque<(InvocationId, Owner)>,
    /// Records removed for the bound.
    evicted: u64,
}

/// The records of one id that are held.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Family {
    /// Their keys, in the order made.
    keys: Vec<RecordKey>,
    /// The `n` of the next collision key `<id>~<n>`. It only grows while a record of
    /// the id is held, so no two records held at once get one key.
    next: usize,
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
    /// The index of the waiter's row in `inv`, while the row still shows this call: a
    /// later Execute of the same action replaces the row, and this operation's events
    /// no longer reach it.
    fn row(&self, inv: &Invocation) -> Option<usize> {
        let i = self.row?;
        (inv.rows[i].operation.as_deref() == Some(self.name.as_str())).then_some(i)
    }
}

impl Default for Records {
    fn default() -> Self {
        Self::new()
    }
}

impl Records {
    /// No records, each to hold up to [`ROW_CAP`] rows, with no memory bound.
    #[must_use]
    pub const fn new() -> Self {
        Self::with_row_cap(ROW_CAP)
    }

    /// No records, each to hold up to `row_cap` rows, with no memory bound.
    #[must_use]
    pub const fn with_row_cap(row_cap: usize) -> Self {
        Self::with_bound(row_cap, usize::MAX)
    }

    /// No records, each to hold up to `row_cap` rows, all of them together within
    /// `bound` bytes ([`Invocation::bytes`]; see the type's docs).
    #[must_use]
    pub const fn with_bound(row_cap: usize, bound: usize) -> Self {
        Self {
            row_cap,
            bound,
            bytes: 0,
            now: WallTime::from_unix_micros(0),
            invocations: BTreeMap::new(),
            ids: BTreeMap::new(),
            operations: BTreeMap::new(),
            by_seen: BTreeSet::new(),
            minted_by_seen: BTreeSet::new(),
            unborn: VecDeque::new(),
            evicted: 0,
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

    /// Every record's key, in key order.
    pub fn keys(&self) -> impl Iterator<Item = &RecordKey> {
        self.invocations.keys()
    }

    /// The records' [`Invocation::bytes`] together.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    /// The memory bound, in bytes.
    #[must_use]
    pub const fn bound(&self) -> usize {
        self.bound
    }

    /// How many records were removed for the bound.
    #[must_use]
    pub const fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Applies one event, then holds the records to the bound.
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
                        inv.refresh_row(i);
                    }
                    self.recount(&key);
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
        self.shrink();
    }

    /// Marks incomplete the record a dropped call of `id` by `owner` went to: the
    /// one held, or else the one that caller's next call makes.
    pub fn mark_incomplete_call(&mut self, id: &InvocationId, owner: &Owner) {
        match self.find(id, owner) {
            Some(key) => self.record(&key).mark_incomplete(),
            None => {
                if self.unborn.len() == UNBORN_CAP {
                    self.unborn.pop_front();
                }
                self.unborn.push_back((id.clone(), owner.clone()));
            }
        }
    }

    /// Marks incomplete every record that waits on `operation`, whose event was
    /// dropped.
    pub fn mark_incomplete_operation(&mut self, operation: OperationId) {
        let keys: Vec<RecordKey> = self
            .operations
            .get(&operation)
            .map(|op| op.waiters.iter().map(|w| w.key.clone()).collect())
            .unwrap_or_default();
        for key in keys {
            self.record(&key).mark_incomplete();
        }
    }

    /// Marks every record incomplete: an event was dropped and which records it
    /// concerned is not known.
    pub fn mark_all_incomplete(&mut self) {
        for inv in self.invocations.values_mut() {
            inv.mark_incomplete();
        }
    }

    /// The record under `key`, which this holds: every key in `ids`, in `by_seen`
    /// and in an operation's waiters names a record held.
    fn record(&mut self, key: &RecordKey) -> &mut Invocation {
        self.invocations.get_mut(key).expect(HELD)
    }

    /// The key of the record of `id` whose owner is `owner`, if there is one.
    fn find(&self, id: &InvocationId, owner: &Owner) -> Option<RecordKey> {
        self.ids
            .get(id)?
            .keys
            .iter()
            .find(|k| self.invocations[*k].owner == *owner)
            .cloned()
    }

    /// The key of the record `call` goes to, made if it is the first call of its id
    /// from its caller. With a record of the id held, a caller other than its owners
    /// gets `<id>~<n>`, and the owner's record (if held) lists it; with none held, the
    /// caller gets the id itself.
    fn route(&mut self, call: &Call) -> RecordKey {
        if let Some(key) = self.find(&call.id, &call.owner) {
            return key;
        }
        let family = self.ids.entry(call.id.clone()).or_default();
        let key = if family.keys.is_empty() {
            family.next = 2;
            RecordKey::owner(call.id.clone())
        } else {
            family.next += 1;
            RecordKey::collision(call.id.clone(), family.next - 1)
        };
        family.keys.push(key.clone());
        let owner = RecordKey::owner(call.id.clone());
        if key != owner
            && let Some(inv) = self.invocations.get_mut(&owner)
        {
            inv.collisions.push(key.clone());
            self.recount(&owner);
        }
        let mut inv = Invocation::new(key.clone(), call);
        if let Some(i) = self
            .unborn
            .iter()
            .position(|(id, owner)| *id == call.id && *owner == call.owner)
        {
            self.unborn.remove(i);
            inv.mark_incomplete();
        }
        self.by_seen.insert((inv.indexed, key.clone()));
        if inv.minted {
            self.minted_by_seen.insert((inv.indexed, key.clone()));
        }
        self.invocations.insert(key.clone(), inv);
        key
    }

    fn call(&mut self, call: &Call) {
        self.now = self.now.max(call.at);
        let key = self.route(call);
        self.record(&key).seen(call);
        // Indexed under its new `last_seen` before any room is made for its row.
        self.recount(&key);
        let inv = self.record(&key);
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
                    self.add_row(&key, row);
                }
            }
            CallKind::Execute { action, answer } => self.execute(&key, call, *action, answer),
        }
        self.recount(&key);
    }

    /// An Execute call of `key`'s record.
    fn execute(&mut self, key: &RecordKey, call: &Call, action: Digest, answer: &ExecuteAnswer) {
        let inv = self.record(key);
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
        let row = ActionRow::new(action, call, how, Some(name.clone()));
        let index = match inv.row_of(&action) {
            Some(i) => {
                inv.rows[i] = row;
                inv.refresh_row(i);
                Some(i)
            }
            None => self.add_row(key, row),
        };
        let inv = self.invocations.get_mut(key).expect(HELD);
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
                    inv.refresh_row(i);
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
            inv.refresh_row(i);
        }
        op.waiters.push(Waiter {
            key: key.clone(),
            row: index,
            name: name.clone(),
        });
    }

    /// Adds `row` to `key`'s record if it takes rows and the row fits within the
    /// bound (making room as the type's docs say); its index, or `None` with the
    /// record marked `rows_capped`.
    fn add_row(&mut self, key: &RecordKey, row: ActionRow) -> Option<usize> {
        let cap = self.row_cap;
        if !self.record(key).takes_rows(cap) || !self.make_room(key, row.cost()) {
            self.record(key).rows_capped = true;
            return None;
        }
        Some(self.record(key).push_row(row))
    }

    /// Copies `operation`'s attempts into every row that waits on it, and marks each
    /// waiting record seen at `at`.
    fn spread(&mut self, operation: OperationId, at: WallTime) {
        self.now = self.now.max(at);
        let op = &self.operations[&operation];
        let mut keys = Vec::with_capacity(op.waiters.len());
        for w in &op.waiters {
            let inv = self.invocations.get_mut(&w.key).expect(HELD);
            inv.touch(at);
            if let Some(i) = w.row(inv) {
                inv.rows[i].attempts.clone_from(&op.attempts);
                inv.refresh_row(i);
            }
            keys.push(w.key.clone());
        }
        for key in keys {
            self.recount(&key);
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
            if let Some(i) = w.row(inv) {
                let row = &mut inv.rows[i];
                row.outcome = Some(outcome.clone());
                if let Some(listing) = &f.result {
                    row.set_listing(listing.clone());
                }
                inv.refresh_row(i);
            }
            self.recount(&w.key);
        }
    }

    /// Counts `key`'s record again toward the bound, and indexes it under its
    /// `last_seen`.
    fn recount(&mut self, key: &RecordKey) {
        let inv = self.invocations.get_mut(key).expect(HELD);
        let bytes = inv.bytes();
        self.bytes = self.bytes - inv.counted + bytes;
        inv.counted = bytes;
        if inv.indexed != inv.last_seen {
            let (old, new) = ((inv.indexed, key.clone()), (inv.last_seen, key.clone()));
            inv.indexed = inv.last_seen;
            if inv.minted {
                self.minted_by_seen.remove(&old);
                self.minted_by_seen.insert(new.clone());
            }
            self.by_seen.remove(&old);
            self.by_seen.insert(new);
        }
    }

    /// Removes records, in the order of the type's docs, until `cost` more bytes fit
    /// within the bound; never `keep`'s. `false`, with the largest open record closed,
    /// if they do not.
    fn make_room(&mut self, keep: &RecordKey, cost: usize) -> bool {
        while self.bytes.saturating_add(cost) > self.bound {
            match self.victim(Some(keep)) {
                Some(victim) => self.evict(&victim),
                None => {
                    self.close_largest();
                    return false;
                }
            }
        }
        true
    }

    /// Removes records, in the order of the type's docs, while the records are over
    /// the bound; closes the largest open record if that is not enough.
    fn shrink(&mut self) {
        while self.bytes > self.bound {
            match self.victim(None) {
                Some(victim) => self.evict(&victim),
                None => {
                    self.close_largest();
                    return;
                }
            }
        }
    }

    /// The next record to remove for the bound, other than `keep`: the least recently
    /// seen minted record, or else the least recently seen idle one.
    fn victim(&self, keep: Option<&RecordKey>) -> Option<RecordKey> {
        let minted = self.minted_by_seen.iter().map(|(_, k)| k);
        let idle = self
            .by_seen
            .iter()
            .map(|(_, k)| (k, &self.invocations[k]))
            // A record with nothing in flight that is not idle was seen too recently,
            // and so was every record after it.
            .take_while(|(_, inv)| inv.in_flight > 0 || inv.state(self.now) == State::Idle)
            .filter(|(_, inv)| inv.in_flight == 0)
            .map(|(k, _)| k);
        minted.chain(idle).find(|k| Some(*k) != keep).cloned()
    }

    /// Removes `key`'s record, and its calls from the operations they wait on.
    fn evict(&mut self, key: &RecordKey) {
        let inv = self.invocations.remove(key).expect(HELD);
        self.bytes -= inv.counted;
        self.by_seen.remove(&(inv.indexed, key.clone()));
        self.minted_by_seen.remove(&(inv.indexed, key.clone()));
        let family = self.ids.get_mut(key.id()).expect(HELD);
        family.keys.retain(|k| k != key);
        if family.keys.is_empty() {
            self.ids.remove(key.id());
        }
        let owner = RecordKey::owner(key.id().clone());
        if let Some(inv) = self.invocations.get_mut(&owner) {
            inv.collisions.retain(|k| k != key);
            self.recount(&owner);
        }
        if inv.in_flight > 0 {
            for op in self.operations.values_mut() {
                op.waiters.retain(|w| w.key != *key);
            }
            self.operations.retain(|_, op| !op.waiters.is_empty());
        }
        self.evicted += 1;
    }

    /// Stops the largest open record that still takes rows from taking more.
    fn close_largest(&mut self) {
        let largest = self
            .invocations
            .values_mut()
            .filter(|inv| !inv.rows_closed)
            .max_by_key(|inv| inv.counted);
        if let Some(inv) = largest {
            inv.rows_closed = true;
            inv.rows_capped = true;
        }
    }
}

/// `op`'s attempt under `lease`.
fn lease_of(op: &mut Operation, lease: LeaseId) -> Option<&mut Attempt> {
    op.attempts.iter_mut().find(|a| a.lease == lease)
}
