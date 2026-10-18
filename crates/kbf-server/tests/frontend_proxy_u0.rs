//! Unit U0 of `docs/design/frontend-proxy.md`: the claims the design makes about the
//! state store and the frontend that the code can be asked about. The report is
//! `docs/design/frontend-proxy-u0.md`; each test names the claim it checks and the defect it
//! would catch.
//!
//! Tests that pass pin behaviour the design relies on. Tests marked
//! `#[ignore = "U0 red: ..."]` assert what the design says and FAIL today; run them with
//! `cargo test -p kbf-server --test frontend_proxy_u0 -- --ignored`. The unit that fixes the
//! behaviour removes the `ignore` in the same commit. They are `ignore`d, not deleted, so the
//! `state-postgres` job (which runs `kbf-state` with `--include-ignored`) and the workspace
//! run stay green while the design is still being implemented.

#[path = "support/hooked.rs"]
mod hooked;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_history::{IndexRoot, InvKey, WriterId};
use kbf_server::members::{
    Members, MembershipConfig, MembershipError, OLDEST_PROTOCOL, PROTOCOL, Progress, Registrant,
    Registration,
};
use kbf_server::{StateStoreUri, members_rpc};
use kbf_state::{Cond, Fenced, GENERATION, Store, StoreError, Txn};

fn registrant(instance: &str) -> Registrant {
    Registrant {
        instance_id: instance.to_owned(),
        addr: "192.0.2.1:8982".to_owned(),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(30),
        dwell: Duration::from_secs(120),
        replication: 2,
    }
}

/// A new, empty directory under cargo's test target directory.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("u0-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

// ---- (a) a fenced child survives -----------------------------------------------------

/// Design section 3 and 8: after `Fenced` the child "does not exit". Catches a membership
/// sweeper (the one background task `kbf --mode frontend` runs beside its listeners) that
/// returns or panics once its store handle is fenced: the frontend's `background` future
/// ending is the process leaving. The sweep logs the refusal and tries again at the next
/// beat; this drives it through five beats on a fenced handle.
#[tokio::test(start_paused = true)]
async fn the_membership_sweeper_outlives_its_fencing() {
    let store = Hooked::new();
    let old: Arc<dyn Store> =
        Arc::new(Fenced::take(Arc::clone(&store.fake) as Arc<dyn Store>, None).expect("gen 1"));
    let _new = Fenced::take(Arc::clone(&store.fake) as Arc<dyn Store>, Some(1)).expect("gen 2");
    let members = Arc::new(Members::new(old, config()));
    assert!(
        matches!(
            members.reap(),
            Err(kbf_server::members::MembershipError::Store(
                StoreError::Fenced
            ))
        ),
        "the fenced handle refuses the reap, as a value"
    );
    let beat = members.config().beat();
    let sweeping = tokio::spawn(members_rpc::sweep(Arc::clone(&members)));
    for _ in 0..5 {
        tokio::time::sleep(beat).await;
    }
    assert!(
        !sweeping.is_finished(),
        "the sweeper must outlive the fence"
    );
    sweeping.abort();
}

// ---- (c) which writes the old child may still make -----------------------------------

/// Design section 8, "Who may write which keys": the old child "appends" run history and
/// logs after takeover, under keys carrying its own id. The history index of the code is
/// written through the state store's `Fenced` handle (`HistoryFlags::open_with` clones
/// `state.store`), and `Fenced` appends a generation check to EVERY transaction, so these
/// appends are refused the moment a successor takes the generation. This is the production
/// path: `StateStoreUri::open`, a demote, `open_after_handover`.
#[test]
fn the_history_index_of_a_superseded_frontend_is_refused_by_the_production_handle() {
    let dir = scratch("history-fenced");
    let uri: StateStoreUri = format!("sqlite://{}/state.db", dir.display())
        .parse()
        .expect("a sqlite uri");
    let old = uri.open().expect("generation 1");
    let key = IndexRoot::default().ichunk(&InvKey::from_record_id("r"), WriterId(1), 1);
    old.store
        .txn(Txn::new().put(key.clone(), "row", Cond::Absent))
        .expect("store")
        .expect("the holder appends");
    old.store.demote().expect("hand over");
    let _new = uri.open_after_handover(1).expect("generation 2");
    let refused = old
        .store
        .txn(Txn::new().put(format!("{key}x"), "row", Cond::Absent));
    assert_eq!(
        refused.unwrap_err(),
        StoreError::Fenced,
        "the superseded handle refuses a history-index append"
    );
}

/// The design's claim as a requirement: a fenced child can still append rows under its own
/// writer id. FAILS today (the handle has no exempt family). Catches a U2 that adds the
/// sticky flag but forgets that history and logs ride the same handle: the old child's
/// open streams would then lose their run records.
#[test]
#[ignore = "U0 red: Fenced refuses every write, so section 8's 'old child appends history/log rows' has no code path; U2 adds an unfenced append handle"]
fn a_fenced_child_can_append_rows_under_its_own_writer_id() {
    let fake = Hooked::new();
    let old = Fenced::take(Arc::clone(&fake.fake) as Arc<dyn Store>, None).expect("gen 1");
    let _new = Fenced::take(Arc::clone(&fake.fake) as Arc<dyn Store>, Some(1)).expect("gen 2");
    let key = IndexRoot::default().ichunk(&InvKey::from_record_id("r"), WriterId(1), 1);
    let appended = old.txn(Txn::new().put(key, "row", Cond::Absent));
    assert!(
        matches!(appended, Ok(Ok(_))),
        "the design lets the old child append under its own writer id: {appended:?}"
    );
}

/// Design section 8 says each history row has one writer because "keys are prefixed by the
/// writing child's own instance id". In the code only the `ichunk` rows carry the writer
/// id; the invocation header, the list entries, the shape rows and the stream counter are
/// shared by every writer. Pins which key builders take a writer, so U2's table starts from
/// the code and not from the design's guess.
#[test]
fn only_the_history_chunk_rows_carry_the_writer_id() {
    let root = IndexRoot::default();
    let inv = InvKey::from_record_id("r");
    let (a, b) = (WriterId(0xa), WriterId(0xb));
    assert_ne!(root.ichunk(&inv, a, 1), root.ichunk(&inv, b, 1));
    assert!(root.ichunk(&inv, a, 1).contains(&a.to_string()));
    // Everything below is the same key whichever writer asks for it.
    assert!(root.invid(&inv).starts_with("ui/invid/"));
    assert_eq!(root.streams_counter(), "ui/streams");
    assert!(root.inv_list("main", 1, &inv).starts_with("ui/inv/"));
    assert!(root.shape_prefix("main").starts_with("ui/shape/"));
    assert!(
        !root.invid(&inv).contains(&a.to_string()),
        "ui/invid/<inv> is one row for every writer (written with Cond::Any for a header)"
    );
}

/// The families that membership writes to the store, from a registration on a fresh store.
/// The design's table names none of `instance/`, `member/`, `map`, `member-id`, `boot/`
/// (the code's module docs list them). Catches a U2 table built from the design's guess.
#[test]
fn membership_writes_these_key_families() {
    let store = Hooked::new();
    let members = Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config());
    let _: Registration = members.register(&registrant("node-1")).expect("register");
    let mut keys: Vec<String> = store
        .fake
        .scan_prefix("")
        .expect("scan")
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    keys.sort();
    let families: std::collections::BTreeSet<&str> = keys
        .iter()
        .map(|k| k.split('/').next().unwrap_or_default())
        .collect();
    assert_eq!(
        families.into_iter().collect::<Vec<_>>(),
        ["instance", "map", "member", "meta"],
        "rows: {keys:?}"
    );
    assert!(keys.contains(&"meta/members_formed".to_owned()), "{keys:?}");
}

// ---- (d) the fence is atomic with the write ------------------------------------------

/// Design section 8: "each fenced write does the check inside its transaction". `Fenced`
/// does that for `txn` and `apply_batch` (the check is the last operation of the batch's
/// transaction), but `reap_expired` reads the generation row with a separate `get` and then
/// writes. Catches the check-then-act window the design names: A reads its generation, B
/// takes, A still commits (here: the Expire rows of A's reap). The hook runs B's takeover
/// right after A's read of the generation row.
#[test]
#[ignore = "U0 red: Fenced::reap_expired checks the generation with a separate get, so a takeover between the get and the reap does not stop it; U2 makes the reap one checked transaction"]
fn a_reap_cannot_pass_the_fence_check_and_then_commit_after_a_takeover() {
    let store = Hooked::new();
    let fake = Arc::clone(&store.fake);
    let old = Fenced::take(Arc::clone(&store) as Arc<dyn Store>, None).expect("gen 1");
    fake.txn(Txn::new().put_ttl("session/w", "v", Duration::from_secs(10), Cond::Any))
        .expect("store")
        .expect("put");
    // B's takeover runs between A's read of the generation row and A's reap, and by then the
    // row has expired, so A's reap has a row to remove.
    let (successor, clock) = (Arc::clone(&store.fake), Arc::clone(&store.clock));
    store.after_get(GENERATION, move || {
        Fenced::take(successor as Arc<dyn Store>, Some(1)).expect("B takes generation 2");
        clock.advance(Duration::from_secs(60));
    });
    let reaped = old.reap_expired();
    assert_eq!(
        reaped,
        Err(StoreError::Fenced),
        "A's reap removed a row after B took the generation: {reaped:?}"
    );
}

/// The same race for an ordinary write: the transaction carries the check, so a takeover
/// in the middle cannot let it through. Holds today; pins that `txn` stays atomic.
#[test]
fn an_ordinary_write_cannot_pass_a_takeover() {
    let store = Hooked::new();
    let old = Fenced::take(Arc::clone(&store) as Arc<dyn Store>, None).expect("gen 1");
    let _new = Fenced::take(Arc::clone(&store.fake) as Arc<dyn Store>, Some(1)).expect("gen 2");
    assert_eq!(
        old.txn(Txn::new().put("op/1", "v", Cond::Any)).unwrap_err(),
        StoreError::Fenced
    );
    assert_eq!(store.fake.get("op/1").expect("read"), None);
}

// ---- (e) the member-map floor of a fresh store -----------------------------------------

/// Design section 9 (the fix of U9): the first map of an empty store carries the OLDEST
/// protocol the build accepts, not the build's own `PROTOCOL`, so a fresh store plus the nodes
/// of release N re-enrolling against a frontend that speaks one number higher locks nobody out
/// (this test replaced one that pinned the first map's floor at `PROTOCOL`). Every constant is
/// 1 today, so the build is stood in for: it accepts from protocol 2 (and `PROTOCOL` is 1).
/// A node that speaks 2 is in and the map's floor is 2; a node that speaks 1 is refused,
/// naming 2. Catches the floor of the first map, or the verdict of `register`, coming from the
/// constant (1) and not from the build's oldest.
#[test]
fn a_fresh_stores_first_map_floor_is_the_oldest_protocol_the_build_accepts() {
    let store = Hooked::new();
    let members = Members::with_oldest_protocol(
        Arc::clone(&store.fake) as Arc<dyn Store>,
        config(),
        PROTOCOL + 1,
    );
    let at = Registrant {
        protocol: PROTOCOL + 1,
        ..registrant("node-1")
    };
    let first = members.register(&at).expect("register");
    assert_eq!(first.map.min_protocol(), PROTOCOL + 1);
    let refused = members
        .register(&registrant("node-2"))
        .expect_err("below the floor");
    assert!(
        matches!(
            refused,
            MembershipError::Protocol { got, min } if (got, min) == (PROTOCOL, PROTOCOL + 1)
        ),
        "{refused}"
    );
}

/// The same fact for a build as it ships, with its own constants: the first map's floor is
/// `OLDEST_PROTOCOL`, and a node below it is refused naming it. Equal to `PROTOCOL` today, so
/// it cannot tell the two apart yet; it goes red at the first release that raises `PROTOCOL`
/// and leaves `OLDEST_PROTOCOL` where it is, if `Members::new` is ever given the wrong one.
#[test]
fn a_build_as_shipped_floors_its_first_map_at_its_oldest_protocol() {
    let store = Hooked::new();
    let members = Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config());
    let first = members.register(&registrant("node-1")).expect("register");
    assert_eq!(first.map.min_protocol(), OLDEST_PROTOCOL);
    let below = Registrant {
        protocol: OLDEST_PROTOCOL - 1,
        ..registrant("node-2")
    };
    let refused = members.register(&below).expect_err("below the floor");
    assert!(
        matches!(
            refused,
            MembershipError::Protocol { got, min }
                if (got, min) == (OLDEST_PROTOCOL - 1, OLDEST_PROTOCOL)
        ),
        "{refused}"
    );
}
