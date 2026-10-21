//! The deny list in the membership store (`Members::deny`, `allow`, `denied`) and the map
//! built from it (`Members::refresh`): the rows, their canonical spelling, a change of the
//! list alone being a new generation, and the bounds. Every test runs on the fake store
//! with its injectable clock.

#[path = "support/hooked.rs"]
mod hooked;

use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_map::{DenyEntry, Map};
use kbf_server::members::{
    MAX_DENIED, Members, MembershipConfig, MembershipError, PROTOCOL, Progress, Registrant,
};
use kbf_state::{Cond, Store, Txn};

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(30),
        dwell: Duration::from_secs(120),
        replication: 2,
    }
}

/// Membership through the (hookable) store.
fn members(store: &Arc<Hooked>) -> Members {
    Members::new(Arc::clone(store) as Arc<dyn Store>, config())
}

/// Membership straight on the fake: another frontend, which no hook interrupts.
fn other_frontend(store: &Arc<Hooked>) -> Members {
    Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config())
}

fn node(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

fn serial(text: &str) -> DenyEntry {
    DenyEntry::Serial(text.to_owned())
}

fn key(digit: char) -> DenyEntry {
    DenyEntry::SpkiSha256(digit.to_string().repeat(64))
}

fn named(text: &str) -> DenyEntry {
    DenyEntry::Node(text.to_owned())
}

fn map_of(m: &Members) -> Map {
    m.current_map().expect("read").expect("a map")
}

/// Catches a deny list that never reaches the map or does not move its generation, which
/// no storage node could then tell from the one before: an entry written through one
/// frontend is not in the map until a refresh (the rows are not the map), the refresh
/// through another frontend turns the generation by exactly one with the entry in the map
/// and the members untouched, and a refresh with nothing new turns nothing. A second
/// entry and a third go in by one refresh, one generation, sorted (serials, key hashes,
/// then node ids); allowing the first takes it out, one generation; allowing one that is
/// not listed says so and changes nothing; an entry denied twice is one row and one
/// generation.
///
/// Mutants seen red: `plan` taking the deny list from the map it replaces (the entry never
/// arrives); the "nothing changed" test of `plan` leaving the deny list out (no new
/// generation).
#[test]
fn an_entry_enters_the_map_and_a_change_of_the_list_alone_turns_the_generation() {
    let s = Hooked::new();
    let m = members(&s);
    let ops = other_frontend(&s);
    let a = m.register(&node(1)).expect("a");
    let g = a.map.generation();
    assert_eq!(a.map.denied(), []);

    ops.deny(&serial("a1b"), "a lost laptop").expect("deny");
    assert_eq!(map_of(&m).generation(), g, "the rows are not the map");
    let map = m.refresh().expect("refresh").expect("a map");
    assert_eq!(map.generation(), g + 1);
    assert_eq!(map.denied(), [serial("a1b")]);
    assert_eq!(map.members(), a.map.members(), "no member changed");
    assert_eq!(map_of(&ops), map, "another frontend reads the same map");
    assert_eq!(m.refresh().expect("again").expect("a map"), map);

    ops.deny(&serial("a1b"), "denied again").expect("a repeat");
    assert_eq!(m.refresh().expect("refresh").expect("a map"), map);

    ops.deny(&named("mac-07"), "retired").expect("node");
    ops.deny(&key('c'), "key leaked").expect("key");
    let map = m.refresh().expect("refresh").expect("a map");
    assert_eq!(
        map.generation(),
        g + 2,
        "two entries, one refresh, one turn"
    );
    assert_eq!(
        map.denied(),
        [serial("a1b"), key('c'), named("mac-07")],
        "sorted, whatever order they were written in"
    );

    assert!(ops.allow(&serial("a1b")).expect("allow"));
    let map = m.refresh().expect("refresh").expect("a map");
    assert_eq!(map.generation(), g + 3);
    assert_eq!(map.denied(), [key('c'), named("mac-07")]);
    assert!(!ops.allow(&serial("a1b")).expect("allow again"));
    assert_eq!(m.refresh().expect("refresh").expect("a map"), map);
}

/// Catches a change of the list that leaves it as long as it was going unseen: one entry
/// denied and another allowed before one refresh (two operators, or one call each) swap the
/// list for another of the same size, and the map must turn to the new one. Were only the
/// length compared, the map would keep the old list: storage would keep refusing the allowed
/// entry and admit the denied one until some other change.
///
/// Mutant seen red: the "nothing changed" test of `plan` comparing the lists' lengths.
#[test]
fn a_swap_of_one_entry_for_another_turns_the_generation() {
    let s = Hooked::new();
    let m = members(&s);
    let ops = other_frontend(&s);
    m.register(&node(1)).expect("a");
    ops.deny(&serial("a1b"), "first").expect("deny");
    ops.deny(&named("mac-07"), "second").expect("deny");
    let before = m.refresh().expect("refresh").expect("a map");
    assert_eq!(before.denied(), [serial("a1b"), named("mac-07")]);

    ops.deny(&key('c'), "swapped in").expect("deny");
    assert!(ops.allow(&serial("a1b")).expect("allow"));
    let after = m.refresh().expect("refresh").expect("a map");
    assert_eq!(after.denied(), [key('c'), named("mac-07")]);
    assert_eq!(after.denied().len(), before.denied().len());
    assert_eq!(after.generation(), before.generation() + 1);
}

/// Catches a node entry that would name no certificate being listed as if it did, and a
/// value that cannot be a row: only a certificate's DNS names are compared with a `node`
/// entry, so an IP address (v4 or v6, bracketed or not) and a `host:port` are `Invalid`, as
/// is a value with a control character; a name that merely looks like numbers, a wildcard
/// and one with a slash are taken. A refusal writes nothing.
///
/// Mutants seen red: the IP test removed; the colon test removed; the control-character
/// test removed.
#[test]
fn a_node_entry_is_a_dns_name_and_no_value_holds_a_control_character() {
    let s = Hooked::new();
    let m = members(&s);
    for refused in [
        "192.0.2.7",
        "2001:db8::7",
        "[2001:db8::7]",
        "mac-07:8982",
        "mac\u{0}07",
        "mac-07\n",
        "mac\t07",
    ] {
        assert!(
            matches!(
                m.deny(&named(refused), "x"),
                Err(MembershipError::Invalid(_))
            ),
            "{refused:?}"
        );
    }
    assert_eq!(m.denied().expect("list"), [], "a refusal wrote nothing");
    for taken in ["10.farm.example", "1.2.3", "*.farm.example", "zone/mac-07"] {
        m.deny(&named(taken), "x").expect(taken);
    }
    assert_eq!(m.denied().expect("list").len(), 4);
}

/// Catches an entry denied before any member registered going nowhere: the rows are the
/// list, so the first map ever built carries them (generation 1), and not the empty list
/// there is to copy forward from.
///
/// Mutant seen red: `plan` taking the deny list from the current map even when there is
/// none (an empty list for the first map).
#[test]
fn an_entry_denied_before_the_first_member_is_in_the_first_map() {
    let s = Hooked::new();
    let m = members(&s);
    m.deny(&named("mac-07"), "retired").expect("deny");
    assert_eq!(m.refresh().expect("refresh"), None, "no member, no map");
    let a = m.register(&node(1)).expect("a");
    assert_eq!(a.map.generation(), 1);
    assert_eq!(a.map.denied(), [named("mac-07")]);
}

/// Catches an entry spelled differently getting a row of its own, a row that does not carry
/// the entry in its key, or a list that depends on the order of the writes: the key is
/// `deny/<kind>/<value>` in the canonical spelling, so `serial 0A:1B` typed with colons and
/// capitals, and a key hash in capitals, are the rows `deny/serial/a1b` and
/// `deny/spki-sha256/<lower case>`; a node id holding a `/` still reads back whole; the
/// value of a row holds the reason; and `denied` lists each entry once, sorted.
#[test]
fn the_rows_are_one_per_entry_under_the_canonical_key() {
    let s = Hooked::new();
    let m = members(&s);
    let spelled: DenyEntry = "serial 0A:1B".parse().expect("an entry");
    let capitals: DenyEntry = format!("spki-sha256 {}", "AB".repeat(32))
        .parse()
        .expect("an entry");
    m.deny(&named("zone/mac-07"), "a node id with a slash")
        .expect("deny");
    m.deny(&capitals, "key leaked").expect("deny");
    m.deny(&spelled, "a lost laptop").expect("deny");
    m.deny(&serial("a1b"), "the same, canonical")
        .expect("a repeat");

    let rows: Vec<String> = s
        .fake
        .scan_prefix("deny/")
        .expect("scan")
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(
        rows,
        [
            "deny/node/zone/mac-07".to_owned(),
            "deny/serial/a1b".to_owned(),
            format!("deny/spki-sha256/{}", "ab".repeat(32)),
        ]
    );
    let value = s.fake.get("deny/serial/a1b").expect("read").expect("a row");
    let text = String::from_utf8_lossy(&value.value).into_owned();
    assert!(
        text.contains("a lost laptop"),
        "the first reason is kept: {text:?}"
    );
    assert!(!text.contains("canonical"), "{text:?}");
    assert_eq!(
        m.denied().expect("list"),
        [
            spelled,
            DenyEntry::SpkiSha256("ab".repeat(32)),
            named("zone/mac-07")
        ]
    );
}

/// Catches a deny that overwrites the row of a frontend that listed the same entry in the
/// meantime: this frontend reads the list (the entry is not on it), and before it writes,
/// another frontend denies the same entry with its own reason. The write must lose to the
/// row that is there (the entry is listed, as asked, and the call answers `Ok`), and the row
/// keeps its first reason and time.
///
/// Mutant seen red: the row written with `Cond::Any` instead of `Cond::Absent`.
#[test]
fn a_deny_that_loses_a_race_for_the_same_entry_keeps_the_first_row() {
    let s = Hooked::new();
    let m = members(&s);
    let rival = other_frontend(&s);
    s.after_get("deny/", move || {
        rival
            .deny(&named("mac-07"), "the first writer")
            .expect("the rival denies");
    });

    m.deny(&named("mac-07"), "the second writer")
        .expect("a lost race for the same entry is still a deny");

    assert_eq!(m.denied().expect("list"), [named("mac-07")]);
    let row = s
        .fake
        .get("deny/node/mac-07")
        .expect("read")
        .expect("a row");
    let text = String::from_utf8_lossy(&row.value).into_owned();
    assert!(text.contains("the first writer"), "{text:?}");
    assert!(!text.contains("the second writer"), "{text:?}");
}

/// Catches a list that is guessed at: a `deny/` row whose key is no entry (written by hand,
/// or by a build that knows another kind) makes `denied` and `refresh` fail
/// `Corrupt` rather than skip it, since an entry dropped lets its caller back in.
#[test]
fn a_row_that_is_no_entry_is_never_skipped() {
    let s = Hooked::new();
    let m = members(&s);
    m.register(&node(1)).expect("a");
    for key in [
        "deny/serial/not-hex",
        "deny/other/x",
        "deny/serial",
        "deny/",
    ] {
        let put = Txn::new().put(key, "x", Cond::Any);
        s.fake.txn(put).expect("store").expect("put");
        assert!(
            matches!(m.denied(), Err(MembershipError::Corrupt(_))),
            "{key}"
        );
        assert!(
            matches!(m.refresh(), Err(MembershipError::Corrupt(_))),
            "{key}"
        );
        let gone = Txn::new().delete(key, Cond::Any);
        s.fake.txn(gone).expect("store").expect("delete");
    }
    assert_eq!(m.denied().expect("list"), []);
}

/// Catches a removal that cannot give up or a store failure that is swallowed: an `allow`
/// whose compare-and-set keeps losing is `Contended` (the row stays), a store that fails the
/// read is the store's error, and neither changes the list.
#[test]
fn a_removal_that_keeps_losing_gives_up_and_a_store_failure_surfaces() {
    let s = Hooked::new();
    let m = members(&s);
    m.deny(&named("mac-07"), "retired").expect("deny");
    s.conflict_on(Some("deny/"));
    assert!(matches!(
        m.allow(&named("mac-07")),
        Err(MembershipError::Contended)
    ));
    s.conflict_on(None);
    s.fail_gets.store(1, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        m.allow(&named("mac-07")),
        Err(MembershipError::Store(_))
    ));
    assert_eq!(m.denied().expect("list"), [named("mac-07")]);
}

/// Catches an unbounded deny list (every map is sent to every node with the whole of it):
/// a value of 253 bytes and a reason of 512 are taken and one byte more of either is
/// `Invalid`; with `MAX_DENIED` entries listed a new one is `Invalid` while one already
/// listed is still taken, and once one is allowed another fits.
#[test]
fn the_deny_list_is_bounded() {
    let s = Hooked::new();
    let m = members(&s);
    m.deny(&named(&"n".repeat(253)), &"r".repeat(512))
        .expect("the longest value and reason");
    for (entry, reason) in [
        (named(&"n".repeat(254)), "short".to_owned()),
        (serial(&"a".repeat(254)), "short".to_owned()),
        (named("mac-07"), "r".repeat(513)),
    ] {
        assert!(
            matches!(m.deny(&entry, &reason), Err(MembershipError::Invalid(_))),
            "{} bytes of reason",
            reason.len()
        );
    }
    assert_eq!(
        m.denied().expect("list").len(),
        1,
        "a refusal wrote nothing"
    );

    for i in 1..MAX_DENIED {
        let put = Txn::new().put(format!("deny/node/n{i}"), "x", Cond::Any);
        s.fake.txn(put).expect("store").expect("put");
    }
    assert_eq!(m.denied().expect("list").len(), MAX_DENIED);
    assert!(matches!(
        m.deny(&named("one-too-many"), "full"),
        Err(MembershipError::Invalid(_))
    ));
    m.deny(&named("n1"), "listed already")
        .expect("a listed one");
    assert!(m.allow(&named("n2")).expect("allow"));
    m.deny(&named("one-too-many"), "room again").expect("room");
    assert_eq!(m.denied().expect("list").len(), MAX_DENIED);
}
