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

    /// The voter count to keep with `candidates` eligible candidates: the largest odd
    /// number not above the target or the candidates, and at least 1 (the leader).
    #[must_use]
    pub fn effective(&self, candidates: usize) -> usize {
        let n = candidates.min(self.target).max(1);
        if n.is_multiple_of(2) { n - 1 } else { n }
    }

    /// The most pressured voter that is not the leader and keeps its vote, and the
    /// calmest eligible learner, with their pressures, for [`LoadHysteresis::observe`].
    #[must_use]
    pub fn load_pair(
        &self,
        leader: ServerId,
        membership: &Membership,
        members: &[MemberState],
    ) -> Option<((ServerId, u32), (ServerId, u32))> {
        let view = View::new(self, leader, membership, members);
        let voter = view
            .good_voters()
            .filter(|s| s.id != leader)
            .max_by_key(|s| (s.pressure, s.id))?;
        let learner = view
            .eligible_learners()
            .min_by_key(|s| (s.pressure, s.lag, s.id))?;
        Some(((voter.id, voter.pressure), (learner.id, learner.pressure)))
    }

    /// The next change towards the voter count, or `None` when the membership is where
    /// it should be. In order:
    ///
    /// 1. Fewer voters keeping their vote than the effective count: promote the best
    ///    eligible learner (least lag, then calmest, then in a failure domain that holds
    ///    no voter, then lowest id). A failing voter is replaced this way first.
    /// 2. More voters than the effective count: a lost voter (unreachable, or not in
    ///    `members`) is removed; a drained, unhealthy or lagging one is demoted; then
    ///    the voter of `load`; then the most pressured other voter.
    /// 3. Otherwise `load`, if its voter still votes and its candidate is an eligible
    ///    learner: promote the candidate (step 2 then demotes the voter).
    ///
    /// The leader is never changed here: moving its vote needs a leadership transfer
    /// first.
    #[must_use]
    pub fn next_change(
        &self,
        leader: ServerId,
        membership: &Membership,
        members: &[MemberState],
        load: Option<LoadSwap>,
    ) -> Option<Change> {
        let view = View::new(self, leader, membership, members);
        let effective = self.effective(view.candidates());
        let voters = membership.voters().count();
        let failing: Vec<Change> = membership
            .voters()
            .filter_map(|v| view.failing(v))
            .collect();
        let good = voters - failing.len();
        if good < effective {
            // Fewer good voters than eligible candidates allow means an eligible
            // learner exists.
            return view.best_learner().map(Change::Promote);
        }
        if voters > effective {
            if let Some(&change) = failing.first() {
                return Some(change);
            }
            let swapped = load.filter(|w| w.voter != leader && membership.is_voter(w.voter));
            if let Some(swap) = swapped {
                return Some(Change::Demote(swap.voter));
            }
            let surplus = view
                .good_voters()
                .filter(|s| s.id != leader)
                .max_by_key(|s| (s.pressure, s.lag, s.id))?;
            return Some(Change::Demote(surplus.id));
        }
        let swap = load.filter(|w| {
            view.good_voters().any(|s| s.id == w.voter)
                && view.eligible_learners().any(|s| s.id == w.candidate)
        })?;
        Some(Change::Promote(swap.candidate))
    }
}

/// One call's view of the membership and the members' states.
struct View<'a> {
    policy: &'a VoterPolicy,
    leader: ServerId,
    membership: &'a Membership,
    members: &'a [MemberState],
}

impl<'a> View<'a> {
    fn new(
        policy: &'a VoterPolicy,
        leader: ServerId,
        membership: &'a Membership,
        members: &'a [MemberState],
    ) -> Self {
        Self {
            policy,
            leader,
            membership,
            members,
        }
    }

    fn state(&self, id: ServerId) -> Option<&'a MemberState> {
        self.members.iter().find(|m| m.id == id)
    }

    /// Not drained, reachable, healthy, and within the lag for the rounds.
    fn eligible(&self, s: &MemberState) -> bool {
        let t = self.policy.thresholds;
        !self.membership.is_drained(s.id)
            && s.reachable
            && s.healthy
            && s.lag <= t.promote_lag
            && s.rounds_within >= t.promote_rounds
    }

    /// How many members are eligible.
    fn candidates(&self) -> usize {
        let m = self.membership;
        m.voters()
            .chain(m.learners())
            .filter_map(|id| self.state(id))
            .filter(|s| self.eligible(s))
            .count()
    }

    /// The change that takes a failing voter's vote away, if it is failing: removal
    /// for a lost one (unreachable, or with no state), demotion for a drained,
    /// unhealthy or lagging one. The leader never fails here.
    fn failing(&self, voter: ServerId) -> Option<Change> {
        if voter == self.leader {
            return None;
        }
        let Some(s) = self.state(voter).filter(|s| s.reachable) else {
            return Some(Change::Remove(voter));
        };
        let lagging = s.rounds_beyond >= self.policy.thresholds.promote_rounds;
        (self.membership.is_drained(voter) || !s.healthy || lagging)
            .then_some(Change::Demote(voter))
    }

    /// The voters that keep their vote, with their states.
    fn good_voters(&self) -> impl Iterator<Item = &'a MemberState> + '_ {
        self.membership
            .voters()
            .filter(|&v| self.failing(v).is_none())
            .filter_map(|v| self.state(v))
    }

    fn eligible_learners(&self) -> impl Iterator<Item = &'a MemberState> + '_ {
        self.membership
            .learners()
            .filter_map(|id| self.state(id))
            .filter(|s| self.eligible(s))
    }

    /// The learner to promote: least lag, then calmest, then in a failure domain that
    /// holds no voter yet (an unknown domain is a domain of its own), then lowest id.
    fn best_learner(&self) -> Option<ServerId> {
        let voter_domains: Vec<u64> = self
            .membership
            .voters()
            .filter_map(|v| self.state(v).and_then(|s| s.domain))
            .collect();
        let shared = |s: &MemberState| s.domain.is_some_and(|d| voter_domains.contains(&d));
        self.eligible_learners()
            .min_by_key(|s| (s.lag, s.pressure, shared(s), s.id))
            .map(|s| s.id)
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
    /// The pair over the margin, and for how many consecutive rounds.
    streak: Option<(LoadSwap, u32)>,
    /// Rounds since the last load-driven change finished.
    since: u32,
    in_flight: bool,
}

impl LoadHysteresis {
    /// A band of `margin` held for `dwell` rounds, `gap` rounds apart.
    #[must_use]
    pub fn new(margin: u32, dwell: u32, gap: u32) -> Self {
        Self {
            margin,
            dwell,
            gap,
            streak: None,
            since: u32::MAX,
            in_flight: false,
        }
    }

    /// One round's reading: the voter and the candidate [`VoterPolicy::load_pair`]
    /// named, with their pressures (or no pair). Returns the swap to make, if any; it
    /// is then in flight until [`LoadHysteresis::finished`].
    pub fn observe(
        &mut self,
        pair: Option<((ServerId, u32), (ServerId, u32))>,
    ) -> Option<LoadSwap> {
        self.since = self.since.saturating_add(1);
        let over = pair.filter(|((_, v), (_, c))| *v >= c.saturating_add(self.margin));
        let Some(((voter, _), (candidate, _))) = over else {
            self.streak = None;
            return None;
        };
        let swap = LoadSwap { voter, candidate };
        let rounds = match self.streak {
            Some((same, n)) if same == swap => n + 1,
            _ => 1,
        };
        self.streak = Some((swap, rounds));
        if self.in_flight || rounds < self.dwell || self.since <= self.gap {
            return None;
        }
        self.in_flight = true;
        self.streak = None;
        Some(swap)
    }

    /// The swap in flight has committed (or was abandoned): the gap starts.
    pub fn finished(&mut self) {
        self.in_flight = false;
        self.since = 0;
    }
}
