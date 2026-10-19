//! What a node's report of its metadata log's last member and boot does to the member id and
//! boot the store gives it (`kbf_server::members::LastBoot`, `docs/design/frontend-proxy-u0.md`
//! section (e)). Every test runs on the fake store.

#[path = "support/hooked.rs"]
mod hooked;

use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_map::{MemberId, MemberState};
use kbf_server::members::{
    LastBoot, Members, MembershipConfig, PROTOCOL, Progress, Registrant, Registration,
};
use kbf_state::{Cond, Store, Txn};

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

fn node(name: &str) -> Registrant {
    Registrant {
        instance_id: name.to_owned(),
        addr: format!("{name}.example:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

fn after(m: &Members, name: &str, member: u64, boot: u64) -> Registration {
    m.register_after(&node(name), LastBoot::from_wire(member, boot))
        .expect("register")
}

/// Catches: a report that names no writer (member or boot 0, which a node with no node
/// writer in its log, or an older node, sends) read as a claim on member 0 or on a zero
/// boot.
#[test]
fn a_report_with_a_zero_names_nothing() {
    assert_eq!(LastBoot::from_wire(0, 5), None);
    assert_eq!(LastBoot::from_wire(5, 0), None);
    assert_eq!(
        LastBoot::from_wire(3, 5),
        Some(LastBoot { member: 3, boot: 5 })
    );
}

/// Catches: a new instance that is not given the id its log names, or a boot not above the
/// log's. Member 3 free, boot 5 reported: id 3 and boot 6; the next registration is 7.
/// Mutant: the seed without the plus one (a loop of `floor` nexts) gives boot 5, red.
#[test]
fn a_new_instance_gets_the_id_its_log_names_and_a_boot_above_the_logs() {
    let s = Hooked::new();
    let m = members(&s);
    let other = after(&m, "other", 0, 0);
    assert_eq!((other.id, other.boot), (MemberId(1), 1));
    let mine = after(&m, "mine", 3, 5);
    assert_eq!((mine.id, mine.boot), (MemberId(3), 6));
    let again = after(&m, "mine", 3, 5);
    assert_eq!((again.id, again.boot), (MemberId(3), 7));
}

/// Catches: an id handed to a second instance. Another instance holds 1 (a registration
/// without a report; a node reporting member 1 is not given it), so the node is given a
/// new id and boot 1, and `instance/` keeps one row per id.
#[test]
fn a_reported_id_that_a_live_instance_holds_is_not_handed_out() {
    let s = Hooked::new();
    let m = members(&s);
    let held = after(&m, "held", 0, 0);
    assert_eq!(held.id, MemberId(1));
    let mine = after(&m, "mine", 1, 4);
    assert_ne!(mine.id, held.id);
    assert_eq!(mine.boot, 1);
    let held_again = after(&m, "held", 0, 0);
    assert_eq!(held_again.id, held.id, "the holder keeps its id");
}

/// Catches: the free-id check that reads only the claim rows. A row written before claims
/// existed (`instance/<name>` with the id, no `member-owner/` row) still holds its id.
/// Mutant: `id_is_free` without the scan, red.
#[test]
fn an_id_held_by_a_row_without_a_claim_is_not_handed_out() {
    let s = Hooked::new();
    let m = members(&s);
    let put = Txn::new().put("instance/old", 2_u64.to_be_bytes(), Cond::Absent);
    s.fake.txn(put).expect("store").expect("commit");
    let mine = after(&m, "mine", 2, 4);
    assert_ne!(mine.id, MemberId(2));
    assert_eq!(mine.boot, 1);
}

/// Catches: two nodes that name one free id both given it. The first gets it, the second
/// a new id; both keep theirs on a second registration.
#[test]
fn two_nodes_naming_one_id_do_not_share_it() {
    let s = Hooked::new();
    let m = members(&s);
    let a = after(&m, "a", 4, 2);
    let b = after(&m, "b", 4, 9);
    assert_eq!((a.id, a.boot), (MemberId(4), 3));
    assert_ne!(b.id, a.id);
    assert_eq!(after(&m, "a", 4, 2).id, a.id);
    assert_eq!(after(&m, "b", 4, 9).id, b.id);
}

/// Catches: a counter lowered by a report, and an instance's id changed by one. A known
/// instance at boot 10 that reports boot 3 gets 11, and one that reports another id keeps
/// its own. Mutant: the boot taken from the report (`last.boot + 1`) gives 4, red.
#[test]
fn a_known_instance_is_never_given_a_lower_boot_or_another_id() {
    let s = Hooked::new();
    let m = members(&s);
    let mut boot = 0;
    for _ in 0..10 {
        boot = after(&m, "mine", 0, 0).boot;
    }
    assert_eq!(boot, 10);
    let low = after(&m, "mine", 1, 3);
    assert_eq!((low.id, low.boot), (MemberId(1), 11));
    let other_id = after(&m, "mine", 9, 100);
    assert_eq!((other_id.id, other_id.boot), (MemberId(1), 12));
}

/// Catches: a known instance (a store restored from an older copy) whose boot is not raised
/// above its log's: id 1 at boot 2 in the store, the log at boot 9: the next boot is 10.
/// Mutant: the raise lowered to `floor - boot` nexts gives 9, red.
#[test]
fn a_known_instance_behind_its_log_is_raised_above_it() {
    let s = Hooked::new();
    let m = members(&s);
    after(&m, "mine", 0, 0);
    after(&m, "mine", 0, 0);
    let lifted = after(&m, "mine", 1, 9);
    assert_eq!((lifted.id, lifted.boot), (MemberId(1), 10));
    assert_eq!(after(&m, "mine", 1, 9).boot, 11, "counters keep going up");
}

/// Catches: the joiner rule skipped on the claim path. In a farm that holds data a node
/// that names a free id is `Joining` with its `unfilled/` mark, exactly as a node that
/// does not.
#[test]
fn a_node_that_names_an_id_still_joins_a_farm_with_data_as_a_joiner() {
    let s = Hooked::new();
    let m = members(&s);
    after(&m, "a", 0, 0);
    after(&m, "b", 0, 0);
    s.clock.advance(Duration::from_secs(120));
    let c = after(&m, "c", 7, 3);
    assert_eq!((c.id, c.boot), (MemberId(7), 4));
    assert_eq!(
        c.map.member(c.id).map(|x| x.state),
        Some(MemberState::Joining)
    );
    assert!(s.fake.get("unfilled/7").expect("read").is_some());
}

/// Catches: an unbounded raise: a node that reports a boot of 2^40 would put that many
/// operations in one transaction. The boot is clamped, so the registration answers.
#[test]
fn a_huge_reported_boot_is_clamped() {
    let s = Hooked::new();
    let m = members(&s);
    let r = after(&m, "mine", 2, 1 << 40);
    assert_eq!(r.id, MemberId(2));
    assert_eq!(r.boot, kbf_server::members::MAX_SEED + 1);
}

/// Catches: a creating transaction that loses a race to the same instance's other
/// registration (it claims the id between this one's read and its write) taken as a failure,
/// or that overwrites the winner's claim. The rival gets member 5 at boot 4; the call that
/// lost reads the known instance and takes the next boot, 5, of the same member.
#[test]
fn a_registration_that_loses_the_race_to_create_its_instance_reads_it_known() {
    let s = Hooked::new();
    let m = members(&s);
    let rival = Members::new(
        Arc::clone(&s.fake) as Arc<dyn Store>,
        MembershipConfig::default(),
    );
    s.after_get("member-owner/", move || {
        let _ = after(&rival, "mine", 5, 3);
    });
    let lost = after(&m, "mine", 5, 3);
    assert_eq!((lost.id, lost.boot), (MemberId(5), 5));
}

/// Catches: a known instance's boot taken when the instance row changed between the read and
/// the write (the check on its version fails): the call reads it again and still gets a
/// boot above the last, and the counter is not spent twice for one registration.
#[test]
fn a_known_instance_that_changes_under_a_registration_is_read_again() {
    let s = Hooked::new();
    let m = members(&s);
    assert_eq!(after(&m, "mine", 0, 0).boot, 1);
    let fake = Arc::clone(&s.fake);
    s.after_get("instance/mine", move || {
        let same = 1_u64.to_be_bytes();
        fake.txn(Txn::new().put("instance/mine", same, Cond::Any))
            .expect("store")
            .expect("commit");
    });
    let again = after(&m, "mine", 0, 0);
    assert_eq!((again.id, again.boot), (MemberId(1), 2));
}
