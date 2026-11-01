//! The scheduler's transitions in the state store: a grant, a lease given up, a result
//! accepted and an operation refused. Several frontends may share one store and its rows,
//! so each transition reads the rows it depends on and writes conditionally on what it
//! read, in one transaction. A frontend that loses finds the rows changed or gone and
//! writes nothing, and the other's work stands:
//!
//! - a grant deletes the `queue/` row at the version read and puts the `lease/` row where
//!   there is none, so one operation never holds two leases;
//! - a lease given up deletes the `lease/` row at the version read, checks that the `op/`
//!   row is still the one read, and puts the `queue/` row where there is none. With no
//!   `lease/` row it writes nothing of the operation: another frontend finished or
//!   requeued it (or an earlier try of this very write did), and putting the `queue/` row
//!   would run a finished operation again or queue a running one beside its lease;
//! - a result deletes the `lease/` row at the version read; if another frontend deletes it
//!   first, the result is made again as with no `lease/` row. With no `lease/` row, the
//!   result is the one of an operation whose lease the farm gave up in the same feed: its
//!   `queue/` and `op/` rows go at the versions read, and the result is put where there is
//!   none. If another writer changed those rows, the result is read: one already there
//!   means the operation was finished (by an earlier try of this write, or by another
//!   frontend), and this finish is a duplicate that is acknowledged, the first result
//!   standing;
//! - a refusal deletes the `queue/` and `op/` rows at the versions read, so it loses to a
//!   grant (which deletes the `queue/` row) of another frontend: an operation that is
//!   running is not refused. A refusal that finds its own result in the store is a
//!   duplicate too.
//!
//! The memory floors a lease given up raises are written with it, each at the version read
//! of its row and with the larger ask of the row and its own: a frontend whose kills raised
//! a floor less far than another's never lowers it, and one that finds the row changed
//! between its read and its write loses, and writes the give-up again from the row as it
//! stands.
//!
//! A write the store applied and did not acknowledge is tried again, and finds its own work
//! in the rows: the lease row of the grant, no lease row to give up, the result.
//!
//! A result and a refusal also put the `doneof/` row of the operation beside its `done/` row,
//! from the request its `op/` row holds (the `named` module), so the transaction that
//! removes the operation's rows leaves the instance and action a client can ask for it by.
//!
//! A grant, a lease given up and a result that finds its lease row also check the `owner/`
//! row of the lease's node (the `owner` module): the transaction fails, whatever else it
//! read, if the daemon has said `Hello` to another frontend since this one did. The grant
//! puts the operation's `lease-by-op/` row beside the lease row (the `leases` module), and
//! the lease given up, like every ending, deletes it.

use kbf_types::StartLease;

use super::leases::lease_by_op_row;
use super::*;

impl Durable {
    /// A grant committed: the operation leaves the queue and holds the lease of `start`.
    /// The operation must be queued, as the transaction reads it. A grant that finds the
    /// operation not queued and the lease row naming this lease and worker is a write
    /// the store applied before without saying so: it is taken as written.
    ///
    /// # Errors
    /// [`StateError::Missing`] if the operation is not queued and holds no such lease,
    /// [`StateError::Conflict`] if its rows changed under the write,
    /// [`StateError::StaleOwner`] if the worker is another frontend's.
    pub(super) fn granted(&self, start: &StartLease) -> Result<(), StateError> {
        // First: a node another frontend owns is granted nothing, whatever the rows say.
        let txn = self.guard(Txn::new(), &start.worker)?;
        let queued = queue_key(start.operation);
        let grant = LeaseGrant {
            lease: start.lease,
            operation: start.operation,
            worker: start.worker.clone(),
            // This store's lease rows keep no cost.
            cost: Default::default(),
        };
        let Some(row) = self.store.get(&queued)? else {
            return self.granted_before(&grant, queued);
        };
        let value = self.codec.lease(&grant)?;
        let lease = lease_key(start.lease);
        let index = lease_by_op_key(start.operation);
        let txn = txn
            .delete(queued, Cond::Version(row.version))
            .put(lease, value, Cond::Absent)
            .put(index, lease_by_op_row(start.lease), Cond::Any);
        self.commit_owned(&start.worker, txn)
    }

    /// The grant of an operation that is not queued: written already if the lease row is
    /// `grant`, by an earlier try of this write; stale otherwise.
    fn granted_before(&self, grant: &LeaseGrant, queued: String) -> Result<(), StateError> {
        let key = lease_key(grant.lease);
        match self.store.get(&key)? {
            Some(row) if self.lease_row(&key, &row)? == *grant => Ok(()),
            _ => Err(StateError::Missing(queued)),
        }
    }

    /// A lease given up: the operation holds nothing and is queued again, if it still held
    /// the lease. Repeatable, and safe beside other frontends: the farm writes a give-up
    /// the store refused again until the store takes it, and a repeat after it was taken
    /// finds the lease row gone and writes nothing, as a give-up of a lease that another
    /// frontend finished or requeued does. An operation that has finished has nothing to
    /// put back.
    ///
    /// `losses` is the operation's count of lost leases, to write down if the give-up is
    /// one ([`kbf_sched::RequeueReason::is_loss`]). `fate` is what the operation's faults
    /// changed so far, if they changed anything: it goes in the same transaction as the
    /// queue row, each over the version it read of the row.
    ///
    /// The loss count and the fate are written only if the store held the lease: a grant
    /// the farm gave up before the store took it has no lease row, the operation is still
    /// queued there, and nothing of that lease is written, so a restart does not count it.
    ///
    /// The memory floors the give-up changes are written whether or not the operation is
    /// queued again: a floor is the farm's, and outlives the operation.
    #[cfg(test)]
    pub(super) fn requeued(
        &self,
        requeue: &Requeue,
        losses: Option<u32>,
        fate: Option<&Fate>,
        floors: &FloorChange,
    ) -> Result<(), StateError> {
        self.requeued_named(requeue, losses, fate, floors, None)
    }

    /// [`Durable::requeued`], with the size summary the raised key floor decays with, if
    /// it is known: its row names it.
    pub(super) fn requeued_named(
        &self,
        requeue: &Requeue,
        losses: Option<u32>,
        fate: Option<&Fate>,
        floors: &FloorChange,
        summary: Option<&kbf_estimator::Key>,
    ) -> Result<(), StateError> {
        let mut txn = Txn::new();
        // The floor outlives the operation: a kill raises it, and the operation may end
        // before the write is taken.
        let raised = floors.raised.as_ref();
        if let Some(floor) = raised {
            // Another frontend may have raised the floor further: the row keeps the larger
            // ask, and is written at the version read.
            let (merged, cond) = self.merged_floor(floor)?;
            let row = floor::encode(&merged).ok_or_else(|| {
                corrupt(&floor::row_key(&floor.key), "a floor is too large to write")
            })?;
            txn = txn.put(floor::row_key(&floor.key), row, cond);
        }
        // A key forgotten and raised again since keeps its row.
        for key in floors
            .evicted
            .iter()
            .filter(|k| raised.is_none_or(|f| &f.key != *k))
        {
            txn = txn.delete(floor::row_key(key), Cond::Any);
        }
        // The same for the key floor, in the same transaction.
        let key_raised = floors.key_raised.as_ref();
        if let Some(floor) = key_raised {
            let (merged, cond) = self.merged_key_floor(floor)?;
            let value = self.key_floor_value(&merged, summary)?;
            txn = txn.put(floor::key_row(floor.key), value, cond);
        }
        for key in floors
            .key_evicted
            .iter()
            .filter(|k| key_raised.is_none_or(|f| f.key != **k))
        {
            txn = txn.delete(floor::key_row(*key), Cond::Any);
        }
        let floors_changed = raised.is_some()
            || !floors.evicted.is_empty()
            || key_raised.is_some()
            || !floors.key_evicted.is_empty();
        let (op, lease) = (op_key(requeue.operation), lease_key(requeue.lease));
        let rows = (self.store.get(&op)?, self.store.get(&lease)?);
        let (Some(unfinished), Some(held)) = rows else {
            // The operation ended, or its lease is not held in the store any more: nothing
            // is put back in the queue.
            return if floors_changed {
                self.commit(txn)
            } else {
                Ok(())
            };
        };
        // The lease is the node's owner's to give up. The floors are the farm's, and are
        // written whoever owns the node: a give-up refused for the owner is not tried again.
        txn = match self.guard(txn.clone(), &requeue.worker) {
            Ok(guarded) => guarded,
            Err(refused) => {
                if floors_changed {
                    self.commit(txn)?;
                }
                return Err(refused);
            }
        };
        txn = txn
            .check(op, Cond::Version(unfinished.version))
            .delete(lease, Cond::Version(held.version))
            .delete(lease_by_op_key(requeue.operation), Cond::Any);
        if let Some(count) = losses {
            let key = loss_key(requeue.operation);
            let cond = self.current(&key)?;
            txn = txn.put(key, u64::from(count).to_be_bytes(), cond);
        }
        if let Some(fate) = fate {
            let row = fate::encode(fate).ok_or_else(|| {
                corrupt(&fate_key(requeue.operation), "a fate is too large to write")
            })?;
            let key = fate_key(requeue.operation);
            let cond = self.current(&key)?;
            txn = txn.put(key, row, cond);
        }
        let queue = queue_key(requeue.operation);
        let txn = txn.put(queue, Vec::new(), Cond::Absent);
        self.commit_owned(&requeue.worker, txn)
    }

    /// What a write that replaces the row at `key` must find: the version it has now, or
    /// no row.
    pub(super) fn current(&self, key: &str) -> Result<Cond, StateError> {
        let row = self.store.get(key)?;
        Ok(row.map_or(Cond::Absent, |row| Cond::Version(row.version)))
    }

    /// `floor` with the larger ask of its own and the stored row's, and what a write of the
    /// row must find: the version read, or no row. A row that cannot be read counts for
    /// nothing, and is written over.
    fn merged_floor(&self, floor: &Floor) -> Result<(Floor, Cond), StateError> {
        let row = self.store.get(&floor::row_key(&floor.key))?;
        let held = row
            .as_ref()
            .and_then(|row| floor::decode(&row.value).ok())
            .map_or(0, |held| held.ask);
        let cond = row.map_or(Cond::Absent, |row| Cond::Version(row.version));
        let merged = Floor {
            ask: held.max(floor.ask),
            ..floor.clone()
        };
        Ok((merged, cond))
    }

    /// [`Durable::merged_floor`] for a key floor.
    fn merged_key_floor(&self, floor: &KeyFloor) -> Result<(KeyFloor, Cond), StateError> {
        let row = self.store.get(&floor::key_row(floor.key))?;
        let held = row
            .as_ref()
            .and_then(|row| floor::decode_key(&row.value).ok())
            .map_or(0, |(held, _)| held.ask);
        let cond = row.map_or(Cond::Absent, |row| Cond::Version(row.version));
        let merged = KeyFloor {
            ask: held.max(floor.ask),
            ..floor.clone()
        };
        Ok((merged, cond))
    }

    /// A result accepted from `lease`: the operation is finished. The lease row goes if
    /// it is there, compare-and-set on its version. It may be gone: the scheduler lets a
    /// worker's result decide an operation whose lease it gave up in the same feed, and
    /// that give-up ([`Durable::requeued`]) already deleted the row and put the queue row
    /// back. Then the operation's `queue/` and `op/` rows go at the versions read, so a
    /// finished operation is never left queued, and the result goes only where there is
    /// none. A result already there is a duplicate of this finish, which is taken as
    /// written, the first standing. A lease row gone with the operation not queued is not
    /// that give-up: `Missing`. The lease row may also go between the read and the write,
    /// to a requeue or a finish of another frontend: the write is then made again as if the
    /// row had been gone at the read. A lease row that changed and is still there is
    /// another writer's: `Conflict`.
    ///
    /// The `done/` row of `ending` is put in the same transaction as the deletes, so the
    /// store holds the operation's unfinished rows or its result, never neither.
    pub(super) fn finished(
        &self,
        operation: OperationId,
        lease: LeaseId,
        ending: &Ending,
    ) -> Result<(), StateError> {
        let key = lease_key(lease);
        let Some(row) = self.store.get(&key)? else {
            return self.end_queued(operation, ending, &key, |_| true);
        };
        // The result of a lease is taken by the owner of the node that holds it.
        let worker = self.lease_row(&key, &row)?.worker;
        let txn = self.guard(Txn::new(), &worker)?;
        let txn = txn.delete(key.clone(), Cond::Version(row.version));
        let txn = self.unpin(txn, operation)?;
        let txn = self.noting(txn, operation, ending)?;
        let walled = self.has_wall(operation)?;
        let settled = ending.settle_with(&*self.ext, txn, operation, walled);
        let committed = self.commit_owned(&worker, settled);
        if matches!(committed, Err(StateError::Conflict(_))) && self.store.get(&key)?.is_none() {
            return self.end_queued(operation, ending, &key, |_| true);
        }
        committed
    }

    /// Whether the store holds an `opwall/` row for `operation`: only then does the finish
    /// delete one.
    fn has_wall(&self, operation: OperationId) -> Result<bool, StateError> {
        Ok(self.store.get(&opwall_key(operation))?.is_some())
    }

    /// A refusal committed: the operation is finished without a lease, and its `done/`
    /// row goes in the same transaction as its rows are removed. The operation must be
    /// queued, as the transaction reads it: one that another frontend has granted is
    /// running, and is not refused. A result already there that is this refusal is a
    /// duplicate of it, which is taken as written; any other result stands, and the
    /// refusal is stale. The rows are read when the refusal is written, which may be after
    /// the store was unreachable for a while: an operation another frontend queued again
    /// in that time is refused all the same, on the judgment made when it was proposed.
    pub(super) fn refused(
        &self,
        operation: OperationId,
        ending: &Ending,
    ) -> Result<(), StateError> {
        let queued = queue_key(operation);
        self.end_queued(operation, ending, &queued, |done| *done == ending.finished)
    }

    /// Ends `operation` in one transaction that deletes its `queue/` and `op/` rows at the
    /// versions read and puts the result of `ending` where there is none. If the rows are
    /// not both there, or another writer changed them since they were read, the result is
    /// read: `duplicate` says whether the one there is this ending's, and then it is taken
    /// as written. Otherwise the write is stale, naming `stale`.
    fn end_queued(
        &self,
        operation: OperationId,
        ending: &Ending,
        stale: &str,
        duplicate: impl Fn(&Finished) -> bool,
    ) -> Result<(), StateError> {
        let queue = queue_key(operation);
        let op = op_key(operation);
        let rows = (self.store.get(&queue)?, self.store.get(&op)?);
        if let (Some(queued), Some(unfinished)) = rows {
            let txn = Txn::new()
                .delete(queue, Cond::Version(queued.version))
                .delete(op, Cond::Version(unfinished.version));
            let txn = self.unpin(txn, operation)?;
            let txn = self.noting(txn, operation, ending)?;
            let walled = self.has_wall(operation)?;
            let settled = ending.settle_rest(&*self.ext, txn, operation, walled, Cond::Absent);
            let committed = self.commit(settled);
            if !matches!(committed, Err(StateError::Conflict(_))) {
                return committed;
            }
        }
        match self.ended(operation)? {
            Some(done) if duplicate(&done) => Ok(()),
            _ => Err(StateError::Missing(stale.to_owned())),
        }
    }

    /// The capabilities and capacity of every daemon the session rows list that is not in
    /// `connected` (the nodes of this frontend): the daemons of the other frontends of the
    /// state store, each renewed by the frontend it is connected to.
    ///
    /// # Errors
    /// The store cannot be read, or a session row is not a daemon's registration.
    pub(super) fn remote_sessions(
        &self,
        connected: &std::collections::BTreeSet<WorkerId>,
    ) -> Result<Vec<(kbf_caps::NodeCaps, kbf_types::Resources)>, StateError> {
        let mut remote = Vec::new();
        for (key, row) in self.store.scan_prefix(SESSION)? {
            if connected.contains(&WorkerId::new(&key[SESSION.len()..])) {
                continue;
            }
            let up = self.session_row(&key, &row)?;
            remote.push((up.caps, up.capacity));
        }
        Ok(remote)
    }

    /// Whether a daemon session in the store, of any frontend, could run `request`: the
    /// session rows hold each connected daemon's capabilities and capacity, written by the
    /// frontend it is connected to, and expire unless that frontend renews them. An
    /// operation is unservable only if none could.
    ///
    /// # Errors
    /// The store cannot be read, or a session row is not a daemon's registration.
    pub(super) fn session_could_run(&self, request: &Request) -> Result<bool, StateError> {
        for (key, row) in self.store.scan_prefix(SESSION)? {
            let up = self.session_row(&key, &row)?;
            if request.runnable_on(&up.caps, &up.capacity) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
