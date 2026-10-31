//! Who owns a node's cordon, as `GET /v1/nodes/{id}` reads it from the `cordon-owner/<node>`
//! row. Nothing writes the row yet: the tests put it into the store by hand, and check that
//! the farm's own placements write what they wrote before. On the js core (the
//! `farm-core-js` feature) the same tests read it through that core's farm.

use kbf_server::farm::NodeAction;
use kbf_server::fleet::{NodeDetail, Owner, PlacementView};

use super::*;

/// Puts `value` in the row `key` of the store, as a writer that records owners would.
fn put(disk: &Disk, key: &str, value: &[u8]) {
    let txn = Txn::new().put(key.to_owned(), value.to_vec(), kbf_state::Cond::Any);
    disk.store.txn(txn).expect("store").expect("put");
}

/// The node's detail as `farm` answers it.
fn detail(farm: &TestFarm, worker: &WorkerId) -> NodeDetail {
    let read = farm.node_detail(worker).expect("readable");
    read.expect("a node the farm knows")
}

/// A store with two frontends, `node-b` connected to the second, and the first having
/// cordoned it. The inbox is kept open for as long as the value lives.
struct Cordoned {
    disk: Disk,
    through: TestFarm,
    own: TestFarm,
    node: WorkerId,
    _inbox: Inbox,
}

fn cordoned() -> Cordoned {
    let disk = Disk::new();
    let node = WorkerId::new("node-b");
    let (through, own) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let (_, inbox) = register_as(&own, &node);
    through
        .place(&node, NodeAction::Cordon)
        .expect("a node of another frontend is placed");
    Cordoned {
        disk,
        through,
        own,
        node,
        _inbox: inbox,
    }
}

/// Asserts that the frontend a node is connected to shows the owner a `cordon-owner/` row
/// names, with the cordon as it was. Catches: an owner read from nowhere, or a reader that
/// takes the row for the cordon's own.
#[tokio::test(start_paused = true)]
async fn a_row_with_an_owner_shows_that_owner() {
    let c = cordoned();
    put(&c.disk, "cordon-owner/node-b", b"updater:kbf-daemon");

    let read = detail(&c.own, &c.node);

    assert_eq!(read.owner, Some(Owner::Updater("kbf-daemon".to_owned())));
    assert_eq!(read.node.placement, PlacementView::Cordoned);
}

/// Asserts that a row naming the operator shows the operator.
#[tokio::test(start_paused = true)]
async fn a_row_naming_the_operator_shows_the_operator() {
    let c = cordoned();
    put(&c.disk, "cordon-owner/node-b", b"operator");

    assert_eq!(detail(&c.own, &c.node).owner, Some(Owner::Operator));
}

/// Asserts that a cordon with no owner row, which is every cordon written today, is the
/// operator's, and shown so. Catches: an absent row read as an error or as no owner.
#[tokio::test(start_paused = true)]
async fn a_cordon_with_no_owner_row_is_the_operators() {
    let c = cordoned();

    let read = detail(&c.own, &c.node);

    assert_eq!(read.owner, Some(Owner::Operator));
    assert_eq!(read.node.placement, PlacementView::Cordoned);
}

/// Asserts that a row another frontend's node is read through shows the owner too, on the
/// frontend that does not hold the node (answered from the store). The js core does not
/// answer a node it does not hold.
#[tokio::test(start_paused = true)]
#[cfg_attr(
    feature = "farm-core-js",
    ignore = "known-different-on-js: the detail of a node another frontend holds is not answered on the js core"
)]
async fn a_frontend_that_does_not_hold_the_node_shows_the_owner_too() {
    let c = cordoned();
    put(&c.disk, "cordon-owner/node-b", b"updater:kbf-daemon");

    let read = detail(&c.through, &c.node);

    assert_eq!(read.owner, Some(Owner::Updater("kbf-daemon".to_owned())));
}

/// Asserts that a node that is serving shows no owner, whatever row is left under its name.
/// Catches: a stale row of a cordon that was lifted, shown as the owner of nothing.
#[tokio::test(start_paused = true)]
async fn a_serving_node_shows_no_owner() {
    let c = cordoned();
    put(&c.disk, "cordon-owner/node-b", b"updater:kbf-daemon");
    c.through
        .place(&c.node, NodeAction::Uncordon)
        .expect("returned");

    let read = detail(&c.own, &c.node);

    assert_eq!(read.node.placement, PlacementView::Serving);
    assert_eq!(read.owner, None);
}

/// Asserts that a drain has an owner as a cordon does.
#[tokio::test(start_paused = true)]
async fn a_draining_node_shows_its_owner() {
    let c = cordoned();
    c.through
        .place(&c.node, NodeAction::Drain(Duration::from_secs(600)))
        .expect("a drain");
    put(&c.disk, "cordon-owner/node-b", b"updater:kbf-daemon");

    let read = detail(&c.own, &c.node);

    assert_eq!(read.owner, Some(Owner::Updater("kbf-daemon".to_owned())));
}

/// What the farm logged on this thread while `scenario` ran on a runtime of its own with
/// the clock paused.
fn logs(scenario: impl std::future::Future<Output = ()>) -> String {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a runtime");
    crate::thread_log::logged(|| runtime.block_on(scenario))
}

/// Asserts that a row that names no owner shows the operator, leaves the cordon read as it
/// was (the row is not a corrupt state, and the node's placement is still answered), and is
/// logged as a warning that names the node. Catches: a malformed row that fails the read of
/// the node, or that is skipped without a trace.
#[test]
fn a_malformed_owner_row_shows_the_operator_and_is_logged() {
    let log = logs(async {
        for bad in [&b"updater:"[..], b"Root", &[0xff, 0xfe], b""] {
            let c = cordoned();
            put(&c.disk, "cordon-owner/node-b", bad);

            let read = detail(&c.own, &c.node);

            assert_eq!(read.owner, Some(Owner::Operator), "{bad:?}");
            assert_eq!(read.node.placement, PlacementView::Cordoned);
        }
    });

    let lines: Vec<&str> = log.lines().filter(|l| l.contains("malformed")).collect();
    assert_eq!(lines.len(), 4, "{log}");
    assert!(
        lines
            .iter()
            .all(|l| l.contains("WARN") && l.contains("node-b")),
        "{log}"
    );
}

/// Asserts that a well-formed row logs nothing about owners.
#[test]
fn a_well_formed_owner_row_logs_nothing() {
    let log = logs(async {
        let c = cordoned();
        put(&c.disk, "cordon-owner/node-b", b"updater:kbf-daemon");
        detail(&c.own, &c.node);
    });

    assert!(!log.contains("malformed"), "{log}");
}

/// Asserts that the farm's own placements still write what they wrote before owners were
/// read: the `cordon/` row is the one tag byte, or the tag and the deadline, and no
/// `cordon-owner/` row is written by a cordon, a drain or an uncordon. Catches: a writer
/// that records an owner before every frontend reads one (an old frontend would refuse a
/// `cordon/` row in a new shape on every read).
#[tokio::test(start_paused = true)]
async fn the_writers_still_write_the_cordon_row_alone() {
    let c = cordoned();
    let row = |disk: &Disk| {
        disk.store
            .get("cordon/node-b")
            .expect("read")
            .map(|r| r.value)
    };
    assert_eq!(row(&c.disk), Some(vec![0]));

    c.through
        .place(&c.node, NodeAction::Drain(Duration::from_secs(600)))
        .expect("a drain");
    let drain = row(&c.disk).expect("a row");
    assert_eq!((drain[0], drain.len()), (1, 9));
    c.own
        .place(&c.node, NodeAction::Cordon)
        .expect("a cordon through the holder");
    assert_eq!(row(&c.disk), Some(vec![0]));
    assert_eq!(c.disk.rows("cordon-owner/"), 0);

    c.through
        .place(&c.node, NodeAction::Uncordon)
        .expect("returned");
    assert_eq!(row(&c.disk), None);
    assert_eq!(c.disk.rows("cordon-owner/"), 0);
}
