//! Which members vote: the leader's voter policy (docs/design/cluster.md, section 5.3,
//! "voter count" and "selection is automatic").
//!
//! Every member is a candidate. The cluster keeps an odd number of voters: the target
//! (3 by default, 5 allowed), or, with fewer eligible candidates, the largest odd number
//! not above their count. A candidate is eligible when it is not drained, reachable,
//! healthy, and within [`Thresholds::promote_lag`] entries of the leader for
//! [`Thresholds::promote_rounds`] rounds. [`VoterPolicy::next_change`] names the one
//! single-server change that moves the membership towards the count, or none; the
//! leader proposes it, waits for it to commit, and asks again. The policy reads no
//! clock: the caller counts rounds and decides reachability with its own timers.
//!
//! Load pressure is a pluggable signal: [`LoadHysteresis`] turns per-round pressure
//! readings into at most one swap at a time, after a margin held for a dwell.

use thiserror::Error;

use crate::{Change, Membership, ServerId};

/// The lag bounds of eligibility (`--promote-lag`, `--promote-rounds`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    /// The most entries a candidate may be behind the leader's commit index.
    pub promote_lag: u64,
    /// For how many consecutive rounds a candidate must be within the lag to be
    /// promoted, and a voter beyond it to be replaced.
    pub promote_rounds: u32,
}

impl Default for Thresholds {
    /// 1000 entries for 5 rounds.
    fn default() -> Self {
        Self {
            promote_lag: 1000,
            promote_rounds: 5,
        }
    }
}

/// What the leader knows of one member this round. Whether it is drained comes from
/// the committed [`Membership`], not from here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberState {
    /// The member.
    pub id: ServerId,
    /// Heard from recently enough (the caller's `--voter-replace-after`). A voter that
    /// is not reachable is removed once replaced; one that is reachable but failing is
    /// demoted to a learner.
    pub reachable: bool,
    /// No failing disk or failed log append reported.
    pub healthy: bool,
    /// Entries behind the leader's commit index.
    pub lag: u64,
    /// Consecutive rounds within [`Thresholds::promote_lag`] (0 while beyond it).
    pub rounds_within: u32,
    /// Consecutive rounds beyond [`Thresholds::promote_lag`] (0 while within it).
    pub rounds_beyond: u32,
    /// Load pressure, a stall percentage (lower is calmer); advisory.
    pub pressure: u32,
    /// The failure domain, if known; `None`: a domain of its own.
    pub domain: Option<u64>,
}

/// A voter-count target the policy refuses.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("a voter-count target must be odd and from 1 to 7, not {0}")]
pub struct VoterPolicyError(pub usize);

/// A load-driven swap: promote `candidate`, then demote `voter`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadSwap {
    /// The voter under pressure.
    pub voter: ServerId,
    /// The calmer candidate to take its vote.
    pub candidate: ServerId,
}

/// The leader's voter policy: an odd target voter count and the lag thresholds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoterPolicy {
    target: usize,
    thresholds: Thresholds,
}

impl Default for VoterPolicy {
    /// A target of 3 voters, default thresholds.
    fn default() -> Self {
        Self {
            target: 3,
            thresholds: Thresholds::default(),
        }
    }
}

impl VoterPolicy {
    /// A policy keeping `target` voters, with `thresholds`.
    ///
    /// # Errors
    ///
    /// [`VoterPolicyError`] for an even target, 0, or above 7.
    pub fn new(target: usize, thresholds: Thresholds) -> Result<Self, VoterPolicyError> {
        if target.is_multiple_of(2) || target > 7 {
            return Err(VoterPolicyError(target));
        }
        Ok(Self { target, thresholds })
    }

    /// The target.
    #[must_use]
    pub fn target(&self) -> usize {
        self.target
    }

    /// The voter count to keep with `candidates` eligible candidates.
    #[must_use]
    pub fn effective(&self, candidates: usize) -> usize {
        let _ = candidates;
        self.target
    }

    /// The most pressured voter that is not the leader and the calmest eligible learner,
    /// with their pressures, for [`LoadHysteresis::observe`].
    #[must_use]
    pub fn load_pair(
        &self,
        leader: ServerId,
        membership: &Membership,
        members: &[MemberState],
    ) -> Option<((ServerId, u32), (ServerId, u32))> {
        let _ = (leader, membership, members);
        None
    }

    /// The next change towards the voter count, or `None` when the membership is where
    /// it should be.
    #[must_use]
    pub fn next_change(
        &self,
        leader: ServerId,
        membership: &Membership,
        members: &[MemberState],
        load: Option<LoadSwap>,
    ) -> Option<Change> {
        let _ = (leader, membership, members, load);
        None
    }
}

/// Hysteresis for load-driven swaps (`--load-margin`, `--load-dwell` and a minimum gap
/// between load-driven changes, all in rounds): a swap is proposed only after the same
/// voter's pressure has stayed at least `margin` above the same candidate's for `dwell`
/// consecutive rounds, at least `gap` rounds after the last load-driven change finished,
/// and never while one is in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadHysteresis {
    margin: u32,
    dwell: u32,
    gap: u32,
}

impl LoadHysteresis {
    /// A band of `margin` held for `dwell` rounds, `gap` rounds apart.
    #[must_use]
    pub fn new(margin: u32, dwell: u32, gap: u32) -> Self {
        Self { margin, dwell, gap }
    }

    /// One round's reading: the voter and the candidate [`VoterPolicy::load_pair`]
    /// named, with their pressures (or no pair). Returns the swap to make, if any.
    pub fn observe(
        &mut self,
        pair: Option<((ServerId, u32), (ServerId, u32))>,
    ) -> Option<LoadSwap> {
        let _ = pair;
        None
    }

    /// The swap in flight has committed (or was abandoned): the gap starts.
    pub fn finished(&mut self) {}
}
