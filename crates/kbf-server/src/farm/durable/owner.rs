//! Which frontend owns a daemon, when several frontends share one state store.
//!
//! A daemon is connected to one frontend at a time, and only that frontend may write the
//! leases of the daemon's node: grant one, give one up, take a result for one. Two
//! frontends that both believed they were its owner could each grant the node an
//! operation, or one could give up a lease the daemon still runs under the other.
//!
//! The `owner/<worker>` row says who owns a node: the id of a frontend and a counter, eight
//! bytes each, big-endian. A daemon's `Hello` makes the frontend it says it to the owner,
//! by a compare-and-set that adds one to the counter. The frontend remembers the counter
//! it wrote, and every write of a lease of that node (a grant, a lease given up, a result)
//! reads the row and is conditional on it: it names this frontend at that counter, in the
//! very transaction. A frontend that finds another name or counter, because the daemon
//! said `Hello` elsewhere meanwhile, writes nothing and fails with
//! [`StateError::StaleOwner`]. That error is about one node: it drops what the frontend
//! holds for that node and nothing else, and does not mark the frontend as superseded as
//! a refusal of the store as `Fenced` does ([`kbf_state::StoreError::Fenced`]).
//!
//! A node this frontend never claimed (a lease taken up by a request that joined its
//! operation, before its daemon said `Hello` here) has no counter to check: a write for it
//! is conditional on its lease rows alone. A node whose claim failed after a `Hello` (the
//! store could not be read or written, or another frontend won every try) is lost like one
//! the daemon left: nothing is written for it until a claim succeeds.
//!
//! The frontend also acknowledges a node's heartbeats only while it owns the node, looking
//! at the row again when the last look is [`OWNER_CACHE_TTL`] old, and only while it has
//! written the node's `session/` row within [`ACK_WINDOW`]. A daemon stops its work `T`
//! after the last heartbeat that was acknowledged, so a frontend that cannot tell whether
//! the daemon still belongs to it, or whether the other frontends can see that the daemon
//! is alive, stops acknowledging.

use kbf_sched::fence::{HANDOVER_GRACE, LEADER_LEASE_MARGIN, SELF_FENCE};

use super::blocks::contended;
use super::*;
use crate::config::MAX_HEARTBEAT_INTERVAL_MS;

pub(super) const OWNER: &str = "owner/";

/// How long a frontend trusts its last look at an `owner/` row before it reads the row
/// again to acknowledge a heartbeat. The frontend a daemon moved to waits this long beyond
/// the handover grace ([`OWNER_HANDOVER_WAIT`]) before it takes a lease the daemon does not
/// list as lost, because the frontend the daemon left may go on acknowledging the old
/// daemon process this long after the move.
pub const OWNER_CACHE_TTL: Duration = Duration::from_secs(2);

/// How long the leases a daemon that moved to this frontend does not list are kept after it
/// registers, when the daemon process may be another than the one that ran them: the
/// handover grace, which ends when the old process has fenced, and [`OWNER_CACHE_TTL`].
pub(super) const OWNER_HANDOVER_WAIT: Duration = HANDOVER_GRACE.saturating_add(OWNER_CACHE_TTL);

/// How old this frontend's last write of a daemon's `session/` row may be for it still to
/// acknowledge the daemon's heartbeats.
///
/// A daemon's work ends `T` ([`SELF_FENCE`]) after the last heartbeat that was acknowledged,
/// and the fence keeps a margin ([`LEADER_LEASE_MARGIN`]) beyond that. The row is there for
/// [`LEASE_GRACE`] after its last write, and a reader that takes a missing row to mean the
/// daemon is gone would wait one renewal interval ([`SESSION_RENEWAL`]) beyond that. The
/// window is what is left of the two after the work and twice the margin: a derived
/// constant, which the orphan sweep relies on ([`ORPHAN_WAIT`]). It is longer than a
/// renewal interval and a heartbeat interval together, checked at compile time, so a
/// frontend that renews its rows on schedule never withholds an acknowledgement.
pub const ACK_WINDOW: Duration = ROW_LIFE.saturating_sub(RUN_LIFE);

/// How long after its last write a daemon's session row, written on schedule, is certain
/// to be there to be read, with the wait beyond its expiry.
pub(super) const ROW_LIFE: Duration = LEASE_GRACE.saturating_add(SESSION_RENEWAL);

/// How long after its last acknowledged heartbeat a daemon may run, with twice the margin
/// of the fence.
pub(super) const RUN_LIFE: Duration =
    SELF_FENCE.saturating_add(LEADER_LEASE_MARGIN.saturating_mul(2));

/// The longest heartbeat interval a server is started with.
const HEARTBEAT_MAX: Duration = Duration::from_millis(MAX_HEARTBEAT_INTERVAL_MS);

// A frontend that renews a session row on schedule acknowledges every heartbeat: the
// renewal comes due at the first heartbeat after `SESSION_RENEWAL`, which comes at most one
// heartbeat interval later.
const _: () =
    assert!(SESSION_RENEWAL.as_millis() + HEARTBEAT_MAX.as_millis() < ACK_WINDOW.as_millis());

/// The two numbers of a 16-byte row, big-endian; `None` for a row of another length.
pub(super) fn pair_of(row: &[u8]) -> Option<(u64, u64)> {
    let packed = <[u8; 16]>::try_from(row).map(u128::from_be_bytes).ok()?;
    let high = u64::try_from(packed >> 64).unwrap_or_default();
    let low = u64::try_from(packed & u128::from(u64::MAX)).unwrap_or_default();
    Some((high, low))
}

/// The 16-byte row of two numbers, big-endian.
pub(super) fn pair_row(high: u64, low: u64) -> [u8; 16] {
    ((u128::from(high) << 64) | u128::from(low)).to_be_bytes()
}

pub(super) fn owner_key(worker: &WorkerId) -> String {
    format!("{OWNER}{worker}")
}

/// What an `owner/<worker>` row says: the frontend that owns the node, and how many times
/// a daemon has said `Hello` to a frontend as the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Owner {
    pub(super) frontend: u64,
    pub(super) counter: u64,
}

impl Owner {
    fn from_row(key: &str, row: &Versioned) -> Result<Self, StateError> {
        let Some((frontend, counter)) = pair_of(&row.value) else {
            return Err(corrupt(key, "not a 16-byte owner"));
        };
        Ok(Self { frontend, counter })
    }
}

/// What a frontend holds of the ownership of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ownership {
    /// The frontend wrote the row at `counter`, and last read it at `checked`.
    Held { counter: u64, checked: FarmTime },
    /// The row names another frontend or another counter, or the frontend could not make
    /// it name itself: until the node says `Hello` here again, nothing is written for it.
    Lost,
}

impl Ownership {
    const fn held(counter: u64, checked: FarmTime) -> Self {
        Self::Held { counter, checked }
    }
}

/// What a `Hello` finds in the store, as the frontend that made itself the node's owner
/// reads it.
#[derive(Debug)]
pub(super) struct Arrival {
    /// This frontend's id, as it wrote it in the row.
    pub(super) frontend: u64,
    /// The owner the row named before, if it named one.
    pub(super) previous: Option<Owner>,
    /// The unfinished operations whose lease rows name the node.
    pub(super) leases: Vec<StoredOperation>,
    /// The daemon process the node's `session/` row names: the one that said `Hello` to the
    /// frontend that owned the node, as far as the row has not expired.
    pub(super) session: Option<DaemonInstance>,
}

impl Durable {
    /// A daemon said `Hello` as `worker` to this frontend: it becomes the node's owner at
    /// the next counter, and reads what it takes over ([`Arrival`]). The write is retried
    /// when another frontend writes the row first, and if every try loses, the node is
    /// given up to the other (nothing is written for it until it says `Hello` here again).
    ///
    /// A failure to read what the claim takes over leaves the node lost to this frontend
    /// too, with the owner the claim replaced kept for the retry, whose claim finds this
    /// frontend's own row.
    ///
    /// # Errors
    /// The store cannot be read or written, the row is not an owner, or another frontend
    /// won every try ([`StateError::Conflict`]).
    pub(super) fn arrive(&self, worker: &WorkerId, now: FarmTime) -> Result<Arrival, StateError> {
        let previous = self.claim(worker, now)?;
        match self.read_arrival(worker, previous) {
            Ok(arrival) => {
                self.forget_handover(worker);
                Ok(arrival)
            }
            Err(error) => {
                self.remember_handover(worker, previous);
                self.remember_owner(worker, Ownership::Lost);
                Err(error)
            }
        }
    }

    /// What the claim of `worker`, which replaced `previous`, takes over.
    fn read_arrival(
        &self,
        worker: &WorkerId,
        previous: Option<Owner>,
    ) -> Result<Arrival, StateError> {
        let leases = self.held_by(worker)?;
        let session = self.session_instance(worker)?;
        Ok(Arrival {
            frontend: self.frontend,
            previous,
            leases,
            session,
        })
    }

    fn remember_handover(&self, worker: &WorkerId, previous: Option<Owner>) {
        let mut handovers = self
            .handovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        handovers.insert(worker.clone(), previous);
    }

    fn forget_handover(&self, worker: &WorkerId) {
        let mut handovers = self
            .handovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        handovers.remove(worker);
    }

    /// The owner a claim of `worker` replaced, given the one it read in the row: the owner
    /// an earlier claim whose arrival did not complete replaced, if the row is that claim's
    /// own (this frontend's), else the row's.
    fn replaced(&self, worker: &WorkerId, read: Option<Owner>) -> Option<Owner> {
        let own = read.is_some_and(|owner| owner.frontend == self.frontend);
        let handovers = self
            .handovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match handovers.get(worker) {
            Some(before) if own => *before,
            _ => read,
        }
    }

    /// Makes this frontend the owner of `worker`: the row, with its counter raised by one,
    /// put where the row is as it was read. Returns the owner it named before.
    fn claim(&self, worker: &WorkerId, now: FarmTime) -> Result<Option<Owner>, StateError> {
        let key = owner_key(worker);
        let claimed = contended(|| {
            let row = self.store.get(&key)?;
            let previous = match &row {
                Some(row) => Some(Owner::from_row(&key, row)?),
                None => None,
            };
            let cond = row.map_or(Cond::Absent, |row| Cond::Version(row.version));
            let counter = previous.map_or(1, |owner| owner.counter.saturating_add(1));
            let value = pair_row(self.frontend, counter);
            let txn = Txn::new().put(key.clone(), value, cond);
            self.commit(txn)?;
            Ok((previous, counter))
        });
        match claimed {
            Ok((previous, counter)) => {
                self.remember_owner(worker, Ownership::held(counter, now));
                Ok(self.replaced(worker, previous))
            }
            Err(error) => {
                self.remember_owner(worker, Ownership::Lost);
                Err(error)
            }
        }
    }

    /// The daemon process the `session/` row of `worker` names, if the row is there and is
    /// a registration.
    fn session_instance(&self, worker: &WorkerId) -> Result<Option<DaemonInstance>, StateError> {
        let key = format!("{SESSION}{worker}");
        let Some(row) = self.store.get(&key)? else {
            return Ok(None);
        };
        Ok(self.session_row(&key, &row).ok().map(|up| up.instance))
    }

    fn remember_owner(&self, worker: &WorkerId, state: Ownership) {
        let mut owners = self.owners.lock().unwrap_or_else(PoisonError::into_inner);
        owners.insert(worker.clone(), state);
    }

    fn ownership_of(&self, worker: &WorkerId) -> Option<Ownership> {
        let owners = self.owners.lock().unwrap_or_else(PoisonError::into_inner);
        owners.get(worker).copied()
    }

    /// The error of a write for `worker` that finds it owned by another frontend; the
    /// node's ownership is lost from now on.
    fn disowned(&self, worker: &WorkerId) -> StateError {
        self.remember_owner(worker, Ownership::Lost);
        StateError::StaleOwner(worker.clone())
    }

    /// `txn`, which writes a lease of `worker`, with the check that the `owner/` row is
    /// the one this frontend wrote, at the version read now: the transaction fails if the
    /// row changes before it commits. A node this frontend never claimed adds nothing.
    ///
    /// # Errors
    /// [`StateError::StaleOwner`] if the row names another frontend or counter, or this
    /// frontend gave the node up before; the store cannot be read.
    pub(super) fn guard(&self, txn: Txn, worker: &WorkerId) -> Result<Txn, StateError> {
        let counter = match self.ownership_of(worker) {
            None => return Ok(txn),
            Some(Ownership::Lost) => return Err(StateError::StaleOwner(worker.clone())),
            Some(Ownership::Held { counter, .. }) => counter,
        };
        let key = owner_key(worker);
        let Some(row) = self.store.get(&key)? else {
            return Err(self.disowned(worker));
        };
        let mine = Owner {
            frontend: self.frontend,
            counter,
        };
        if Owner::from_row(&key, &row)? != mine {
            return Err(self.disowned(worker));
        }
        Ok(txn.check(key, Cond::Version(row.version)))
    }

    /// Commits `txn`, which [`Durable::guard`] checked the owner of `worker` in. A conflict
    /// on the owner row is the node changing hands between the read and the commit.
    pub(super) fn commit_owned(&self, worker: &WorkerId, txn: Txn) -> Result<(), StateError> {
        match self.commit(txn) {
            Err(StateError::Conflict(conflict)) if conflict.key == owner_key(worker) => {
                Err(self.disowned(worker))
            }
            done => done,
        }
    }

    /// Whether this frontend owns `worker` at `now`. The row is read when the frontend's
    /// last look is [`OWNER_CACHE_TTL`] old; a node it never claimed, or gave up, is not its
    /// own.
    ///
    /// # Errors
    /// The store cannot be read, or the row is not an owner.
    pub(super) fn owns(&self, worker: &WorkerId, now: FarmTime) -> Result<bool, StateError> {
        let Some(Ownership::Held { counter, checked }) = self.ownership_of(worker) else {
            return Ok(false);
        };
        if now < checked.saturating_add(OWNER_CACHE_TTL) {
            return Ok(true);
        }
        let key = owner_key(worker);
        let mine = Owner {
            frontend: self.frontend,
            counter,
        };
        let holds = match self.store.get(&key)? {
            Some(row) => Owner::from_row(&key, &row)? == mine,
            None => false,
        };
        if holds {
            self.remember_owner(worker, Ownership::held(counter, now));
        } else {
            self.remember_owner(worker, Ownership::Lost);
        }
        Ok(holds)
    }
}

impl Engine {
    /// A daemon said `Hello` here as `worker`, whose newest stream is `instance`'s: this
    /// frontend becomes its owner, takes up the leases the store holds for it that the
    /// scheduler does not hold (a drain of the node that ended at the start, for want of a
    /// lease, is put back in force: [`State::resume_drain`]), and reads what the node's
    /// session row said before the registration replaces it. Call it before the registration is written and the worker
    /// is fed to the scheduler.
    ///
    /// The leases the scheduler recovered for the node wait for it to register, as they
    /// do after a start. Those of a node whose owner was another frontend are kept for
    /// [`OWNER_HANDOVER_WAIT`] after the registration when the daemon process may be
    /// another than the one the other frontend heard (the session row names another, or
    /// has expired): that process may go on running them, acknowledged by the other
    /// frontend, until it fences, so a heartbeat that leaves them out is not taken as
    /// proof that they stopped.
    ///
    /// A claim the store did not take because it could not be reached is tried again, while
    /// the node is connected, when a feed finds the store reachable ([`Engine::claim_pending`]).
    pub(in crate::farm) fn claim_worker(
        &mut self,
        a: &mut dyn Adapter,
        now: FarmTime,
        worker: &WorkerId,
        instance: &DaemonInstance,
    ) {
        let Some(arrival) = self.take_arrival(now, worker, instance) else {
            return;
        };
        for op in arrival.leases {
            // An operation this frontend took in while it was queued, and another frontend
            // has granted to this node since, is still held queued until the next intake
            // poll sees its `queue/` row gone. It leaves the queue now, as the poll would
            // have it, so the lease the node holds is taken up and its result is accepted.
            if self.sched.queued().any(|queued| queued == op.operation) {
                self.hand_off(op.operation);
            }
            // An operation the scheduler finished and retired, whose finish is not written
            // yet, is not taken in again: it would run twice.
            if !self.known_operation(op.operation) {
                self.take_in_stored(a, op);
            }
        }
        self.resume_drain(now, worker);
        let moved = match arrival.previous {
            Some(owner) => owner.frontend != arrival.frontend,
            None => false,
        };
        let same_process = match arrival.session {
            Some(old) => old.same_as(instance),
            None => false,
        };
        if moved && !same_process {
            let sched = &mut self.sched;
            sched.hold_recovered_leases(now, worker, OWNER_HANDOVER_WAIT);
        }
    }

    /// What the store says of `worker` once this frontend owns it; `None` if the farm has
    /// no state store, or the store did not take the claim. A claim refused because the
    /// store could not be reached is kept to be made again, in place of an older one for
    /// the node, which was made for a daemon process that has been replaced.
    fn take_arrival(
        &mut self,
        now: FarmTime,
        worker: &WorkerId,
        instance: &DaemonInstance,
    ) -> Option<Arrival> {
        self.unclaimed.remove(worker);
        let mut arrival = None;
        let attempt = self.attempt("an owner claim", |d| {
            arrival = Some(d.arrive(worker, now)?);
            Ok(())
        });
        if attempt == Attempt::Transient {
            self.unclaimed.insert(worker.clone(), instance.clone());
        }
        arrival
    }

    /// Claims the nodes whose `Hello` found the state store unreachable, and whose streams
    /// are still open, if the store is reachable now. Called at the start of every feed.
    pub(in crate::farm) fn claim_pending(&mut self, a: &mut dyn Adapter, now: FarmTime) {
        if self.store_down {
            return;
        }
        for (worker, instance) in std::mem::take(&mut self.unclaimed) {
            if a.stream_open(&worker) {
                self.claim_worker(a, now, &worker, &instance);
            }
        }
    }

    /// Whether this frontend owns `worker` now: a heartbeat of a node it does not own is
    /// neither acknowledged nor a reason to renew the node's session row. A farm with no
    /// state store, or one that handed its work over, owns every node it hears from.
    pub(in crate::farm) fn owns_worker(&self, now: FarmTime, worker: &WorkerId) -> bool {
        if self.fenced || self.durable.is_none() {
            return true;
        }
        let mut owns = false;
        self.durably("an owner check", |d| {
            owns = d.owns(worker, now)?;
            Ok(())
        });
        owns
    }

    /// Whether the session row of `worker` was written, at its registration or a renewal,
    /// within [`ACK_WINDOW`] of `now`. A farm with no state store, or one that handed its
    /// work over, writes none and needs none.
    pub(in crate::farm) fn session_fresh(&self, now: FarmTime, worker: &WorkerId) -> bool {
        if self.fenced || self.durable.is_none() {
            return true;
        }
        let Some(written) = self.session_ok.get(worker) else {
            return false;
        };
        now < written.saturating_add(ACK_WINDOW)
    }
}
