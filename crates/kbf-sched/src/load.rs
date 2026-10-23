//! How loaded a worker is: what placement ranks the workers that could run an action by,
//! so work spreads across them instead of filling the first.

use std::cmp::Ordering;

use kbf_types::Resources;

/// A worker's load; the smaller is the less loaded. The largest of its CPU share, its
/// memory share and its lease share (its leases of its lease cap, if it reports one) of
/// what is booked, then how many leases it holds.
///
/// Both shares count because the node's limits differ in what happens at the end: a node
/// out of cores runs its actions slowly, a node out of memory kills them. A share is not
/// capped at 1: admission books no memory ([`crate::scheduler::Worker::has_room`]), so a
/// worker can hold leases whose memory is more than it has, and the worker with more
/// of it booked is still the one to avoid. (A cap would make every such worker a tie,
/// exactly when the farm is busy.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Load {
    share: Share,
    leases: usize,
}

impl Load {
    /// The load of a worker of `capacity` that has `booked` booked in `leases` leases.
    pub(crate) fn new(booked: Resources, capacity: Resources, leases: usize) -> Self {
        let cpu = Share::new(booked.cpu_millis, capacity.cpu_millis);
        let memory = Share::new(booked.memory_bytes, capacity.memory_bytes);
        // A node with no lease cap (or a cap of 0) has no lease share to be short of.
        let held = u64::try_from(leases).unwrap_or(u64::MAX);
        let lease_cap = Share::new(held, capacity.leases.unwrap_or(0));
        Self {
            share: cpu.max(memory).max(lease_cap),
            leases,
        }
    }
}

/// `booked / capacity`, compared exactly: placement is replayed by the simulator and
/// must give the same choice every time, which a float need not.
#[derive(Clone, Copy, Debug)]
struct Share {
    booked: u64,
    capacity: u64,
}

impl Share {
    /// A resource the worker has none of is one it cannot be short of: its share is 0
    /// whatever is booked, and nothing divides by zero.
    fn new(booked: u64, capacity: u64) -> Self {
        if capacity == 0 {
            Self {
                booked: 0,
                capacity: 1,
            }
        } else {
            Self { booked, capacity }
        }
    }
}

impl Ord for Share {
    fn cmp(&self, other: &Self) -> Ordering {
        // `a / b` against `c / d`, with `b` and `d` above 0, is `a * d` against `c * b`;
        // a product of two `u64`s fits a `u128`.
        (u128::from(self.booked) * u128::from(other.capacity))
            .cmp(&(u128::from(other.booked) * u128::from(self.capacity)))
    }
}

impl PartialOrd for Share {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Share {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Share {}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn load(cpu: (u64, u64), memory: (u64, u64), leases: usize) -> Load {
        Load::new(
            Resources::new(cpu.0, memory.0),
            Resources::new(cpu.1, memory.1),
            leases,
        )
    }

    /// Catches: a share compared by division in floats, or by a product that overflows:
    /// the shares one part in `u64::MAX` apart must differ, and equal fractions of
    /// different capacities must be equal (a tie falls to the lease count and the name,
    /// not to rounding).
    #[test]
    fn shares_compare_exactly() {
        let just_under = load((u64::MAX - 1, u64::MAX), (0, GIB), 0);
        let full = load((u64::MAX, u64::MAX), (0, GIB), 0);
        assert!(just_under < full);
        assert_eq!(load((1, 3), (0, GIB), 0), load((2_000, 6_000), (0, GIB), 0));
        assert!(load((1, 3), (0, GIB), 0) < load((34, 100), (0, GIB), 0));
    }

    /// Catches: only one resource ranking a worker: the load is the larger share, so a
    /// worker at half its cores and three quarters of its memory is as loaded as one at
    /// three quarters of its cores and a sixteenth of its memory, and more than one at half
    /// of each.
    #[test]
    fn the_larger_of_the_cpu_and_memory_shares_is_the_load() {
        let cpu_bound = load((750, 1_000), (GIB, 16 * GIB), 0);
        let memory_bound = load((500, 1_000), (12 * GIB, 16 * GIB), 0);
        let both_half = load((500, 1_000), (8 * GIB, 16 * GIB), 0);
        assert_eq!(cpu_bound, memory_bound);
        assert!(both_half < memory_bound);
    }

    /// Catches: a share capped at 1, which ties every worker booked past its memory.
    #[test]
    fn a_share_past_one_still_ranks() {
        let a = load((1_000, 8_000), (24 * GIB, 16 * GIB), 4);
        let b = load((1_000, 8_000), (16 * GIB, 16 * GIB), 4);
        assert!(b < a);
    }

    /// Catches: a resource with no capacity dividing by zero, or counting: a worker that
    /// reports no memory is ranked by its cores.
    #[test]
    fn a_resource_without_capacity_counts_for_nothing() {
        let no_memory = load((500, 1_000), (7 * GIB, 0), 1);
        assert_eq!(no_memory, load((500, 1_000), (0, 0), 1));
        assert!(no_memory < load((501, 1_000), (0, 0), 1));
        assert_eq!(load((9, 0), (9, 0), 0), load((0, 5), (0, 5), 0));
    }

    /// Catches: a capped node ranked by its CPUs alone: a 32-core node capped at 2 leases
    /// that holds 1 is half used, more than a peer at 10 of 32 cores, and a node at its
    /// cap is more loaded than any under it. A node without a cap, or with a cap of 0,
    /// is ranked as before.
    #[test]
    fn a_capped_node_is_ranked_by_its_lease_share_too() {
        let capped = |cap: Option<u64>, leases: usize| {
            let capacity = Resources::new(32_000, 0);
            let capacity = cap.map_or(capacity, |c| capacity.with_lease_cap(c));
            Load::new(Resources::new(leases as u64 * 1_000, 0), capacity, leases)
        };
        let peer = load((10_000, 32_000), (0, 0), 10);
        assert!(peer < capped(Some(2), 1));
        assert!(capped(Some(2), 1) < capped(Some(2), 2));
        assert_eq!(capped(None, 1), load((1_000, 32_000), (0, 0), 1));
        assert_eq!(capped(Some(0), 1), capped(None, 1));
    }

    /// Catches: leases ignored when the shares tie, or counted before the shares.
    #[test]
    fn a_tie_in_shares_goes_to_fewer_leases() {
        let few = load((500, 1_000), (GIB, 16 * GIB), 1);
        let many = load((250, 500), (GIB, 16 * GIB), 2);
        assert!(few < many);
        assert!(load((1, 10), (0, GIB), 9) < load((2, 10), (0, GIB), 0));
    }
}
