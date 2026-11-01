//! The lists a [`NodeLoad`] carries (`covered`, `lease_clocks`, `minutes`): their caps and
//! the rules that decide which records a receiver keeps.
//!
//! The load is telemetry, so a bad one never costs the heartbeat around it. [`NodeLoad::vet`]
//! never fails: it drops a record that breaks a rule, alone, and counts it in [`LoadDrops`].
//! The caps and rules are the ones the job scheduler's agent wire applies to the same lists
//! (`js_agent_wire::limits` and its `NodeLoad::vet`); a test of this crate pins the documented
//! numbers, which are js-agent-wire's. The check that the two crates agree is made where both
//! are visible, not here.
//!
//! A list is cut to its cap first and the excess is counted as `over_cap` without being
//! judged; each remaining record is then judged (`invalid`), and a repeat of an earlier kept
//! record is dropped (`duplicate`), the first one staying.

use std::collections::BTreeSet;

use crate::worker::{LeaseClock, LeaseId, LeaseMinute, NodeLoad};

/// The most leases a load names in each of `lease_clocks` and `covered`.
pub const MAX_LOAD_LEASES: usize = 96;
/// The most closed minutes a load carries.
pub const MAX_LOAD_MINUTES: usize = 64;
/// The last minute index. It means "this minute or later": a sender folds every later
/// minute into it with the larger value of each field, and a receiver merges repeats of it
/// the same way.
pub const TAIL_MINUTE: u32 = 63;
/// The largest working set a minute may report, in bytes (32 TiB).
pub const MAX_MEMORY_BYTES: u64 = 1 << 45;

/// How many records [`NodeLoad::vet`] dropped, by reason, summed over the three lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadDrops {
    /// Records that broke a rule: a missing lease, a minute above [`TAIL_MINUTE`], a working
    /// set above [`MAX_MEMORY_BYTES`], a mean above its peak, a start after the reading.
    pub invalid: u32,
    /// Records that repeated an earlier kept one.
    pub duplicate: u32,
    /// Records past the cap of their list.
    pub over_cap: u32,
}

impl LoadDrops {
    /// Every drop, summed without overflow.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.invalid
            .saturating_add(self.duplicate)
            .saturating_add(self.over_cap)
    }
}

/// The records of a [`NodeLoad`]'s lists that passed [`NodeLoad::vet`], in their sent order,
/// and what was dropped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Vetted {
    /// `covered`: at most [`MAX_LOAD_LEASES`], each lease once.
    pub covered: Vec<LeaseId>,
    /// `lease_clocks`: at most [`MAX_LOAD_LEASES`], each lease once, none started after the
    /// reading.
    pub clocks: Vec<LeaseClock>,
    /// `minutes`: at most [`MAX_LOAD_MINUTES`], each (lease, minute) once.
    pub minutes: Vec<LeaseMinute>,
    /// What was dropped from the three lists.
    pub drops: LoadDrops,
}

impl NodeLoad {
    /// The records of this load's lists that a receiver keeps, and a count of those it drops.
    ///
    /// Never fails and never changes the load. Vetting a load built from the result drops
    /// nothing and returns the same records.
    ///
    /// - `minutes`: invalid when `lease` is absent, `minute` is above [`TAIL_MINUTE`],
    ///   `ws_max_bytes` is above [`MAX_MEMORY_BYTES`] or `cpu_mean_millis` is above
    ///   `cpu_peak_window_millis`; a repeat of the same (lease, minute) is a duplicate.
    /// - `lease_clocks`: invalid when `lease` is absent or `started_mono_usec` is after this
    ///   load's `mono_usec`; a repeat of the same lease is a duplicate.
    /// - `covered`: only the cap and duplicates apply.
    #[must_use]
    pub fn vet(&self) -> Vetted {
        let mut drops = LoadDrops::default();
        let minutes = sweep(&self.minutes, MAX_LOAD_MINUTES, &mut drops, |m| {
            let lease = m.lease.as_ref()?;
            let broken = m.minute > TAIL_MINUTE
                || m.ws_max_bytes > MAX_MEMORY_BYTES
                || m.cpu_mean_millis > m.cpu_peak_window_millis;
            (!broken).then_some((lease.term, lease.seq, m.minute))
        });
        let mono = self.mono_usec;
        let clocks = sweep(&self.lease_clocks, MAX_LOAD_LEASES, &mut drops, |c| {
            let lease = c.lease.as_ref()?;
            (c.started_mono_usec <= mono).then_some((lease.term, lease.seq))
        });
        let covered = sweep(&self.covered, MAX_LOAD_LEASES, &mut drops, |id| {
            Some((id.term, id.seq))
        });
        Vetted {
            covered,
            clocks,
            minutes,
            drops,
        }
    }
}

/// Cuts `items` to `cap` (the excess is `over_cap`, never judged), then keeps each record
/// whose `key` is `Some` and not seen before. `None` is invalid; a seen key is a duplicate.
fn sweep<T: Clone, K: Ord>(
    items: &[T],
    cap: usize,
    drops: &mut LoadDrops,
    key: impl Fn(&T) -> Option<K>,
) -> Vec<T> {
    let (head, excess) = items.split_at(items.len().min(cap));
    drops.over_cap = drops.over_cap.saturating_add(count(excess.len()));
    let mut seen = BTreeSet::new();
    let mut kept = Vec::with_capacity(head.len());
    for item in head {
        match key(item) {
            None => drops.invalid = drops.invalid.saturating_add(1),
            Some(id) => {
                if seen.insert(id) {
                    kept.push(item.clone());
                } else {
                    drops.duplicate = drops.duplicate.saturating_add(1);
                }
            }
        }
    }
    kept
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
