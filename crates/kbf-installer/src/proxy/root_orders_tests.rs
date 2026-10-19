//! The real `RootOrders`: what it reads from `status.json` and where its writes land.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use kbf_update::record::file_name;

use super::api::traits::OrderWriter;
use super::model::{Locator, StagedFacts, Verb};
use super::orders::{EPOCH, OrderSpool};
use super::ports::RootOrders;
use super::root_orders::{MAX_STATUS, SpoolRootOrders};
use crate::CommitId;
use crate::helper::memory::{Memory, StagedFacts as RootFacts};
use crate::helper::{order, status};
use crate::state::State;

struct Rig {
    orders: PathBuf,
    status: PathBuf,
    spool: OrderSpool,
    adapter: SpoolRootOrders,
}

fn rig(name: &str) -> Rig {
    let root = kbf_host::testkit::scratch(&format!("proxy-root-orders-{name}"));
    let (orders, state) = (root.join("orders"), root.join("state"));
    fs::create_dir(&state).expect("state dir");
    let status = root.join("status.json");
    let spool = OrderSpool::open(&orders, &state).expect("open");
    let adapter = SpoolRootOrders::new(spool.clone(), &status);
    Rig {
        orders,
        status,
        spool,
        adapter,
    }
}

fn commit(c: char) -> CommitId {
    CommitId::new(&c.to_string().repeat(40)).expect("a commit")
}

fn put(rig: &Rig, json: &str) {
    fs::write(&rig.status, json).expect("status.json");
}

/// A status file with an installed commit `a`, a previous `c` and a staged `b` with facts.
fn staged_json() -> String {
    format!(
        "{{\"schema\":2,\"installed_commit\":\"{}\",\"previous_commit\":\"{}\",\
         \"staged_commit\":\"{}\",\"state\":\"staged\",\"refusal\":\"\",\"acked_order_id\":4,\
         \"staged\":{{\"protocol\":7,\"min_protocol\":5,\"state_schema\":3,\"profile\":\"frontend\"}},\
         \"envoy\":{{}}}}",
        commit('a').as_str(),
        commit('c').as_str(),
        commit('b').as_str(),
    )
}

/// Catches `status.json` with no release staged being read as one (a swap that goes on to
/// bake nothing).
///
/// Mutants planted: the commit of the facts read from `installed_commit`; `staged` always `None`
/// (the latter is also caught by the next test).
#[test]
fn staged_is_none_when_status_names_no_staged_release() {
    let rig = rig("no-staged");
    put(
        &rig,
        &format!(
            "{{\"schema\":2,\"installed_commit\":\"{}\",\"state\":\"idle\",\"staged\":null}}",
            commit('a').as_str()
        ),
    );
    assert_eq!(rig.adapter.staged(), Ok(None));
}

/// Catches the facts being taken from the wrong field: the commit must be `staged_commit`
/// (not the installed or previous one) and the numbers must be the three of `staged{}`.
///
/// Mutants planted: the commit read from `installed_commit`; `staged` returning `None`.
#[test]
fn staged_is_the_staged_commit_with_its_facts() {
    let rig = rig("staged");
    put(&rig, &staged_json());
    assert_eq!(
        rig.adapter.staged(),
        Ok(Some(StagedFacts {
            commit: commit('b'),
            protocol: 7,
            min_protocol: 5,
            state_schema: 3,
            profile: Some("frontend".into()),
        }))
    );
}

/// Catches `installed` ignoring the file, or reading the staged or previous commit: after
/// a `confirm` landed it is the one fact that says so.
///
/// Mutants planted: `installed` returning `None` always; reading `staged_commit`.
#[test]
fn installed_is_the_installed_commit() {
    let rig = rig("installed");
    put(&rig, &staged_json());
    assert_eq!(rig.adapter.installed(), Ok(Some(commit('a'))));
    put(&rig, "{\"state\":\"idle\",\"installed_commit\":null}");
    assert_eq!(rig.adapter.installed(), Ok(None));
}

/// Catches a cached read: the file is read again on every call, so a rewrite shows.
///
/// Planted `installed` mutants (always `None`, reading `staged_commit`) also go red here.
#[test]
fn every_call_reads_the_file_again() {
    let rig = rig("fresh");
    assert_eq!(rig.adapter.installed(), Ok(None), "missing file first");
    put(&rig, &staged_json());
    assert_eq!(rig.adapter.installed(), Ok(Some(commit('a'))));
    put(
        &rig,
        &staged_json().replace(commit('a').as_str(), commit('d').as_str()),
    );
    assert_eq!(rig.adapter.installed(), Ok(Some(commit('d'))));
}

/// Catches a missing file being an error (a host whose root half has not written yet is
/// not a failed swap).
///
/// Mutant planted: `NotFound` falling into the error arm.
#[test]
fn a_missing_file_reads_as_nothing() {
    let rig = rig("missing");
    assert_eq!(rig.adapter.staged(), Ok(None));
    assert_eq!(rig.adapter.installed(), Ok(None));
}

/// Catches a malformed file being read as empty (the swap would then see "not confirmed"
/// and could revert a confirmed release) or as a guess.
///
/// Mutant planted: a parse error mapped to `Ok(None)`.
#[test]
fn a_malformed_file_is_an_error() {
    let rig = rig("malformed");
    for text in [
        "{",
        "",
        "null",
        "{\"installed_commit\":\"abc\"}",
        "{\"installed_commit\":7}",
        "{\"state\":\"idle\",\"staged_commit\":\"B\"}",
    ] {
        put(&rig, text);
        let err = rig.adapter.installed().expect_err(text);
        assert!(err.0.contains("does not parse"), "{text:?}: {err}");
        assert!(rig.adapter.staged().is_err(), "{text:?}");
    }
}

/// Catches the cap being off by one in either direction: a file of exactly `MAX_STATUS`
/// bytes is read, one byte more is refused without being parsed.
///
/// Mutants planted: `>` turned `>=`; `take(MAX_STATUS)` without the extra byte.
#[test]
fn the_file_is_size_capped() {
    let rig = rig("cap");
    let body = "{\"installed_commit\":null}";
    let pad = |len: u64| {
        let mut text = body.to_owned();
        text.push_str(&" ".repeat(usize::try_from(len).expect("fits") - body.len()));
        text
    };
    put(&rig, &pad(MAX_STATUS));
    assert_eq!(rig.adapter.installed(), Ok(None));
    put(&rig, &pad(MAX_STATUS + 1));
    let err = rig.adapter.installed().expect_err("over the cap");
    assert!(err.0.contains("is over"), "{err}");
    assert!(rig.adapter.staged().is_err());
}

/// Catches an unreadable file (not just a missing one) being treated as absent: a status
/// path that is a directory fails at the read, a symlink loop fails at the open.
///
/// Mutants planted: the open's error arm returning `Ok(None)`; the read error dropped.
#[test]
fn an_unreadable_file_is_an_error() {
    let rig = rig("unreadable");
    fs::create_dir(&rig.status).expect("a directory where the file belongs");
    let err = rig.adapter.installed().expect_err("a directory");
    assert!(err.0.starts_with("cannot read "), "{err}");
    fs::remove_dir(&rig.status).expect("remove");
    symlink(&rig.status, &rig.status).expect("loop");
    let err = rig.adapter.staged().expect_err("a loop");
    assert!(err.0.starts_with("cannot read "), "{err}");
}

/// Catches the adapter and the root half's writer disagreeing on the shape: the bytes
/// `helper/status.rs` renders are read back as the commit and facts it was given.
///
/// Mutants planted: the staged and installed mutants above go red here too.
#[test]
fn the_root_halfs_own_rendering_reads_back() {
    let rig = rig("render");
    let state = State {
        current: Some(commit('a')),
        staged: Some(commit('b')),
        ..State::default()
    };
    let memory = Memory {
        staged: Some(RootFacts {
            commit: commit('b'),
            protocol: 9,
            min_protocol: 4,
            state_schema: 2,
        }),
        ..Memory::default()
    };
    fs::write(&rig.status, status::render(&state, &memory)).expect("status.json");
    assert_eq!(rig.adapter.installed(), Ok(Some(commit('a'))));
    assert_eq!(
        rig.adapter.staged(),
        Ok(Some(StagedFacts {
            commit: commit('b'),
            protocol: 9,
            min_protocol: 4,
            state_schema: 2,
            profile: None,
        }))
    );
}

fn read_order(rig: &Rig, id: u64) -> order::Order {
    let bytes = fs::read(rig.orders.join(file_name(EPOCH, id))).expect("the record");
    order::parse(EPOCH, id, &bytes).expect("the root half's reader takes it")
}

/// Catches a write that does not reach `orders/`, or reaches it with another verb or
/// commit, and ids that are not from the one counter the spool clone shares.
///
/// Mutants planted: `confirm` writing a revert; `revert` writing a confirm; `confirm` and
/// `revert` each returning `Ok(())` unwritten.
#[test]
fn confirm_and_revert_are_written_through_the_shared_spool() {
    let mut rig = rig("writes");
    let first = rig
        .spool
        .write(Verb::FrontendUpdate, &commit('a'), 900)
        .expect("through the clone");
    rig.adapter.confirm(&commit('b')).expect("confirm");
    rig.adapter.revert(&commit('c')).expect("revert");
    let ids: Vec<u64> = [first.order_id]
        .into_iter()
        .chain((first.order_id + 1)..(first.order_id + 1_000_000))
        .filter(|id| rig.orders.join(file_name(EPOCH, *id)).is_file())
        .collect();
    assert_eq!(ids.len(), 3, "{ids:?}");
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "one id space: {ids:?}");
    let (confirm, revert) = (read_order(&rig, ids[1]), read_order(&rig, ids[2]));
    assert_eq!(
        (confirm.verb, confirm.commit),
        (order::Verb::Confirm, commit('b'))
    );
    assert_eq!(
        (revert.verb, revert.commit),
        (order::Verb::Revert, commit('c'))
    );
    assert_eq!((confirm.window_s, revert.window_s), (0, 0));
}

/// Catches `frontend_update` not reaching the spool: a locator the spool issued whose
/// record was consumed is written again, and one it never issued is refused.
///
/// Mutant planted: `frontend_update` returning `Ok(())`.
#[test]
fn frontend_update_goes_to_the_spool() {
    let mut rig = rig("frontend");
    let locator: Locator = rig
        .spool
        .write(Verb::FrontendUpdate, &commit('a'), 900)
        .expect("issued");
    let path = rig.orders.join(file_name(EPOCH, locator.order_id));
    fs::remove_file(&path).expect("the root half took it");
    rig.adapter
        .frontend_update(&locator)
        .expect("written again");
    let again = read_order(&rig, locator.order_id);
    assert_eq!(
        (again.verb, again.commit, again.window_s),
        (order::Verb::FrontendUpdate, commit('a'), 900)
    );
    let not_issued = Locator {
        order_id: locator.order_id + 10_000_000,
        ..locator
    };
    assert!(rig.adapter.frontend_update(&not_issued).is_err());
}
