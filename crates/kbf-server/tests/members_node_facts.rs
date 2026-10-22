//! What a storage node says of the build it runs and of its metadata log, as its member row
//! keeps it (`Members::register_reporting`, `Members::heartbeat_reporting`,
//! `Members::node_facts`; `GET /v1/members` shows them, `api_members.rs`): the newest log
//! version its build can raise the log to, the version the log is at, and the commit built.
//! Nothing decides on them yet.

#[path = "support/hooked.rs"]
mod hooked;

use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_map::{MemberId, MemberState};
use kbf_server::members::{
    Fill, MAX_BUILD_COMMIT, Members, MembershipConfig, NodeFacts, Progress, Registrant,
    Registration,
};
use kbf_server::storage_health::Reported;
use kbf_state::fake::Fake;
use kbf_state::{Cond, Store, Txn};
use prost::Message as _;

const TTL: Duration = Duration::from_secs(30);

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: TTL,
        dwell: Duration::from_secs(120),
        replication: 2,
    }
}

fn build(store: &Arc<Hooked>) -> Members {
    Members::new(Arc::clone(store) as Arc<dyn Store>, config())
}

/// Storage node `i`, whose index is whole.
fn node(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: kbf_server::members::PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// What a node that runs log version `runs_log`, has its log at `log_version` and was built
/// from `commit` says.
fn said(runs_log: u32, log_version: u32, commit: &str) -> NodeFacts {
    NodeFacts {
        runs_log: Some(runs_log),
        log_version: Some(log_version),
        build_commit: Some(commit.to_owned()),
    }
}

/// Storage node `i` registers saying `facts`.
fn join(members: &Members, i: u32, facts: &NodeFacts) -> Registration {
    members
        .register_reporting(&node(i), None, facts)
        .expect("registers")
}

/// The member `id` beats saying `facts`.
fn beat(members: &Members, id: MemberId, boot: u64, facts: &NodeFacts) {
    members
        .heartbeat_reporting(
            id,
            boot,
            Progress::CaughtUp,
            Reported::default(),
            (Fill::default(), facts),
            None,
        )
        .expect("beats");
}

/// What the row of `id` holds.
fn facts_of(members: &Members, id: MemberId) -> NodeFacts {
    let mut all = members.node_facts().expect("rows");
    all.remove(&id).expect("a live row")
}

/// A member row as a build that predates the node facts wrote it: tags 1 to 6 only.
#[derive(Clone, PartialEq, prost::Message)]
struct RowBeforeFacts {
    #[prost(message, optional, tag = "1")]
    member: Option<kbf_proto::kbf::map::v1::Member>,
    #[prost(uint64, tag = "2")]
    boot: u64,
    /// The debts report, whose layout this test does not read.
    #[prost(bytes = "vec", optional, tag = "3")]
    report: Option<Vec<u8>>,
    #[prost(uint32, tag = "4")]
    protocol: u32,
    #[prost(uint64, tag = "5")]
    planned_until_nanos: u64,
    /// The handoff report, whose layout this test does not read.
    #[prost(bytes = "vec", optional, tag = "6")]
    fill: Option<Vec<u8>>,
}

/// Rewrites the row of `id` as a frontend built before the node facts does when it beats or
/// changes a state: the fields it knows, and no more.
fn rewrite_as_an_older_frontend(fake: &Fake, id: MemberId) {
    let key = format!("member/{}", id.0);
    let row = fake.get(&key).expect("store").expect("a live row");
    let old = RowBeforeFacts::decode(row.value.as_slice()).expect("a row");
    assert_ne!(old.encode_to_vec(), row.value, "the facts are on the row");
    fake.txn(Txn::new().put_ttl(&key, old.encode_to_vec(), TTL, Cond::Version(row.version)))
        .expect("store")
        .expect("no conflict");
}

/// Catches: the facts moved to other tags of the stored row, or to other wire types. The
/// rows live in the store across builds, so the numbers one build writes are the numbers
/// the next reads; only bytes spelled out by hand pin that, because a test that writes and
/// reads through the same `Row` follows a tag wherever it goes. The facts are tags 7 and 8
/// as plain varints (key bytes `7 << 3 | 0`, 0x38, and 0x40) and tag 9 as a string (key
/// byte `9 << 3 | 2`, 0x4a), after the fields of the older layout.
#[test]
fn the_facts_are_tags_seven_to_nine_of_the_stored_row() {
    let s = Hooked::new();
    let members = build(&s);
    let a = join(&members, 1, &said(5, 1, "abc"));
    let key = format!("member/{}", a.id.0);
    let row = s.fake.get(&key).expect("store").expect("a live row");
    let tail = [0x38, 5, 0x40, 1, 0x4a, 3, b'a', b'b', b'c'];
    assert!(row.value.ends_with(&tail), "{:?}", row.value);
}

/// Catches: a fact of 0 read as absent, and an absent one read as 0 (a node that says
/// nothing of its log would show a log at version 0, below every real version, and would
/// count as having reported). A node that says 0, 0 and an empty commit has said so; a node
/// that says nothing has said nothing. Mutants: a row that stores the facts as plain
/// numbers (0 for none); a read that maps 0 to `None`.
#[test]
fn a_fact_of_zero_is_a_fact_and_a_missing_one_is_none() {
    let s = Hooked::new();
    let members = build(&s);
    let zero = join(&members, 1, &said(0, 0, ""));
    let silent = join(&members, 2, &NodeFacts::default());
    let all = members.node_facts().expect("rows");
    assert_eq!(all[&zero.id], said(0, 0, ""));
    assert_eq!(all[&silent.id], NodeFacts::default());
}

/// Catches: a heartbeat that replaces the facts a registration wrote (a node's build cannot
/// change without a restart, so what it says again is the same, and what a newer frontend
/// keeps is the registration's), one that leaves the log version where it was (a log is
/// raised while the node runs, and the view must follow), and a state change that rewrites
/// the row without the facts. The beat here says 9 and "xyz" against the registration's 5 and
/// "abc": the row keeps 5 and "abc" and takes the 4, then the 5, the log was raised to.
#[test]
fn a_heartbeat_updates_the_log_version_and_keeps_the_rest_as_does_a_state_change() {
    let s = Hooked::new();
    let members = build(&s);
    let a = join(&members, 1, &said(5, 1, "abc"));
    join(&members, 2, &said(5, 1, "abc"));
    beat(&members, a.id, a.boot, &said(9, 4, "xyz"));
    assert_eq!(facts_of(&members, a.id), said(5, 4, "abc"));

    members
        .set_state(a.id, MemberState::Draining)
        .expect("draining");
    assert_eq!(facts_of(&members, a.id), said(5, 4, "abc"));

    beat(&members, a.id, a.boot, &said(5, 5, "abc"));
    assert_eq!(facts_of(&members, a.id), said(5, 5, "abc"));
}

/// Catches: a registration that merges its facts into the row of an earlier boot instead of
/// replacing them. A node restarted on a newer build shows that build and its log; a node
/// restarted on an older one, which says none of them, shows none (it must not keep the
/// numbers of the build it replaced: they would claim a log version the node cannot
/// raise to).
#[test]
fn a_registration_replaces_the_facts() {
    let s = Hooked::new();
    let members = build(&s);
    let first = join(&members, 1, &said(4, 4, "abc"));
    let second = join(&members, 1, &said(5, 4, "def"));
    assert_eq!((second.id, second.boot > first.boot), (first.id, true));
    assert_eq!(facts_of(&members, first.id), said(5, 4, "def"));

    join(&members, 1, &NodeFacts::default());
    assert_eq!(facts_of(&members, first.id), NodeFacts::default());
}

/// Catches: a node that says nothing read as 0, and a row an older frontend rewrote (a
/// frontend built before the tags drops them when it beats or changes a state) that never
/// recovers. The facts of that row read `None` (and a beat that says nothing brings
/// nothing back); the node's next heartbeat to a frontend that knows the tags brings all
/// three back, so the hazard lasts one beat. Mutant: a heartbeat that keeps the row's facts
/// without filling the ones it lacks.
#[test]
fn a_row_an_older_frontend_rewrote_reads_none_until_the_next_heartbeat() {
    let s = Hooked::new();
    let members = build(&s);
    let a = join(&members, 1, &said(5, 1, "abc"));
    rewrite_as_an_older_frontend(&s.fake, a.id);
    assert_eq!(facts_of(&members, a.id), NodeFacts::default());

    members
        .heartbeat(a.id, a.boot, Progress::CaughtUp, Reported::default(), None)
        .expect("beats");
    assert_eq!(facts_of(&members, a.id), NodeFacts::default());

    beat(&members, a.id, a.boot, &said(5, 4, "abc"));
    assert_eq!(facts_of(&members, a.id), said(5, 4, "abc"));
}

/// Catches: a `build_commit` stored without a bound (a node is trusted with its own
/// version, not with the size of a row every frontend reads at every refresh), a bound
/// that is off by one, and a cut that lands inside a character (the cut of a string by
/// bytes panics there). A commit of 100 bytes keeps 64, one of 64 keeps all 64, and
/// "c" and 40 two-byte characters (81 bytes) keeps 63: byte 64 is inside a character.
#[test]
fn a_build_commit_is_cut_to_64_bytes_at_a_character_boundary() {
    let s = Hooked::new();
    let members = build(&s);
    assert_eq!(MAX_BUILD_COMMIT, 64);
    let wide = format!("c{}", "é".repeat(40));
    let cut = format!("c{}", "é".repeat(31));
    let cases = [
        ("a".repeat(100), "a".repeat(64)),
        ("b".repeat(64), "b".repeat(64)),
        (wide, cut),
    ];
    for (i, (sent, kept)) in (1..).zip(cases) {
        let joined = join(&members, i, &said(5, 1, &sent));
        let stored = facts_of(&members, joined.id).build_commit;
        assert_eq!(stored, Some(kept), "{sent:?}");
    }
}

/// Catches: facts listed for a member whose row has lapsed (`Down`): its row, which held
/// them, expired with its heartbeats, so a reader treats it as not known, as for the
/// protocol. Only the member that kept beating is listed.
#[test]
fn a_member_whose_row_lapsed_has_no_facts() {
    let s = Hooked::new();
    let members = build(&s);
    let a = join(&members, 1, &said(5, 1, "abc"));
    join(&members, 2, &said(5, 1, "abc"));
    s.clock.advance(TTL - Duration::from_secs(1));
    beat(&members, a.id, a.boot, &said(5, 1, "abc"));
    s.clock.advance(Duration::from_secs(2));
    let listed: Vec<MemberId> = members.node_facts().expect("rows").into_keys().collect();
    assert_eq!(listed, [a.id]);
}
