//! The orphan sweep: a lease whose node no frontend hears is requeued.
//!
//! A lease is held by a daemon connected to one frontend of the state store, and the
//! store's `lease/` row is the frontend's. If the frontend dies with its daemons (the host
//! fails), nothing gives the lease up: the frontends that remain hold no state for it. Every
//! frontend therefore looks, at each intake poll, for leases whose node has no `session/`
//! row: the rows expire [`LEASE_GRACE`] after the last write of the frontend that owns the
//! node, which writes one every [`SESSION_RENEWAL`] while the daemon heartbeats, and no
//! frontend writes a row for a node it does not hear. A lease whose node is connected to
//! this frontend is left alone, whatever its session row says.
//!
//! The first look that finds such a lease remembers it, with the store's clock reading, the
//! version of its row and that of its node's `owner/` row. A later look that finds the lease
//! as it was, and its node still without a session, [`ORPHAN_WAIT`] after the first (by two
//! readings of the store's clock, never this host's), requeues it in one transaction that
//! checks the rows it read: the `lease/` row at its version, the `op/` row, the `owner/`
//! row at its version (a daemon that said `Hello` to a frontend since has a session there
//! within moments, and owns its lease again), and writes the `queue/` row where there is
//! none and the loss count one higher, as any lease lost to a silent node is counted. A
//! lease that changed, or whose node came back, starts the wait over; so does one whose
//! requeue lost to a write of another frontend between the look and the transaction, and
//! the pass goes on to the other leases.
//!
//! Why the wait is safe. A daemon stops its work `SELF_FENCE` after the newest heartbeat
//! its frontend acknowledged, and a frontend acknowledges only while it wrote the node's
//! session row within `ACK_WINDOW`, so the daemon runs no longer than `ACK_WINDOW` plus
//! `SELF_FENCE` (and the margin the fence keeps) past the last write of the row. The row
//! was there `LEASE_GRACE` after that write, so the first look that misses it is made at
//! least that long after the write, and the second at least [`ORPHAN_WAIT`] after the first:
//! the check below holds at compile time.

use std::collections::BTreeSet;
use std::collections::btree_map::Entry;

use kbf_sched::fence::{LEADER_LEASE_MARGIN, SELF_FENCE};
use kbf_state::StoreTime;

use super::owner::owner_key;
use super::*;

/// How long a lease whose node has no session row is left, measured between two readings of
/// the store's clock, before it is requeued: one renewal interval of the session row (the
/// first look that misses the row is made after its expiry) and the margin of the fence.
pub const ORPHAN_WAIT: Duration = SESSION_RENEWAL.saturating_add(LEADER_LEASE_MARGIN);

// The daemon of a lease has stopped its work by the second look: it ran at most `ACK_WINDOW`
// and `SELF_FENCE` (and the margin) past the last write of its session row, which was
// `LEASE_GRACE` before the first look at the latest.
const _: () = assert!(
    LEASE_GRACE.as_millis() + ORPHAN_WAIT.as_millis()
        > ACK_WINDOW.as_millis() + SELF_FENCE.as_millis() + LEADER_LEASE_MARGIN.as_millis()
);

/// A lease whose node had no session row at a look.
#[derive(Clone, Copy, Debug)]
pub(super) struct Orphan {
    /// The store's clock at the look.
    first: StoreTime,
    /// The version of the lease's row.
    lease: Version,
    /// The version of its node's `owner/` row, if the node had one.
    owner: Option<Version>,
}

impl Durable {
    /// One look at the leases whose node has no session row: remembers each in `seen` (and
    /// forgets those that are none now), and requeues those `wait` after the first look
    /// that found them as they are. Leases of the nodes in `connected` are left alone.
    ///
    /// # Errors
    /// The store cannot be read or written, or a row read is not one the scheduler wrote.
    pub(super) fn sweep(
        &self,
        wait: Duration,
        connected: &BTreeSet<WorkerId>,
        seen: &mut BTreeMap<LeaseId, Orphan>,
    ) -> Result<(), StateError> {
        let now = self.store.now();
        let sessions: BTreeSet<String> = self
            .store
            .scan_prefix(SESSION)?
            .into_iter()
            .map(|(key, _)| key[SESSION.len()..].to_owned())
            .collect();
        let mut orphaned = BTreeSet::new();
        for (key, row) in self.store.scan_prefix(LEASE)? {
            let grant = self.lease_row(&key, &row)?;
            if connected.contains(&grant.worker) || sessions.contains(grant.worker.as_str()) {
                continue;
            }
            orphaned.insert(grant.lease);
            let owner = self
                .store
                .get(&owner_key(&grant.worker))?
                .map(|owner| owner.version);
            let look = Orphan {
                first: now,
                lease: row.version,
                owner,
            };
            match seen.entry(grant.lease) {
                Entry::Vacant(slot) => {
                    slot.insert(look);
                }
                Entry::Occupied(mut slot) => {
                    let first = *slot.get();
                    if first.lease != look.lease || first.owner != look.owner {
                        slot.insert(look);
                    } else if now.since(first.first) >= wait {
                        // A row changed since the look: the lease is looked at afresh by
                        // the next pass, and the pass goes on to the other leases.
                        match self.requeue_orphan(&grant, &row, owner) {
                            Ok(()) | Err(StateError::Conflict(_) | StateError::Missing(_)) => {
                                slot.remove();
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
            }
        }
        seen.retain(|lease, _| orphaned.contains(lease));
        Ok(())
    }

    /// Requeues the lease `lease` (`grant`), whose node's `owner/` row is at `owner`: one
    /// transaction that deletes the lease and its index row, queues the operation and
    /// counts the loss, if the rows are as read. An operation with no `op/` row has
    /// finished and has nothing to queue.
    fn requeue_orphan(
        &self,
        grant: &LeaseGrant,
        lease: &Versioned,
        owner: Option<Version>,
    ) -> Result<(), StateError> {
        let op = op_key(grant.operation);
        let Some(unfinished) = self.store.get(&op)? else {
            return Ok(());
        };
        let losses = loss_key(grant.operation);
        let (count, cond) = match self.store.get(&losses)? {
            Some(row) => (
                number(&losses, &row)?.saturating_add(1),
                Cond::Version(row.version),
            ),
            None => (1, Cond::Absent),
        };
        let txn = Txn::new()
            .check(op, Cond::Version(unfinished.version))
            .check(
                owner_key(&grant.worker),
                owner.map_or(Cond::Absent, Cond::Version),
            )
            .delete(lease_key(grant.lease), Cond::Version(lease.version))
            .delete(lease_by_op_key(grant.operation), Cond::Any)
            .put(losses, count.to_be_bytes(), cond)
            .put(queue_key(grant.operation), Vec::new(), Cond::Absent);
        self.commit(txn)?;
        tracing::warn!(
            operation = %grant.operation,
            lease = %grant.lease,
            node = %grant.worker,
            "no frontend hears the node of the lease: the operation is queued again"
        );
        Ok(())
    }
}
