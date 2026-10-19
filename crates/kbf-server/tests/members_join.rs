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
use kbf_server::storage_health::Reported;
use kbf_state::{Cond, Store, Txn};
use kbf_types::{Digest, DigestFunction};
use sha2::{Digest as _, Sha256};

fn config_of() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(3600),
        dwell: Duration::from_secs(7200),
        replication: 2,
    }
}

fn members(store: &Arc<Hooked>) -> Members {
    Members::new(Arc::clone(store) as Arc<dyn Store>, config_of())
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
    assert_eq!(state(&c.map, c.id), Some(MemberState::Joining));
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
    m.heartbeat(c.id, c.boot, Progress::CaughtUp, Reported::default(), None)
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

/// Catches: the real new node, which registers `CatchingUp` (a new meta log has not
/// rebuilt its index) and then beats caught up as soon as the empty rebuild is done, being
/// made `Active` by that beat: it must be `Joining` from its first registration and stay
/// so, primary for nothing. Mutant: the rebuilding check before the join check turns it red.
#[test]
fn a_new_node_that_registers_rebuilding_joins_and_is_not_promoted_by_its_beat() {
    let s = Hooked::new();
    let m = members(&s);
    let _ = old_farm(&s, &m);
    let c = m.register(&node(3, Progress::CatchingUp)).expect("c");
    assert_eq!(state(&c.map, c.id), Some(MemberState::Joining));
    m.heartbeat(c.id, c.boot, Progress::CaughtUp, Reported::default(), None)
        .expect("beat");
    let map = m.current_map().expect("read").expect("a map");
    assert_eq!(state(&map, c.id), Some(MemberState::Joining));
    assert_eq!(primaries(&map, c.id), 0);
}

/// Catches: a join decided apart from the creation of the instance: the registration
/// creates `instance/<id>` and then fails to write the row (a store that conflicts on
/// `member/`), and the node's retry finds a known instance and is `Active` with no mark.
#[test]
fn a_registration_that_fails_after_the_instance_is_created_retries_as_joining() {
    let s = Hooked::new();
    let m = members(&s);
    let _ = old_farm(&s, &m);
    s.conflict_on(Some("member/"));
    assert!(m.register(&node(3, Progress::CaughtUp)).is_err());
    s.conflict_on(None);
    let c = m.register(&node(3, Progress::CaughtUp)).expect("retry");
    assert_eq!(state(&c.map, c.id), Some(MemberState::Joining));
}

/// Catches: two registrations of one new instance ending `Active` and unmarked: the
/// second (not fresh) runs between the first's reading of the mark and its write, and the
/// first then loses to its larger boot. The mark is written with the instance, so the
/// second reads it.
#[test]
fn two_registrations_of_one_new_instance_end_joining() {
    let s = Hooked::new();
    let m = members(&s);
    let _ = old_farm(&s, &m);
    let rival = Members::new(Arc::clone(&s.fake) as Arc<dyn Store>, config_of());
    s.after_get("unfilled/", move || {
        let _ = rival.register(&node(3, Progress::CaughtUp));
    });
    let _ = m.register(&node(3, Progress::CaughtUp));
    let map = m.current_map().expect("read").expect("a map");
    let id = map
        .members()
        .iter()
        .find(|x| x.addr.starts_with("192.0.2.3"))
        .expect("the member")
        .id;
    assert_eq!(state(&map, id), Some(MemberState::Joining));
}

/// Catches: a restart that read the mark as present overwriting the operator's promotion
/// (a 200 that reverts to `Joining`): the promotion lands between the read and the write.
#[test]
fn a_promotion_between_a_restarts_read_and_write_is_not_reverted() {
    let s = Hooked::new();
    let m = members(&s);
    let _ = old_farm(&s, &m);
    let c = m.register(&node(3, Progress::CaughtUp)).expect("c");
    let operator = Members::new(Arc::clone(&s.fake) as Arc<dyn Store>, config_of());
    let id = c.id;
    s.after_get("unfilled/", move || {
        operator
            .set_state(id, MemberState::Active)
            .expect("promote");
    });
    let again = m.register(&node(3, Progress::CaughtUp)).expect("restart");
    assert_eq!(state(&again.map, c.id), Some(MemberState::Active));
    let map = m.current_map().expect("read").expect("a map");
    assert_eq!(state(&map, c.id), Some(MemberState::Active));
}

/// Catches: a repeated promotion that leaves a stale mark (one an older frontend's promotion
/// never deleted), which would demote a working primary at its next restart.
#[test]
fn promoting_an_active_member_deletes_a_stale_mark() {
    let s = Hooked::new();
    let m = members(&s);
    let _ = old_farm(&s, &m);
    let c = m.register(&node(3, Progress::CaughtUp)).expect("c");
    m.set_state(c.id, MemberState::Active).expect("promote");
    let stale = s
        .fake
        .txn(Txn::new().put(format!("unfilled/{}", c.id.0), Vec::new(), Cond::Any));
    assert!(stale.expect("store").is_ok());
    m.set_state(c.id, MemberState::Active).expect("again");
    let again = m.register(&node(3, Progress::CaughtUp)).expect("restart");
    assert_eq!(state(&again.map, c.id), Some(MemberState::Active));
}

/// Catches: the bootstrap window moved: a node 59 s after the farm's first member is
/// `Active` (nodes starting together), one 61 s after is `Joining`.
#[test]
fn the_bootstrap_window_is_sixty_seconds() {
    for (after, want) in [(59, MemberState::Active), (61, MemberState::Joining)] {
        let s = Hooked::new();
        let m = members(&s);
        m.register(&node(1, Progress::CaughtUp)).expect("a");
        s.clock.advance(Duration::from_secs(after));
        let c = m.register(&node(3, Progress::CaughtUp)).expect("c");
        assert_eq!(state(&c.map, c.id), Some(want), "after {after} s");
    }
}

/// Catches: the window sliding: a restart of an `Active` member at 50 s must not give the
/// farm a new minute, so a new node at 61 s is `Joining`.
#[test]
fn a_restart_does_not_extend_the_bootstrap_window() {
    let s = Hooked::new();
    let m = members(&s);
    m.register(&node(1, Progress::CaughtUp)).expect("a");
    s.clock.advance(Duration::from_secs(50));
    m.register(&node(1, Progress::CaughtUp)).expect("a again");
    s.clock.advance(Duration::from_secs(11));
    let c = m.register(&node(3, Progress::CaughtUp)).expect("c");
    assert_eq!(state(&c.map, c.id), Some(MemberState::Joining));
}

/// Every log line this test binary writes, from one subscriber for the whole process: a
/// subscriber set for one thread misses a call site another test's thread met first.
#[derive(Clone, Default)]
struct Lines(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn process_lines() -> &'static Lines {
    static LINES: std::sync::OnceLock<Lines> = std::sync::OnceLock::new();
    LINES.get_or_init(|| {
        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one subscriber");
        lines
    })
}

/// Catches: a joiner admitted without a word to the operator. A node that joins a farm
/// with data waits as `Joining` until it is promoted (`POST /v1/members`, state active),
/// so its registration logs a warning that names the member and how to promote it; the
/// first members of an empty farm, which are `Active` at once, log none. The members
/// here have addresses no other test uses, so their lines are this test's.
#[test]
fn a_joiner_is_logged_with_how_to_promote_it_and_a_first_member_is_not() {
    let lines = process_lines();
    let s = Hooked::new();
    let m = members(&s);
    m.register(&node(201, Progress::CaughtUp)).expect("a");
    m.register(&node(202, Progress::CaughtUp)).expect("b");
    s.clock.advance(Duration::from_secs(120));
    // Member ids are per store and repeat across tests; the address does not.
    let mine = |i: u32| {
        let text = String::from_utf8(lines.0.lock().expect("lock").clone()).expect("UTF-8");
        let tag = format!("addr={}", node(i, Progress::CaughtUp).addr);
        text.lines()
            .filter(|line| line.contains(&tag))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert!(
        mine(201).is_empty(),
        "a first member was logged: {:?}",
        mine(201)
    );
    assert!(
        mine(202).is_empty(),
        "a first member was logged: {:?}",
        mine(202)
    );
    let c = m.register(&node(203, Progress::CaughtUp)).expect("c");
    assert_eq!(state(&c.map, c.id), Some(MemberState::Joining));
    let logged = mine(203);
    assert!(
        logged.iter().any(|line| line.contains("WARN")
            && line.contains("a joiner")
            && line.contains("POST /v1/members")
            && line.contains(&format!("member={}", c.id.0))),
        "{logged:?}"
    );
}
