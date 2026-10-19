//! A storage node joining a farm that already holds data (`kbf_server::members`): it holds
//! none of it, so it must not be primary (rank 1 of a write, the `FindMissing` target) for
//! any digest until it is promoted. Every test runs on the fake store with its injectable
//! clock, and rows live long enough that the clock moving does not lapse them.

#[path = "support/hooked.rs"]
mod hooked;

use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_map::{Map, MemberId, MemberState};
use kbf_server::members::{
    Members, MembershipConfig, PROTOCOL, Progress, Registrant, Registration,
};
use kbf_server::storage_health::Debts;
use kbf_state::Store;
use kbf_types::{Digest, DigestFunction};
use sha2::{Digest as _, Sha256};

fn members(store: &Arc<Hooked>) -> Members {
    Members::new(
        Arc::clone(store) as Arc<dyn Store>,
        MembershipConfig {
            ttl: Duration::from_secs(3600),
            dwell: Duration::from_secs(7200),
            replication: 2,
        },
    )
}

fn node(i: u32, progress: Progress) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress,
    }
}

fn state(map: &Map, id: MemberId) -> Option<MemberState> {
    map.member(id).map(|m| m.state)
}

fn digests() -> Vec<Digest> {
    (0_u64..300)
        .map(|i| {
            let hash: [u8; 32] = Sha256::digest(i.to_be_bytes()).into();
            Digest::new(DigestFunction::Sha256, hash, 1000 + i)
        })
        .collect()
}

/// How many digests `id` is the primary (first write owner) of.
fn primaries(map: &Map, id: MemberId) -> usize {
    digests()
        .iter()
        .filter(|d| map.write_owners(d).first() == Some(&id))
        .count()
}

/// How many digests `id` owns a write of at all.
fn owned(map: &Map, id: MemberId) -> usize {
    digests()
        .iter()
        .filter(|d| map.write_owners(d).contains(&id))
        .count()
}

/// Two members that formed the farm, then a farm that has been up long enough to hold
/// data (the clock past the bootstrap window).
fn old_farm(s: &Arc<Hooked>, m: &Members) -> (Registration, Registration) {
    let a = m.register(&node(1, Progress::CaughtUp)).expect("a");
    let b = m.register(&node(2, Progress::CaughtUp)).expect("b");
    s.clock.advance(Duration::from_secs(120));
    (a, b)
}

/// Catches: a node with an empty store admitted `Active` into a farm that holds data, so
/// it is rank 1 for about 1/N of the digests that exist and holds none of them (the
/// `FindMissing` on its slice then says every blob is missing). The joiner must be
/// `Joining`, primary for nothing, and still an owner of writes (its copy of new writes
/// is how it fills). Mutant: admit `Active` whatever the farm holds turns this red.
#[test]
fn a_node_joining_a_farm_with_data_is_never_primary() {
    let s = Hooked::new();
    let m = members(&s);
    let _ = old_farm(&s, &m);
    let c = m.register(&node(3, Progress::CaughtUp)).expect("c");
    assert_eq!(state(&c.map, c.id), Some(MemberState::Joining));
    assert_eq!(primaries(&c.map, c.id), 0);
    assert!(
        owned(&c.map, c.id) > 0,
        "a joiner takes writes as a replica"
    );
}

/// Catches: the bootstrap of an empty farm refused by the same rule: nodes that start
/// together (a farm that has held nothing) are `Active` at once.
#[test]
fn the_first_members_of_a_farm_are_active() {
    let s = Hooked::new();
    let m = members(&s);
    let regs: Vec<Registration> = (1..=3)
        .map(|i| m.register(&node(i, Progress::CaughtUp)).expect("register"))
        .collect();
    let map = m.current_map().expect("read").expect("a map");
    for r in &regs {
        assert_eq!(state(&map, r.id), Some(MemberState::Active));
    }
    assert!(primaries(&map, regs[2].id) > 0);
}

/// Catches: a promotion that does not take effect (the joiner stays out of the primary
/// slot), and one that leaves the unfilled mark: a restart of the promoted member must
/// keep it `Active`, as a restart of any filled member does.
#[test]
fn a_promoted_joiner_is_primary_and_keeps_active_across_a_restart() {
    let s = Hooked::new();
    let m = members(&s);
    let _ = old_farm(&s, &m);
    let c = m.register(&node(3, Progress::CaughtUp)).expect("c");
    let map = m.set_state(c.id, MemberState::Active).expect("promote");
    assert_eq!(state(&map, c.id), Some(MemberState::Active));
    assert!(primaries(&map, c.id) > 0);
    let again = m.register(&node(3, Progress::CaughtUp)).expect("restart");
    assert_eq!((again.id, again.boot), (c.id, 2));
    assert_eq!(state(&again.map, c.id), Some(MemberState::Active));
}

/// Catches: an unpromoted joiner that restarts (its index reads whole, since it has
/// nothing to rebuild) being made `Active` by its own registration, and one that beats
/// caught up being promoted by the beat: neither says it holds its slice.
#[test]
fn an_unfilled_joiner_stays_joining_through_a_restart_and_a_beat() {
    let s = Hooked::new();
    let m = members(&s);
    let _ = old_farm(&s, &m);
    let c = m.register(&node(3, Progress::CaughtUp)).expect("c");
    m.heartbeat(c.id, c.boot, Progress::CaughtUp, Debts::Unknown, None)
        .expect("beat");
    let map = m.current_map().expect("read").expect("a map");
    assert_eq!(state(&map, c.id), Some(MemberState::Joining));
    let again = m.register(&node(3, Progress::CaughtUp)).expect("restart");
    assert_eq!(state(&again.map, c.id), Some(MemberState::Joining));
    let rebuilding = m.register(&node(3, Progress::CatchingUp)).expect("restart");
    assert_eq!(state(&rebuilding.map, c.id), Some(MemberState::Joining));
    assert_eq!(primaries(&rebuilding.map, c.id), 0);
}

/// Catches: a restart of a member that is `Active` in a farm with data being demoted to a
/// joiner for good: it holds its data, its instance is known, and it comes back `Active`.
#[test]
fn a_restarted_active_member_of_a_farm_with_data_stays_active() {
    let s = Hooked::new();
    let m = members(&s);
    let (a, _) = old_farm(&s, &m);
    let again = m.register(&node(1, Progress::CaughtUp)).expect("restart");
    assert_eq!(again.id, a.id);
    assert_eq!(state(&again.map, a.id), Some(MemberState::Active));
}
