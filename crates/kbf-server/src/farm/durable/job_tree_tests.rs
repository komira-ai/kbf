//! The `job-tree/` row a frontend writes when it publishes the kbf base tree, read by a binary
//! that does not know the family: its start loads as if the row were not there (B8).

use kbf_state::fake::{Fake, Flaw};
use kbf_types::{ActionKey, DigestFunction, LeaseKind, Qos, Resources};

use super::*;
use crate::job_tree;

/// The prefix of every scan this binary makes of the state store (each `scan_prefix` of the
/// farm's rows, the members', the object store registry's, the enrollment registry's and the
/// handoff pins').
const FAMILIES: [&str; 27] = [
    "op/",
    "queue/",
    "loss/",
    "done/",
    "lease/",
    "cordon/",
    "cordon-owner/",
    "session/",
    "fate/",
    "floor/",
    "keyfloor/",
    "opkey/",
    "opwall/",
    "est/",
    "subject/",
    "member/",
    "member-owner/",
    "deny/",
    "unfilled/",
    "handoff/pin/",
    "instance/",
    "objstore/conn/",
    "objstore/bind/",
    "objstore/ack/",
    "enroll/token/",
    "enroll/node/",
    "enroll/updater/",
];

const ROOT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const ROW: &[u8] =
    br#"{"release":"0123456789abcdef0123456789abcdef01234567","first_published_ms":1700000000123}"#;

fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
        whole_worker: false,
        pin: None,
    }
}

fn store() -> Arc<Fake> {
    let clock = Arc::new(kbf_state::ManualClock::default());
    Arc::new(Fake::with_flaw(clock, Flaw::None))
}

fn put(store: &Fake, key: &str, value: &[u8]) {
    let put = Txn::new().put(key, value.to_vec(), Cond::Any);
    store.txn(put).expect("store").expect("put");
}

fn open(store: &Arc<Fake>) -> Durable {
    let store: Arc<dyn Store> = store.clone();
    Durable::open(store, EpochPolicy::Keep, 100)
        .expect("open")
        .0
}

/// Asserts that the row of a published base tree is in a family of its own: no scan of this
/// binary reads a prefix the key starts with, and a scan of each prefix over a store that holds
/// the row returns nothing. Catches: a key under `cordon/`, `lease/` or any family a frontend
/// that does not know of base trees would read, and refuse as a corrupt row, on every start.
#[test]
fn the_row_is_outside_every_family_a_scan_reads() {
    let key = job_tree::key(ROOT);
    let store = store();
    put(&store, &key, ROW);

    assert!(key.starts_with("job-tree/"));
    for family in FAMILIES {
        assert!(!key.starts_with(family), "{family}");
        let rows = store.scan_prefix(family).expect("a scan");
        assert!(rows.is_empty(), "{family}: {rows:?}");
    }
    let rows = store.scan_prefix(job_tree::PREFIX).expect("a scan");
    assert_eq!(rows.len(), 1);
}

/// Asserts that a start (the durable load of the farm's rows) and the search a worker's
/// arrival makes (the leases a node holds) read the same from a store with a `job-tree/` row as
/// from one without, rows of the farm's own beside it or none: the previous release's state
/// reader skips the record. Catches: a reader that refuses the row, or counts it as an operation,
/// a lease or a cordon.
#[test]
fn a_previous_releases_reader_skips_the_job_tree_row() {
    let without = store();
    let with = store();
    let submit = encode(FarmInput::Submit(request(1))).expect("encode");
    for store in [&without, &with] {
        put(store, "op/0000000000000001", &submit);
    }
    put(&with, &job_tree::key(ROOT), ROW);
    put(&with, &job_tree::key(&"f".repeat(64)), b"not even JSON");

    let (loaded_without, loaded_with) = (open(&without).load(), open(&with).load());
    let loaded = loaded_with.expect("a store with the rows loads");

    assert_eq!(
        loaded,
        loaded_without.expect("the store without them loads")
    );
    assert_eq!(loaded.operations.len(), 1);
    let worker = WorkerId::new("node-a");
    let held = |store| {
        open(store)
            .held_by(&worker)
            .expect("the search reads")
            .len()
    };
    assert_eq!(held(&with), held(&without));
    assert!(loaded.cordons.is_empty());
}
