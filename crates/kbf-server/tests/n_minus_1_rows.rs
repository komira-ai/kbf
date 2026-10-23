//! Rows of the state store across builds: every format changes additively, so a build reads
//! the rows the build before it wrote, and the build before it reads the rows this one wrote
//! (docs/design/kbf-cicd.md, section 9). A row that holds a field its reader does not name
//! reads as the row without it.
//!
//! `tests/golden/rows/` holds the bytes of one member row, as a build wrote it, as hex text.
//! Like the corpus of `kbf-log`, it is append-only: a file is added when a release adds a
//! row, and one that exists is never rewritten or deleted. The test of the directory fails on
//! a file added or deleted, and the rule that a file is not edited is held by review. The
//! released state stores themselves are in `tests/golden/state/` (`state_golden.rs`).

#[path = "support/hooked.rs"]
mod hooked;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_map::{Member, MemberId, MemberState};
use kbf_server::members::{
    Fill, Members, MembershipConfig, NodeFacts, PROTOCOL, Progress, Registrant, Registration,
};
use kbf_server::storage_health::Reported;
use kbf_state::{Cond, Store, Txn};
use prost::Message as _;

const TTL: Duration = Duration::from_secs(30);

/// Two fields no build names in any of these rows yet: number 20, a varint, 42; and number 21,
/// a string, "hi".
const LATER: [u8; 8] = [0xa0, 0x01, 0x2a, 0xaa, 0x01, 0x02, 0x68, 0x69];

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/rows")
}

fn golden(name: &str) -> Vec<u8> {
    let path = golden_dir().join(format!("{name}.hex"));
    let text = std::fs::read_to_string(&path);
    let text = text.unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    hex::decode(text.trim()).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn put_raw(store: &Hooked, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.fake.txn(put).expect("store").expect("no conflict");
}

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
        protocol: PROTOCOL,
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

/// A member row with its messages as bytes, so that a test can add to them, and the numbers
/// the row has no field for yet.
#[derive(Clone, PartialEq, prost::Message)]
struct RawRow {
    #[prost(bytes = "vec", optional, tag = "1")]
    member: Option<Vec<u8>>,
    #[prost(uint64, tag = "2")]
    boot: u64,
    #[prost(bytes = "vec", optional, tag = "3")]
    report: Option<Vec<u8>>,
    #[prost(uint32, tag = "4")]
    protocol: u32,
    #[prost(uint64, tag = "5")]
    planned_until_nanos: u64,
    #[prost(bytes = "vec", optional, tag = "6")]
    fill: Option<Vec<u8>>,
    #[prost(uint32, optional, tag = "7")]
    runs_log: Option<u32>,
    #[prost(uint32, optional, tag = "8")]
    log_version: Option<u32>,
    #[prost(string, optional, tag = "9")]
    build_commit: Option<String>,
    #[prost(uint64, tag = "20")]
    later_number: u64,
    #[prost(string, tag = "21")]
    later_text: String,
}

/// Rewrites the row of `id` as a later build would write it: with a field the row has no tag
/// for, and the same inside the member it holds.
fn add_later_fields(store: &Hooked, id: MemberId) {
    let key = format!("member/{}", id.0);
    let row = store.fake.get(&key).expect("store").expect("a live row");
    let mut raw = RawRow::decode(row.value.as_slice()).expect("a row");
    let mut member = raw.member.take().expect("a member");
    member.extend_from_slice(&LATER);
    raw.member = Some(member);
    raw.later_number = 42;
    raw.later_text = "hi".to_owned();
    let value = raw.encode_to_vec();
    let cond = Cond::Version(row.version);
    let put = Txn::new().put_ttl(&key, value, TTL, cond);
    store.fake.txn(put).expect("store").expect("no conflict");
}

/// Asserts that a member row holding fields this build does not name, in the row and in the
/// member inside it, reads as the row without them: the facts, the protocol and the member in
/// the map are the ones the node registered with, the map keeps its generation, and a
/// heartbeat of the member goes through and leaves it active.
#[test]
fn a_member_row_with_fields_no_build_names_yet_reads_as_without_them() {
    let s = Hooked::new();
    let members = build(&s);
    let a = join(&members, 1, &said(5, 1, "abc"));
    add_later_fields(&s, a.id);
    assert_eq!(facts_of(&members, a.id), said(5, 1, "abc"));
    let protocols = members.protocols().expect("rows");
    assert_eq!(protocols, BTreeMap::from([(a.id, Some(PROTOCOL))]));
    let map = members.refresh().expect("a refresh").expect("a map");
    assert_eq!(map.generation(), a.map.generation());
    beat(&members, a.id, a.boot, &said(5, 1, "abc"));
    assert_eq!(facts_of(&members, a.id), said(5, 1, "abc"));
    let map = members.refresh().expect("a refresh").expect("a map");
    let state = map.member(a.id).map(|m| m.state);
    assert_eq!(state, Some(MemberState::Active));
}

/// Asserts that the member row of the golden corpus, as a build wrote it, reads as the member
/// it was written from: the facts the node said, no protocol (the row names none), and an
/// active member in the map, with the boot of the row.
#[test]
fn the_golden_member_row_reads_as_the_member_it_was_written_from() {
    let s = Hooked::new();
    put_raw(&s, "member/1", golden("member-row"));
    let members = build(&s);
    let facts = members.node_facts().expect("rows");
    let want = BTreeMap::from([(MemberId(1), said(5, 1, "abc"))]);
    assert_eq!(facts, want);
    let protocols = members.protocols().expect("rows");
    assert_eq!(protocols, BTreeMap::from([(MemberId(1), None)]));
    let map = members.refresh().expect("a refresh").expect("a map");
    let member = Member {
        id: MemberId(1),
        addr: "192.0.2.1:8982".to_owned(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
        boot: 3,
    };
    assert_eq!(map.member(MemberId(1)), Some(&member));
}

/// Asserts that the golden directory holds the file the tests of this file read and no
/// other: a file added without a test that reads it, and a file deleted, each fail here.
#[test]
fn the_golden_rows_directory_holds_exactly_the_files_the_tests_read() {
    let mut found: Vec<String> = std::fs::read_dir(golden_dir())
        .expect("the golden directory")
        .map(|entry| entry.expect("an entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    found.sort();
    assert_eq!(found, ["member-row.hex"]);
}
