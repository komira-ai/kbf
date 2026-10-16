//! The voter policy (`VoterPolicy`) and the load hysteresis (`LoadHysteresis`), as pure
//! functions: a membership, what the leader knows of each member, and the one change
//! the policy names next. `converge` applies changes until the policy names none, as a
//! leader does one committed change at a time.

use kbf_raft::{
    Change, ChangeError, LoadHysteresis, LoadSwap, MemberState, Membership, ServerId, Thresholds,
    VoterPolicy, VoterPolicyError,
};

const LEADER: ServerId = ServerId(1);

fn s(n: u64) -> ServerId {
    ServerId(n)
}

/// A member that is reachable, healthy and caught up for long enough.
fn ok(n: u64) -> MemberState {
    MemberState {
        id: s(n),
        reachable: true,
        healthy: true,
        lag: 0,
        rounds_within: 5,
        rounds_beyond: 0,
        pressure: 0,
        domain: None,
    }
}

fn policy(target: usize) -> VoterPolicy {
    VoterPolicy::new(target, Thresholds::default()).expect("an odd target")
}

/// The bootstrap voter `LEADER` and learners `2..=n`.
fn joined(n: u64) -> Membership {
    Membership::new([LEADER], (2..=n).map(s)).expect("valid")
}

fn voters(m: &Membership) -> Vec<u64> {
    m.voters().map(|v| v.0).collect()
}

fn learners(m: &Membership) -> Vec<u64> {
    m.learners().map(|v| v.0).collect()
}

/// Applies the policy's changes until it names none; returns the changes made.
fn converge(
    policy: &VoterPolicy,
    m: &mut Membership,
    members: &[MemberState],
    load: Option<LoadSwap>,
) -> Vec<Change> {
    let mut made = Vec::new();
    for _ in 0..20 {
        let Some(change) = policy.next_change(LEADER, m, members, load) else {
            return made;
        };
        *m = m.changed(change).expect("the policy names a valid change");
        made.push(change);
    }
    panic!("no fixed point after 20 changes: {made:?}");
}

/// Catches: an even voter count, a count above the target, and a count below what the
/// candidates allow. 1 or 2 members: 1 voter; 3: 3; 4: 3 and a learner; 5 with target
/// 3: 3 and two learners; 5 with target 5: 5.
#[test]
fn the_voter_count_is_the_largest_odd_number_within_the_target_and_the_members() {
    let cases: [(u64, usize, &[u64], &[u64]); 6] = [
        (1, 3, &[1], &[]),
        (2, 3, &[1], &[2]),
        (3, 3, &[1, 2, 3], &[]),
        (4, 3, &[1, 2, 3], &[4]),
        (5, 3, &[1, 2, 3], &[4, 5]),
        (5, 5, &[1, 2, 3, 4, 5], &[]),
    ];
    for (n, target, want_voters, want_learners) in cases {
        let policy = policy(target);
        let members: Vec<MemberState> = (1..=n).map(ok).collect();
        let mut m = joined(n);
        let changes = converge(&policy, &mut m, &members, None);
        assert_eq!(voters(&m), want_voters, "{n} members, target {target}");
        assert_eq!(learners(&m), want_learners, "{n} members, target {target}");
        assert!(
            changes.iter().all(|c| matches!(c, Change::Promote(_))),
            "growth only promotes: {changes:?}"
        );
    }
    assert_eq!(
        [1, 2, 3, 4, 5, 6, 9].map(|c| policy(3).effective(c)),
        [1, 1, 3, 3, 3, 3, 3]
    );
    assert_eq!(policy(5).effective(4), 3);
    assert_eq!(policy(3).effective(0), 1, "the leader always votes");
    for bad in [0, 2, 4, 8, 9] {
        assert_eq!(
            VoterPolicy::new(bad, Thresholds::default()),
            Err(VoterPolicyError(bad))
        );
    }
    assert_eq!(VoterPolicy::default(), policy(3));
    assert_eq!(policy(5).target(), 5);
}

/// Catches: a drained member chosen as a voter, a drained voter that keeps its vote,
/// and a vote moved by demoting first (the count would drop to an even number, and the
/// quorum shrink, before the replacement holds the log). A drained voter's vote moves:
/// first the replacement is promoted, then the drained voter is demoted (it stays a
/// member).
#[test]
fn a_drained_member_is_never_a_voter_and_a_drained_voter_hands_its_vote_over() {
    let members: Vec<MemberState> = (1..=4).map(ok).collect();
    let mut m = joined(4).with_drained([s(2)]).expect("a member");
    converge(&policy(3), &mut m, &members, None);
    assert_eq!(voters(&m), [1, 3, 4]);
    assert_eq!(learners(&m), [2]);

    let mut m = Membership::new([s(1), s(2), s(3)], [s(4)])
        .expect("valid")
        .with_drained([s(3)])
        .expect("a member");
    let changes = converge(&policy(3), &mut m, &members, None);
    assert_eq!(changes, [Change::Promote(s(4)), Change::Demote(s(3))]);
    assert_eq!(voters(&m), [1, 2, 4]);

    // Three members, one drained: two candidates, so one voter.
    let members: Vec<MemberState> = (1..=3).map(ok).collect();
    let mut m = joined(3).with_drained([s(3)]).expect("a member");
    converge(&policy(3), &mut m, &members, None);
    assert_eq!(voters(&m), [1]);
}

/// Catches: a lost voter that is never replaced, one removed before its replacement is
/// promoted, and a lost voter demoted (kept as a member) rather than removed.
#[test]
fn a_lost_voter_is_replaced_by_a_learner_first_and_then_removed() {
    let mut members: Vec<MemberState> = (1..=4).map(ok).collect();
    members[1].reachable = false;
    let mut m = Membership::new([s(1), s(2), s(3)], [s(4)]).expect("valid");
    let changes = converge(&policy(3), &mut m, &members, None);
    assert_eq!(changes, [Change::Promote(s(4)), Change::Remove(s(2))]);
    assert_eq!(voters(&m), [1, 3, 4]);
    assert!(!m.contains(s(2)));
}

/// Catches: a lagging or unhealthy learner promoted, a promotion before the lag has held
/// for the rounds, a voter that stays behind or unhealthy kept, and a voter replaced for
/// a lag that has not lasted the rounds. The bounds are the thresholds, exactly.
#[test]
fn lag_and_health_decide_eligibility_at_the_thresholds() {
    let thresholds = Thresholds {
        promote_lag: 100,
        promote_rounds: 3,
    };
    let policy = VoterPolicy::new(3, thresholds).expect("odd");
    // Voter 2 is lost; learner 4 replaces it only if eligible. If not, two candidates
    // remain, so the count falls to one voter, starting with the lost one.
    let base = Membership::new([s(1), s(2), s(3)], [s(4)]).expect("valid");
    let with = |f: fn(&mut MemberState)| {
        let mut members: Vec<MemberState> = (1..=4).map(ok).collect();
        members[1].reachable = false;
        f(&mut members[3]);
        policy.next_change(LEADER, &base, &members, None)
    };
    let promoted = Some(Change::Promote(s(4)));
    let shrunk = Some(Change::Remove(s(2)));
    assert_eq!(with(|_| {}), promoted);
    assert_eq!(with(|c| c.lag = 100), promoted, "at the lag bound");
    assert_eq!(with(|c| c.lag = 101), shrunk, "beyond the lag");
    assert_eq!(with(|c| c.rounds_within = 2), shrunk, "not for long enough");
    assert_eq!(with(|c| c.rounds_within = 3), promoted, "for the rounds");
    assert_eq!(with(|c| c.healthy = false), shrunk, "unhealthy");
    assert_eq!(with(|c| c.reachable = false), shrunk, "unreachable");

    // A voter beyond the lag for the rounds is replaced, then demoted; one that has
    // been behind for fewer rounds is kept.
    let three = Membership::new([s(1), s(2), s(3)], [s(4)]).expect("valid");
    let mut members: Vec<MemberState> = (1..=4).map(ok).collect();
    members[2].lag = 500;
    members[2].rounds_within = 0;
    members[2].rounds_beyond = 2;
    assert_eq!(policy.next_change(LEADER, &three, &members, None), None);
    members[2].rounds_beyond = 3;
    let mut m = three.clone();
    let changes = converge(&policy, &mut m, &members, None);
    assert_eq!(changes, [Change::Promote(s(4)), Change::Demote(s(3))]);

    let mut members: Vec<MemberState> = (1..=4).map(ok).collect();
    members[1].healthy = false;
    let mut m = three;
    let changes = converge(&policy, &mut m, &members, None);
    assert_eq!(changes, [Change::Promote(s(4)), Change::Demote(s(2))]);
}

/// Catches: a promotion that is not the most caught-up candidate, one that ignores
/// load among equally caught-up candidates, and one that ignores failure domains among
/// otherwise equal candidates (ties then go to the lowest id).
#[test]
fn the_candidate_promoted_is_the_most_caught_up_then_calmest_then_in_a_new_domain() {
    let m = Membership::new([s(1), s(2)], [s(3), s(4), s(5)]).expect("valid");
    let pick = |members: &[MemberState]| policy(3).next_change(LEADER, &m, members, None);
    let mut members: Vec<MemberState> = (1..=5).map(ok).collect();
    members[2].lag = 7;
    members[3].lag = 3;
    members[4].lag = 9;
    assert_eq!(pick(&members), Some(Change::Promote(s(4))), "least lag");

    let mut members: Vec<MemberState> = (1..=5).map(ok).collect();
    members[2].pressure = 40;
    members[3].pressure = 10;
    members[4].pressure = 30;
    assert_eq!(pick(&members), Some(Change::Promote(s(4))), "calmest");

    let mut members: Vec<MemberState> = (1..=5).map(ok).collect();
    for (i, domain) in [(0, 1), (1, 2), (2, 1), (3, 2), (4, 3)] {
        members[i].domain = Some(domain);
    }
    assert_eq!(pick(&members), Some(Change::Promote(s(5))), "a new domain");

    let members: Vec<MemberState> = (1..=5).map(ok).collect();
    assert_eq!(pick(&members), Some(Change::Promote(s(3))), "lowest id");
}

/// Catches: a target lowered with no voters taken away, the leader demoted by the
/// policy (it would need a leadership transfer first), and a change made when nothing
/// needs one.
#[test]
fn surplus_voters_are_demoted_one_at_a_time_never_the_leader() {
    let mut m = Membership::new((1..=5).map(s), []).expect("valid");
    let mut members: Vec<MemberState> = (1..=5).map(ok).collect();
    members[0].pressure = 90;
    members[3].pressure = 50;
    members[4].pressure = 20;
    let changes = converge(&policy(3), &mut m, &members, None);
    assert_eq!(changes, [Change::Demote(s(4)), Change::Demote(s(5))]);
    assert_eq!(voters(&m), [1, 2, 3]);
    assert_eq!(converge(&policy(3), &mut m, &members, None), []);
}

/// Catches: a swap proposed on a single reading, a band that does not reset when the
/// margin is lost (oscillating pressure would add up to a swap), a second swap while one
/// is in flight or before the gap, and a swap that the policy does not carry out as
/// promote-then-demote. Load never overrides lag: a lagging candidate is not taken.
#[test]
fn load_swaps_need_the_margin_held_for_the_dwell_one_at_a_time() {
    let mut h = LoadHysteresis::new(20, 3, 4);
    let hot = |v: u32, c: u32| Some(((s(2), v), (s(4), c)));
    // Oscillating around the margin: never three rounds in a row.
    for round in 0..12 {
        let voter = if round % 3 == 2 { 25 } else { 50 };
        assert_eq!(h.observe(hot(voter, 10)), None, "round {round}");
    }
    let mut h = LoadHysteresis::new(20, 3, 4);
    assert_eq!(h.observe(hot(30, 10)), None);
    assert_eq!(h.observe(hot(30, 10)), None);
    let swap = h.observe(hot(30, 10));
    assert_eq!(
        swap,
        Some(LoadSwap {
            voter: s(2),
            candidate: s(4)
        })
    );
    for _ in 0..5 {
        assert_eq!(h.observe(hot(90, 0)), None, "one in flight");
    }
    h.finished();
    for round in 0..4 {
        assert_eq!(h.observe(hot(90, 0)), None, "gap round {round}");
    }
    assert!(
        h.observe(hot(90, 0)).is_some(),
        "after the gap and the dwell"
    );
    assert_eq!(h.observe(None), None);

    // A different pair restarts the dwell.
    let mut h = LoadHysteresis::new(20, 2, 0);
    assert_eq!(h.observe(hot(30, 0)), None);
    assert_eq!(h.observe(Some(((s(3), 30), (s(4), 0)))), None);
    assert!(h.observe(Some(((s(3), 30), (s(4), 0)))).is_some());

    // The policy carries out the swap: promote the candidate, then demote the voter.
    let mut members: Vec<MemberState> = (1..=4).map(ok).collect();
    members[1].pressure = 80;
    members[3].pressure = 5;
    let mut m = Membership::new([s(1), s(2), s(3)], [s(4)]).expect("valid");
    let pair = policy(3).load_pair(LEADER, &m, &members);
    assert_eq!(pair, Some(((s(2), 80), (s(4), 5))));
    let changes = converge(&policy(3), &mut m, &members, swap);
    assert_eq!(changes, [Change::Promote(s(4)), Change::Demote(s(2))]);
    members[3].lag = 5000;
    members[3].rounds_within = 0;
    let m = Membership::new([s(1), s(2), s(3)], [s(4)]).expect("valid");
    assert_eq!(policy(3).load_pair(LEADER, &m, &members), None);
    assert_eq!(policy(3).next_change(LEADER, &m, &members, swap), None);
}

/// Catches: a demotion of a learner or of the last voter, a drain of a stranger, of a
/// member drained already or of the only voter, an undrain of a member not drained,
/// and a removal that leaves a drain mark behind (a rejoin under the same id would
/// come back drained).
#[test]
fn demote_drain_and_undrain_check_their_preconditions() {
    let one = Membership::new([s(1)], [s(2)]).expect("valid");
    let two = Membership::new([s(1), s(3)], [s(2)]).expect("valid");
    let refusals = [
        (&one, Change::Demote(s(2)), ChangeError::NotAVoter(s(2))),
        (&one, Change::Demote(s(1)), ChangeError::LastVoter(s(1))),
        (&one, Change::Drain(s(9)), ChangeError::NotAMember(s(9))),
        (&one, Change::Drain(s(1)), ChangeError::LastVoter(s(1))),
        (&one, Change::Undrain(s(2)), ChangeError::NotDrained(s(2))),
    ];
    for (m, change, refused) in refusals {
        assert_eq!(m.changed(change), Err(refused), "{change:?}");
    }
    let demoted = two.changed(Change::Demote(s(3))).expect("two voters");
    assert_eq!(
        (voters(&demoted), learners(&demoted)),
        (vec![1], vec![2, 3])
    );
    let drained = two
        .changed(Change::Drain(s(1)))
        .expect("another voter remains");
    assert!(drained.is_drained(s(1)));
    assert_eq!(
        drained.changed(Change::Drain(s(1))),
        Err(ChangeError::AlreadyDrained(s(1)))
    );
    let learner = two.changed(Change::Drain(s(2))).expect("a learner");
    let removed = learner.changed(Change::Remove(s(2))).expect("removed");
    assert_eq!(removed.drained().count(), 0);
    let undrained = learner.changed(Change::Undrain(s(2))).expect("undrained");
    assert_eq!(undrained, two);
}
