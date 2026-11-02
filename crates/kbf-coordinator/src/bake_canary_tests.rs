//! `bake_beats` and `canary`: a host that has updated holds its slot until it has beaten
//! healthy enough times, `promotion_pending` holds the slot and is not up, and a canary
//! gate lets the first hosts go alone. Every time is explicit; the fleets are built beat by
//! beat.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::HostRole;
use kbf_updater::wire::Reply;

use crate::converge::{State, converge_kbf};
use crate::decide::{Decision, Hold, decide_in, decide_why};
use crate::desired::{Desired, KBF};
use crate::fleet::Fleet;
use crate::identity::Identity;
use crate::services::Services;
use crate::testutil::{Rig, beat, desired, id, last, unhealthy, updating, version};

pub(crate) fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// `kbf` replaced by a service of daemons (`max_updating` and `min_up` as given) with the
/// definition lines `extra` (`bake_beats`, `canary`) in its table.
pub(crate) fn definition(extra: &str, max_updating: u32, min_up: u32) -> String {
    format!(
        "[[service]]\nname = \"kbf\"\n{extra}\n[service.roles.daemon]\nmax_updating = {max_updating}\nmin_up = {min_up}\n"
    )
}

pub(crate) fn services(extra: &str, max_updating: u32, min_up: u32) -> Services {
    Services::parse(&definition(extra, max_updating, min_up)).expect("services")
}

pub(crate) fn all(want: &Desired) -> BTreeMap<String, Desired> {
    BTreeMap::from([(KBF.to_owned(), want.clone())])
}

pub(crate) fn why(s: &Services, f: &Fleet, host: &str, now: u64) -> Decision {
    let want = desired("2.0", 0, 0);
    let decision = decide_why(s, &all(&want), KBF, f, &id(host), secs(now));
    let reply = decide_in(s, &all(&want), KBF, f, &id(host), secs(now));
    match &decision {
        Decision::Update { version } => assert_eq!(
            reply,
            Reply::Update {
                version: version.clone()
            }
        ),
        Decision::Hold(_) => assert_eq!(reply, Reply::Nothing),
    }
    decision
}

pub(crate) fn go() -> Decision {
    Decision::Update {
        version: version("2.0"),
    }
}

pub(crate) fn held(why: Hold) -> Decision {
    Decision::Hold(why)
}

pub(crate) fn beats(f: &mut Fleet, host: &str, b: &kbf_updater::wire::Heartbeat, at: u64) {
    f.record(id(host), HostRole::DAEMON, b.clone(), secs(at));
}

/// With `bake_beats` absent, and with it zero, a host at the desired version holds no slot:
/// the next host is ordered at once, as before, and the definition is the same one.
#[test]
fn bake_zero_changes_nothing() {
    assert_eq!(
        services("bake_beats = 0", 1, 0),
        services("", 1, 0),
        "zero is the default"
    );
    for extra in ["", "bake_beats = 0"] {
        let s = services(extra, 1, 0);
        let mut f = Fleet::new(Duration::ZERO);
        beats(&mut f, "a", &beat("2.0"), 100);
        beats(&mut f, "b", &beat("1.0"), 100);
        assert_eq!(why(&s, &f, "b", 100), go());
    }
}

/// With `bake_beats = 3` and one slot, the host that updated holds the slot through its
/// first two healthy beats at the new version and frees it at the third.
#[test]
fn an_updated_host_holds_its_slot_until_it_has_beaten_healthy_bake_beats_times() {
    let s = services("bake_beats = 3", 1, 0);
    let mut f = Fleet::new(Duration::ZERO);
    beats(&mut f, "b", &beat("1.0"), 100);
    for (n, at) in [100, 115, 130].into_iter().enumerate() {
        beats(&mut f, "a", &beat("2.0"), at);
        beats(&mut f, "b", &beat("1.0"), at);
        let expect = if n < 2 { held(Hold::Baking) } else { go() };
        assert_eq!(why(&s, &f, "b", at), expect, "after beat {}", n + 1);
    }
}

/// An unhealthy beat, an updating beat or a pending promotion each start the count again:
/// two healthy beats before and two after are not three in a row.
#[test]
fn an_unhealthy_or_updating_beat_resets_the_bake() {
    let mut pending = beat("2.0");
    pending.promotion_pending = true;
    let breaks = [
        unhealthy(beat("2.0")),
        updating(beat("2.0"), "2.0"),
        pending,
    ];
    for broken in breaks {
        let s = services("bake_beats = 3", 1, 0);
        let mut f = Fleet::new(Duration::ZERO);
        let mut at = 100;
        for b in [beat("2.0"), beat("2.0"), broken, beat("2.0"), beat("2.0")] {
            beats(&mut f, "a", &b, at);
            at += 15;
        }
        beats(&mut f, "b", &beat("1.0"), at);
        assert_eq!(why(&s, &f, "b", at), held(Hold::Baking), "two beats since");
        beats(&mut f, "a", &beat("2.0"), at);
        assert_eq!(why(&s, &f, "b", at), go(), "three beats since");
    }
}

/// A baking host that is silent for the down limit does not hold its slot: the fleet
/// is not stalled by a host that cannot finish baking.
#[test]
fn a_baking_host_that_went_silent_frees_its_slot() {
    let s = services("bake_beats = 3", 1, 0);
    let mut f = Fleet::new(Duration::ZERO);
    beats(&mut f, "a", &beat("2.0"), 100);
    beats(&mut f, "b", &beat("1.0"), 100);
    assert_eq!(why(&s, &f, "b", 100), held(Hold::Baking));
    beats(&mut f, "b", &beat("1.0"), 145);
    assert_eq!(why(&s, &f, "b", 145), go());
}

/// A slot held by a host that is updating is a slot held, not a bake: the reason says so.
#[test]
fn a_slot_held_by_an_update_is_no_slot_not_baking() {
    let s = services("bake_beats = 3", 1, 0);
    let mut f = Fleet::new(Duration::ZERO);
    beats(&mut f, "a", &updating(beat("1.0"), "2.0"), 100);
    beats(&mut f, "b", &beat("1.0"), 100);
    assert_eq!(why(&s, &f, "b", 100), held(Hold::NoSlot));
}

/// A pending promotion holds the slot, and the host is not up: with a minimum of one other
/// host up, the next host waits while it is pending and goes when it clears.
#[test]
fn promotion_pending_holds_the_slot_and_is_not_up_until_it_clears() {
    let mut pending = beat("2.0");
    pending.promotion_pending = true;

    let slot = services("", 1, 0);
    let mut f = Fleet::new(Duration::ZERO);
    beats(&mut f, "a", &pending, 100);
    beats(&mut f, "b", &beat("1.0"), 100);
    assert_eq!(why(&slot, &f, "b", 100), held(Hold::NoSlot));
    beats(&mut f, "a", &beat("2.0"), 115);
    beats(&mut f, "b", &beat("1.0"), 115);
    assert_eq!(why(&slot, &f, "b", 115), go());

    let up = services("", 2, 1);
    let mut f = Fleet::new(Duration::ZERO);
    beats(&mut f, "a", &pending, 100);
    beats(&mut f, "b", &beat("1.0"), 100);
    assert_eq!(why(&up, &f, "b", 100), held(Hold::MinUp), "a is not up");
    beats(&mut f, "a", &beat("2.0"), 115);
    beats(&mut f, "b", &beat("1.0"), 115);
    assert_eq!(why(&up, &f, "b", 115), go());
}

/// A pending promotion keeps the service from converging even when no minimum notices.
#[test]
fn a_pending_or_baking_host_keeps_the_service_in_progress() {
    let s = services("bake_beats = 2", 1, 0);
    let def = s.get(KBF).expect("kbf");
    let want = desired("2.0", 0, 0);
    let mut f = Fleet::new(Duration::ZERO);
    beats(&mut f, "a", &beat("2.0"), 100);
    let state = |f: &Fleet, at| converge_kbf(def, Some(&want), f, secs(at)).state;
    assert_eq!(state(&f, 100), State::InProgress, "one beat of two");
    beats(&mut f, "a", &beat("2.0"), 115);
    assert_eq!(state(&f, 115), State::Converged);
    let mut pending = beat("2.0");
    pending.promotion_pending = true;
    beats(&mut f, "a", &pending, 130);
    assert_eq!(state(&f, 130), State::InProgress, "pending");
}

pub(crate) fn started(f: &mut Fleet, at: u64) {
    for host in ["a", "b", "c"] {
        beats(f, host, &beat("1.0"), at);
    }
}

/// With a canary of one, the host with the smallest id alone is ordered. It is not
/// followed until it has baked: not while ordered, updating, or baking; when its second
/// healthy beat comes the others go.
#[test]
fn with_a_canary_of_one_nobody_else_is_ordered_until_it_has_baked() {
    let s = services("bake_beats = 2\ncanary = { count = 1 }", 3, 0);
    let mut f = Fleet::new(Duration::ZERO);
    started(&mut f, 100);
    assert_eq!(why(&s, &f, "a", 100), go());
    assert_eq!(why(&s, &f, "b", 100), held(Hold::CanaryWait));
    assert_eq!(why(&s, &f, "c", 100), held(Hold::CanaryWait));
    f.order(KBF, id("a"), version("2.0"), secs(100));
    assert_eq!(
        why(&s, &f, "a", 100),
        go(),
        "its own order does not hold it"
    );
    assert_eq!(why(&s, &f, "b", 100), held(Hold::CanaryWait), "ordered");
    beats(&mut f, "a", &updating(beat("1.0"), "2.0"), 115);
    started_others(&mut f, 115);
    assert_eq!(why(&s, &f, "b", 115), held(Hold::CanaryWait), "updating");
    beats(&mut f, "a", &beat("2.0"), 130);
    started_others(&mut f, 130);
    assert_eq!(why(&s, &f, "b", 130), held(Hold::CanaryWait), "one beat");
    beats(&mut f, "a", &beat("2.0"), 145);
    started_others(&mut f, 145);
    assert_eq!(why(&s, &f, "b", 145), go(), "baked");
    assert_eq!(why(&s, &f, "c", 145), go(), "baked");
}

pub(crate) fn started_others(f: &mut Fleet, at: u64) {
    for host in ["b", "c"] {
        beats(f, host, &beat("1.0"), at);
    }
}

/// A canary at the desired version whose promotion is pending has not baked: the gate stays
/// closed, whatever the bake count says.
#[test]
fn a_canary_with_a_pending_promotion_keeps_the_gate_closed() {
    let s = services("canary = { count = 1 }", 3, 0);
    let mut f = Fleet::new(Duration::ZERO);
    started(&mut f, 100);
    let mut pending = beat("2.0");
    pending.promotion_pending = true;
    beats(&mut f, "a", &pending, 115);
    started_others(&mut f, 115);
    assert_eq!(why(&s, &f, "b", 115), held(Hold::CanaryWait));
    beats(&mut f, "a", &beat("2.0"), 130);
    started_others(&mut f, 130);
    assert_eq!(why(&s, &f, "b", 130), go());
}

/// A canary of two lets two hosts go; the third waits until both have baked.
#[test]
fn a_canary_of_two_orders_two_hosts() {
    let s = services("canary = { count = 2 }", 3, 0);
    let mut f = Fleet::new(Duration::ZERO);
    started(&mut f, 100);
    assert_eq!(why(&s, &f, "a", 100), go());
    assert_eq!(why(&s, &f, "b", 100), go());
    assert_eq!(why(&s, &f, "c", 100), held(Hold::CanaryWait));
    beats(&mut f, "a", &beat("2.0"), 115);
    started_others(&mut f, 115);
    assert_eq!(why(&s, &f, "c", 115), held(Hold::CanaryWait), "one baked");
    beats(&mut f, "b", &beat("2.0"), 115);
    assert_eq!(why(&s, &f, "c", 115), go(), "two baked");
}

/// A canary that failed keeps its place, so nobody else is ordered and the service reads
/// as blocked.
#[test]
fn a_failed_canary_blocks_the_service_and_orders_nobody_else() {
    let s = services("canary = { count = 1 }", 3, 0);
    let def = s.get(KBF).expect("kbf");
    let mut f = Fleet::new(Duration::ZERO);
    started(&mut f, 100);
    let failed = last(beat("1.0"), "2.0", kbf_updater::wire::Outcome::Failed);
    beats(&mut f, "a", &failed, 115);
    started_others(&mut f, 115);
    assert_eq!(why(&s, &f, "b", 115), held(Hold::CanaryWait));
    assert_eq!(why(&s, &f, "c", 115), held(Hold::CanaryWait));
    let want = desired("2.0", 0, 0);
    let state = converge_kbf(def, Some(&want), &f, secs(115)).state;
    assert_eq!(state, State::Blocked);
}

/// A canary that went silent before it was ever ordered is not a canary: the next host is.
#[test]
fn a_canary_that_is_down_and_never_started_is_replaced_by_the_next_host() {
    let s = services("canary = { count = 1 }", 3, 0);
    let mut f = Fleet::new(Duration::ZERO);
    started(&mut f, 100);
    beats(&mut f, "b", &beat("1.0"), 200);
    beats(&mut f, "c", &beat("1.0"), 200);
    assert_eq!(why(&s, &f, "b", 200), go());
    assert_eq!(why(&s, &f, "c", 200), held(Hold::CanaryWait));
}

/// A canary that started and then went silent still counts as started: no one takes its
/// place.
#[test]
fn one_that_started_and_went_down_still_counts() {
    let s = services("canary = { count = 1 }", 3, 0);
    let mut f = Fleet::new(Duration::ZERO);
    beats(&mut f, "a", &updating(beat("1.0"), "2.0"), 100);
    beats(&mut f, "b", &beat("1.0"), 200);
    beats(&mut f, "c", &beat("1.0"), 200);
    assert_eq!(why(&s, &f, "b", 200), held(Hold::CanaryWait));
    assert_eq!(why(&s, &f, "c", 200), held(Hold::CanaryWait));
}

/// A host of a role that waits for another role is chosen after the hosts of roles that
/// wait for nobody: the smallest id alone would pick a frontend that cannot go until the
/// storage hosts have, and nobody would.
#[test]
fn a_canary_is_chosen_among_roles_that_wait_for_nobody_first() {
    let text = "[[service]]\nname = \"kbf\"\ncanary = { count = 1 }\n\
        [service.roles.frontend]\nafter = [\"storage\"]\nmax_updating = 2\nmin_up = 0\n\
        [service.roles.storage]\nmax_updating = 2\nmin_up = 0\n";
    let s = Services::parse(text).expect("services");
    let mut f = Fleet::new(Duration::ZERO);
    f.record(id("a1"), HostRole::FRONTEND, beat("1.0"), secs(100));
    f.record(id("s1"), HostRole::STORAGE, beat("1.0"), secs(100));
    assert_eq!(why(&s, &f, "s1", 100), go());
    assert_eq!(why(&s, &f, "a1", 100), held(Hold::CanaryWait));
}

/// With a canary of two and two hosts of a role that waits for nobody, both go before any
/// host of a role that waits for them.
#[test]
fn a_canary_of_two_takes_both_hosts_of_the_free_role_before_a_waiting_one() {
    let text = "[[service]]\nname = \"kbf\"\ncanary = { count = 2 }\n\
        [service.roles.frontend]\nafter = [\"storage\"]\nmax_updating = 2\nmin_up = 0\n\
        [service.roles.storage]\nmax_updating = 2\nmin_up = 0\n";
    let s = Services::parse(text).expect("services");
    let mut f = Fleet::new(Duration::ZERO);
    f.record(id("a1"), HostRole::FRONTEND, beat("1.0"), secs(100));
    f.record(id("s1"), HostRole::STORAGE, beat("1.0"), secs(100));
    f.record(id("s2"), HostRole::STORAGE, beat("1.0"), secs(100));
    assert_eq!(why(&s, &f, "s1", 100), go());
    assert_eq!(why(&s, &f, "s2", 100), go());
    assert_eq!(why(&s, &f, "a1", 100), held(Hold::CanaryWait));
}

/// A restart forgets the count: a host that had baked two of three beats starts again at
/// one, and the next host waits until it has beaten three times since the restart.
#[tokio::test]
async fn a_restart_mid_bake_starts_the_count_again() {
    let mut rig = Rig::new("bake-restart");
    let s = || services("bake_beats = 3", 1, 0);
    rig.coordinator = Rig::start(&rig.dir, &rig.clock).with_services(s());
    rig.set(r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    let who = |name: &str| Identity {
        node_id: id(name),
        role: HostRole::DAEMON,
    };
    rig.at(2_000);
    for at in [2_000, 2_015] {
        rig.at(at);
        rig.coordinator.heartbeat(who("a"), beat("2.0"));
    }
    let asked = |rig: &Rig| rig.coordinator.heartbeat(who("b"), beat("1.0"));
    assert_eq!(asked(&rig), Reply::Nothing, "two of three");
    rig.at(3_000);
    rig.coordinator = Rig::start(&rig.dir, &rig.clock).with_services(s());
    rig.at(3_031);
    rig.coordinator.heartbeat(who("a"), beat("2.0"));
    assert_eq!(
        asked(&rig),
        Reply::Nothing,
        "one of three since the restart"
    );
    for at in [3_046, 3_061] {
        rig.at(at);
        rig.coordinator.heartbeat(who("a"), beat("2.0"));
    }
    assert_eq!(
        asked(&rig),
        Reply::Update {
            version: version("2.0")
        }
    );
}

/// The fields are read from a definition and refused where they are not usable: over the
/// cap, a canary of none, and the roles and labels form of the canary (a later change).
#[test]
fn the_definition_reads_bake_and_canary_and_refuses_the_rest() {
    let s = services("bake_beats = 100\ncanary = { count = 2 }", 1, 0);
    let kbf = s.get(KBF).expect("kbf");
    assert_eq!(kbf.bake_beats, 100);
    assert_eq!(kbf.canary.map(|c| c.count), Some(2));
    let refused = |extra: &str| {
        Services::parse(&definition(extra, 1, 0))
            .expect_err("refused")
            .to_string()
    };
    assert!(refused("bake_beats = 101").contains("bake_beats"));
    assert!(refused("bake_beats = 101").contains("100"));
    assert!(refused("canary = { count = 0 }").contains("canary"));
    assert!(refused("canary = { roles = [\"daemon\"] }").contains("unknown field `roles`"));
    assert!(refused("canary = { labels = { a = \"b\" } }").contains("unknown field `labels`"));
}
