//! Crash-at-point hooks: the server dies the moment it reaches a named point of a code
//! path, rather than at a time or a filesystem operation.
//!
//! A [`CrashPlan`] is generic over the point type so each path names its own points:
//! the direct write path's are [`StorePoint`], the local write tier's [`TierPoint`]
//! (docs/design/write-tier-scenarios.md lists the scenario each serves).

use std::fmt;

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
    /// (from 0). The crash is planted in the disk there; if the commit takes fewer
    /// operations it lands in a later commit, and is not recorded as this point.
    InCommit { after: u64 },
    /// `PutBlob` has committed; the client is not yet answered.
    BeforeReply,
}

/// A point of the local write tier's paths (docs/design/write-tier-scenarios.md): the
/// staged write (a blob written to the open block, synced, indexed with `PutStaged`,
/// acknowledged) and the flush (the sealed block PUT whole, `ObjectFlushed`
/// committed, the block renamed to a flushed copy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TierPoint {
    /// The blob's bytes are written to its range; the block is not synced (scenario 1).
    AfterAppend,
    /// The range is synced, its block's name too; `PutStaged` is not proposed
    /// (scenario 2).
    AfterSync,
    /// Part-way through the `PutStaged` commit, at its `after`-th filesystem operation
    /// of the raft directory (scenario 3), as [`StorePoint::InCommit`].
    InPutStaged { after: u64 },
    /// `PutStaged` has committed; the client is not yet answered (scenario 4).
    BeforeReply,
    /// The client has been answered; the block is not flushed (scenario 5).
    Acked,
    /// The store has the flushed object; `ObjectFlushed` is not proposed (scenarios
    /// 11 and 12).
    AfterFlushPut,
    /// Part-way through the `ObjectFlushed` commit, at its `after`-th filesystem
    /// operation (scenario 11).
    InFlushCommit { after: u64 },
    /// `ObjectFlushed` has committed; the block is not renamed to its flushed copy
    /// (scenario 21: the next start deletes the `.blk`).
    BeforeRename,
}

impl TierPoint {
    /// Every point, with `after` 0 for those part-way through a commit.
    pub const ALL: [Self; 8] = [
        Self::AfterAppend,
        Self::AfterSync,
        Self::InPutStaged { after: 0 },
        Self::BeforeReply,
        Self::Acked,
        Self::AfterFlushPut,
        Self::InFlushCommit { after: 0 },
        Self::BeforeRename,
    ];

    /// The point its [`fmt::Display`] form names, for a replay line.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let fixed = Self::ALL
            .into_iter()
            .filter(|p| p.in_commit().is_none())
            .find(|p| p.to_string() == text);
        if fixed.is_some() {
            return fixed;
        }
        if let Some(after) = text.strip_prefix("tier-in-put-staged-") {
            return Some(Self::InPutStaged {
                after: after.parse().ok()?,
            });
        }
        let after = text.strip_prefix("tier-in-flush-commit-")?.parse().ok()?;
        Some(Self::InFlushCommit { after })
    }

    /// The filesystem operation of the commit a crash part-way through it lands at;
    /// `None` for every other point.
    #[must_use]
    pub fn in_commit(self) -> Option<u64> {
        match self {
            Self::InPutStaged { after } | Self::InFlushCommit { after } => Some(after),
            Self::AfterAppend
            | Self::AfterSync
            | Self::BeforeReply
            | Self::Acked
            | Self::AfterFlushPut
            | Self::BeforeRename => None,
        }
    }
}

impl fmt::Display for TierPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AfterAppend => f.write_str("tier-after-append"),
            Self::AfterSync => f.write_str("tier-after-sync"),
            Self::InPutStaged { after } => write!(f, "tier-in-put-staged-{after}"),
            Self::BeforeReply => f.write_str("tier-before-reply"),
            Self::Acked => f.write_str("tier-acked"),
            Self::AfterFlushPut => f.write_str("tier-after-flush-put"),
            Self::InFlushCommit { after } => write!(f, "tier-in-flush-commit-{after}"),
            Self::BeforeRename => f.write_str("tier-before-rename"),
        }
    }
}

impl Down {
    /// The `Down` its [`fmt::Display`] form names, for a replay line.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text == "kill" {
            return Some(Self::Kill);
        }
        let (keep, names) = text.strip_prefix("cut-")?.split_once('-')?;
        Some(Self::PowerCut {
            keep: keep.parse().ok()?,
            names: names.parse().ok()?,
        })
    }
}

impl fmt::Display for Down {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kill => f.write_str("kill"),
            Self::PowerCut { keep, names } => write!(f, "cut-{keep}-{names}"),
        }
    }
}

impl StorePoint {
    /// The point its [`fmt::Display`] form names, for a replay line.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "put-sent" => Some(Self::PutSent),
            "before-commit" => Some(Self::BeforeCommit),
            "before-reply" => Some(Self::BeforeReply),
            _ => {
                let after = text.strip_prefix("in-commit-")?.parse().ok()?;
                Some(Self::InCommit { after })
            }
        }
    }

    /// The filesystem operation of the `PutBlob` commit an [`StorePoint::InCommit`]
    /// crash lands at; `None` for every other point.
    #[must_use]
    pub fn in_commit(self) -> Option<u64> {
        match self {
            Self::InCommit { after } => Some(after),
            Self::PutSent | Self::BeforeCommit | Self::BeforeReply => None,
        }
    }
}

impl fmt::Display for StorePoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PutSent => f.write_str("put-sent"),
            Self::BeforeCommit => f.write_str("before-commit"),
            Self::InCommit { after } => write!(f, "in-commit-{after}"),
            Self::BeforeReply => f.write_str("before-reply"),
        }
    }
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
        assert_eq!(StorePoint::InCommit { after: 2 }.in_commit(), Some(2));
        assert_eq!(StorePoint::BeforeReply.in_commit(), None);
    }

    /// Catches a point or a way down that a replay line names but cannot be read back
    /// (the replay would run another crash), and text read as one that names none.
    #[test]
    fn points_and_downs_read_back_from_their_names() {
        let points = [
            StorePoint::PutSent,
            StorePoint::BeforeCommit,
            StorePoint::InCommit { after: 3 },
            StorePoint::BeforeReply,
        ];
        for point in points {
            assert_eq!(StorePoint::parse(&point.to_string()), Some(point));
        }
        let cut = Down::PowerCut {
            keep: 17,
            names: u64::MAX,
        };
        for down in [Down::Kill, cut] {
            assert_eq!(Down::parse(&down.to_string()), Some(down));
        }
        for bad in ["", "in-commit-x", "after-reply"] {
            assert_eq!(StorePoint::parse(bad), None, "{bad}");
        }
        for bad in ["", "cut-1", "cut-x-1", "cut-1-x", "kil"] {
            assert_eq!(Down::parse(bad), None, "{bad}");
        }
    }

    /// Catches a tier point a replay line names but cannot be read back (the replay
    /// would run another crash), two points with one name, and an in-commit point
    /// whose operation is lost.
    #[test]
    fn tier_points_read_back_from_their_names() {
        let mut names = std::collections::BTreeSet::new();
        let deep = [
            TierPoint::InPutStaged { after: 7 },
            TierPoint::InFlushCommit { after: 9 },
        ];
        for point in TierPoint::ALL.into_iter().chain(deep) {
            assert_eq!(TierPoint::parse(&point.to_string()), Some(point));
            assert!(names.insert(point.to_string()), "{point} named twice");
        }
        assert_eq!(TierPoint::InFlushCommit { after: 9 }.in_commit(), Some(9));
        assert_eq!(TierPoint::InPutStaged { after: 7 }.in_commit(), Some(7));
        assert_eq!(TierPoint::Acked.in_commit(), None);
        for bad in [
            "",
            "tier-in-put-staged-x",
            "tier-in-flush-commit-",
            "put-sent",
        ] {
            assert_eq!(TierPoint::parse(bad), None, "{bad}");
        }
    }
}
