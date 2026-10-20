//! What a frontend knows of a joiner's fill (`docs/design/rebalance-handoff.md`, sections 3
//! and 4): the settings of automatic promotion, what a node reports at a heartbeat
//! ([`Fill`]), the body of the `unfilled/<id>` mark ([`Unfilled`]) and the reasons a
//! joiner is not promoted yet ([`Block`]). The rules live in `fence` (the sweeper's side)
//! and `promote` (the joiner's own beat).
//!
//! **The mark.** `unfilled/<id>` was an empty presence mark. It now holds the joiner's
//! phase: *flushing* until every member that can flush has flushed what it staged before
//! the join (`gen`, the generation of the map that first held the joiner), then
//! *adopting* from the epoch the frontend opened (`epoch`, `meta/tail_epoch`), with the
//! placement fingerprint (`fp`) and the boots of the write owners at that moment
//! (`fence_boots`). An empty body, which is what a frontend that predates this wrote, is a
//! joiner in the first phase whose generation is not yet set. A frontend that predates it
//! reads any body as "unfilled", as it always did.

use std::fmt;
use std::time::Duration;

use kbf_map::{Map, MemberId, MemberState};

/// The settings of automatic promotion (`--auto-promote` and its companions). Without a
/// [`Handoff`] a [`super::Members`] behaves as before it existed: a joiner is promoted by
/// an operator's `POST` only, and its mark stays empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handoff {
    /// How long all the conditions of a joiner's fill must keep holding, as the joiner's
    /// own beats see them, before it is promoted (`--handoff-settle-secs`).
    pub settle: Duration,
    /// Whether a farm that had a member `Down` during the handoff may promote a joiner
    /// (`--handoff-allow-degraded`). A `Down` member still inside its dwell blocks the
    /// first phase whatever this says.
    pub allow_degraded: bool,
}

impl Default for Handoff {
    fn default() -> Self {
        Self {
            settle: Duration::from_secs(30),
            allow_degraded: false,
        }
    }
}

/// What a node reports of its flush and its tail at a heartbeat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fill {
    /// The highest map generation `g` such that every object the node was to flush that it
    /// staged before it held map `g` is flushed (`flush_clear_gen`); `None`: it cannot say
    /// (an older node, or one with no flush gate).
    pub flush_clear_gen: Option<u64>,
    /// The highest tail epoch for which the node finished a pass (`tail_epoch_done`); 0:
    /// none, which is also what a restarted node says.
    pub tail_epoch_done: u64,
    /// The placement fingerprint of the map the pass began under, the node assumed
    /// `Active` (`fill_fp`).
    pub fill_fp: u64,
}

impl Fill {
    /// The wire's `flush_known`, `flush_clear_gen`, `tail_epoch_done` and `fill_fp`. A
    /// generation sent with `flush_known` false is not a generation.
    #[must_use]
    pub const fn from_wire(
        flush_known: bool,
        flush_clear_gen: u64,
        tail_epoch_done: u64,
        fill_fp: u64,
    ) -> Self {
        Self {
            flush_clear_gen: if flush_known {
                Some(flush_clear_gen)
            } else {
                None
            },
            tail_epoch_done,
            fill_fp,
        }
    }

    /// The wire's `flush_known` and `flush_clear_gen`.
    #[must_use]
    pub const fn flush_to_wire(self) -> (bool, u64) {
        match self.flush_clear_gen {
            Some(generation) => (true, generation),
            None => (false, 0),
        }
    }
}

/// The placement fingerprint of `map` with `id` assumed `Active`: what the map will place
/// once the joiner is promoted, and the value a node reports as [`Fill::fill_fp`] and the
/// frontend compares it with. `None` if `id` is not in the map.
#[must_use]
pub fn fill_fp(map: &Map, id: MemberId) -> Option<u64> {
    map.assuming(id, MemberState::Active)
        .map(|assumed| assumed.placement_fingerprint())
}

/// One write owner's boot at the moment the fence opened.
#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(super) struct FenceBoot {
    #[prost(uint64, tag = "1")]
    pub(super) member: u64,
    #[prost(uint64, tag = "2")]
    pub(super) boot: u64,
}

/// The body of an `unfilled/<id>` mark.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub(super) struct Unfilled {
    /// The generation of the map that first held the joiner: the barrier of the flush
    /// phase. 0: not set yet.
    #[prost(uint64, tag = "1")]
    pub(super) generation: u64,
    /// The placement fingerprint of the current map with the joiner assumed `Active`, as of
    /// the epoch.
    #[prost(uint64, tag = "2")]
    pub(super) fp: u64,
    /// 0: flushing; 1: adopting.
    #[prost(uint32, tag = "3")]
    pub(super) phase: u32,
    /// The tail epoch the joiner's pass is acknowledged against (adopting only).
    #[prost(uint64, tag = "4")]
    pub(super) epoch: u64,
    /// The write owners' boots when the epoch was opened, by member id.
    #[prost(message, repeated, tag = "5")]
    pub(super) fence_boots: Vec<FenceBoot>,
    /// Some member was `Down`, or left the map, during the handoff.
    #[prost(bool, tag = "6")]
    pub(super) degraded: bool,
    /// The store's clock, in nanoseconds, when every condition of the joiner's own beat
    /// first held and has held since; 0: they do not hold now.
    #[prost(uint64, tag = "7")]
    pub(super) settled_nanos: u64,
}

/// The phase a mark is in.
pub(super) const FLUSHING: u32 = 0;
/// See [`Unfilled::phase`].
pub(super) const ADOPTING: u32 = 1;

impl Unfilled {
    /// The boot recorded for `member` when the epoch was opened.
    pub(super) fn fenced_boot(&self, member: MemberId) -> Option<u64> {
        self.fence_boots
            .iter()
            .find(|f| f.member == member.0)
            .map(|f| f.boot)
    }
}

/// Why a joiner is not promoted yet. The first of them says what to look at; `Display` is
/// what the operator is shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// A member has not flushed the objects it staged before the join: it reports that it
    /// cannot say, or a generation below the join's.
    Flushing {
        /// The member.
        member: MemberId,
        /// What it reports (`None`: it cannot say, or its report is not fresh).
        have: Option<u64>,
        /// The generation the join needs.
        need: u64,
    },
    /// A member is `Down` and still inside its dwell.
    Down(MemberId),
    /// A member was `Down`, or left the map, while the joiner filled.
    Degraded,
    /// The joiner's index is being rebuilt.
    Rebuilding,
    /// The joiner has not acknowledged the tail epoch.
    TailPending {
        /// The epoch it acknowledged (0: none).
        done: u64,
        /// The epoch it must reach.
        need: u64,
    },
    /// The joiner's acknowledgement is not of the boot the fence recorded for it (it
    /// restarted, or registered after the fence): the frontend opens a new epoch.
    Boot {
        /// The boot on its row.
        have: u64,
        /// The boot the fence recorded; `None` if it recorded none.
        fenced: Option<u64>,
    },
    /// The joiner's pass began under another placement than the map has now.
    Fingerprint,
    /// All hold; the settle time has not passed.
    Settling(Duration),
}

impl fmt::Display for Block {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Flushing {
                member,
                have: Some(have),
                need,
            } => write!(
                f,
                "member {} has flushed what it staged before map {have}; the join needs map {need}",
                member.0
            ),
            Self::Flushing {
                member,
                have: None,
                need,
            } => write!(
                f,
                "member {} has not reported its flush (the join needs map {need})",
                member.0
            ),
            Self::Down(member) => write!(f, "member {} is down, inside its dwell", member.0),
            Self::Degraded => f.write_str("a member was down or left while this joiner filled"),
            Self::Rebuilding => f.write_str("the index rebuild has not finished"),
            Self::TailPending { done, need } => {
                write!(f, "tail epoch {need} is not acknowledged (done {done})")
            }
            Self::Boot {
                have,
                fenced: Some(fenced),
            } => write!(
                f,
                "the acknowledgement is of boot {have}; the fence recorded boot {fenced}"
            ),
            Self::Boot { have, fenced: None } => write!(
                f,
                "boot {have} registered after the fence; a new epoch is to be opened"
            ),
            Self::Fingerprint => f.write_str("the pass began under a different placement"),
            Self::Settling(left) => {
                write!(
                    f,
                    "all conditions hold; settling for {}s more",
                    left.as_secs()
                )
            }
        }
    }
}
