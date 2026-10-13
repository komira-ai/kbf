//! Crash-at-point hooks: the server dies the moment it reaches a named point of a code
//! path, rather than at a time or a filesystem operation.
//!
//! A [`CrashPlan`] is generic over the point type so each path names its own points:
//! the direct write path's are [`StorePoint`]; the local write tier (rollout stage 4 of
//! docs/design/write-path.md) adds its own enum (after the append, after the sync,
//! after `PutStaged`, after the flush PUT, after `ObjectFlushed`, before the rename)
//! and arms it the same way. docs/design/write-tier-scenarios.md lists the points each
//! scenario needs.

/// How the server goes down at a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Down {
    /// The process is killed: everything it wrote stays readable.
    Kill,
    /// The machine loses power: only what was synced survives, with up to `keep`
    /// bytes of each file's unsynced tail and the name changes `names` selects
    /// ([`kbf_store::FaultFs::crash_reordered`]).
    PowerCut { keep: usize, names: u64 },
}

/// A point of the server's direct write path (an upload stored in the object store,
/// then indexed with `PutBlob`, then acknowledged).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorePoint {
    /// The PUT has been sent; the store has not answered.
    PutSent,
    /// The store has answered the PUT; `PutBlob` is not yet proposed.
    BeforeCommit,
    /// Part-way through the `PutBlob` commit: at its `after`-th filesystem operation
    /// (or a later commit's, when this one takes fewer).
    InCommit { after: u64 },
    /// `PutBlob` has committed; the client is not yet answered.
    BeforeReply,
}

/// Which points take the server down, each the `nth` time (from 0) it is reached once
/// armed. A plan outlives the process: a point not yet reached stays armed across a
/// restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashPlan<P> {
    armed: Vec<Armed<P>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Armed<P> {
    point: P,
    nth: u32,
    down: Down,
}

impl<P> Default for CrashPlan<P> {
    fn default() -> Self {
        Self { armed: Vec::new() }
    }
}

impl<P: Copy + PartialEq> CrashPlan<P> {
    /// Arms `point`: the `nth` time it is reached from now, the server goes `down`.
    pub fn arm(&mut self, point: P, nth: u32, down: Down) {
        self.armed.push(Armed { point, nth, down });
    }

    /// The server has reached `point`: how it goes down there, if an armed point fires.
    /// A fired point is disarmed; every other armed `point` counts one more reach.
    pub fn reached(&mut self, point: P) -> Option<(P, Down)> {
        let mut fired = None;
        self.armed.retain_mut(|a| {
            if a.point != point {
                return true;
            }
            if a.nth > 0 {
                a.nth -= 1;
                return true;
            }
            fired.get_or_insert((a.point, a.down));
            false
        });
        fired
    }

    /// The points armed, for a point that must act before it is reached (a crash
    /// part-way through a commit is armed in the disk first).
    pub fn peek(&self) -> impl Iterator<Item = (P, u32, Down)> + '_ {
        self.armed.iter().map(|a| (a.point, a.nth, a.down))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUT: Down = Down::PowerCut { keep: 0, names: 0 };

    /// Catches a point that fires on the wrong reach (off by one on `nth`), fires on
    /// another point, fires twice, or does not count down the points it skips.
    #[test]
    fn a_point_fires_on_its_nth_reach_once() {
        let mut plan = CrashPlan::default();
        plan.arm(StorePoint::BeforeCommit, 1, Down::Kill);
        plan.arm(StorePoint::BeforeReply, 0, CUT);
        assert_eq!(plan.reached(StorePoint::PutSent), None);
        assert_eq!(plan.reached(StorePoint::BeforeCommit), None);
        assert_eq!(
            plan.reached(StorePoint::BeforeCommit),
            Some((StorePoint::BeforeCommit, Down::Kill))
        );
        assert_eq!(plan.reached(StorePoint::BeforeCommit), None);
        assert_eq!(plan.peek().count(), 1);
        assert_eq!(
            plan.reached(StorePoint::BeforeReply),
            Some((StorePoint::BeforeReply, CUT))
        );
        assert_eq!(plan.peek().count(), 0);
    }
}
