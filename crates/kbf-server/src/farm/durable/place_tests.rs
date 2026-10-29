//! An operator's or an updater's node action (`Farm::place`) when the state store fails: the
//! caller is told the store failed, never "no such node", and never that an action was placed
//! which the store refused.

use super::blocks_tests::{Racing, farm_over};
use super::intake_tests::{Inbox, register};
use super::*;
use crate::farm::{NodeAction, NodeUnread};
use crate::fleet::PlacementView;

/// What `Farm::place` answers when the store refused the write of the node's `cordon/` row.
const UNWRITTEN: &str =
    "the state store could not be written for node node-b; the action was not applied";

/// Two frontends over one store, and a node connected to the second (`own`); actions are
/// placed through the first (`through`) or the second.
struct Pair {
    through: Farm,
    own: Farm,
    node: WorkerId,
    _stream: Inbox,
}

fn pair(store: &Arc<Racing>) -> Pair {
    let (through, own) = (farm_over(store), farm_over(store));
    let stream = register(&own, "node-b");
    Pair {
        through,
        own,
        node: WorkerId::new("node-b"),
        _stream: stream,
    }
}

fn placement(farm: &Farm, node: &WorkerId) -> PlacementView {
    let read = farm.node_detail(node).expect("the store can be read");
    read.expect("a node").node.placement
}

/// Asserts that an action on a node this frontend is not connected to is a store error, not
/// "no such node", when the store cannot say who holds the node: its `owner/` row, or every
/// row, cannot be read. Catches: a host updater that takes the 404 for "no daemon here" and
/// goes on to switch a node that is draining on another frontend.
#[tokio::test(start_paused = true)]
async fn an_action_on_a_node_the_store_cannot_place_is_a_store_error() {
    for prefix in ["owner/", ""] {
        let store = Racing::new();
        let pair = pair(&store);
        store.fail_reads(Some(prefix));
        for action in [
            NodeAction::Cordon,
            NodeAction::Drain(Duration::from_secs(600)),
            NodeAction::Uncordon,
        ] {
            let placed = pair.through.place(&pair.node, action);

            let refused = placed.expect_err("the store cannot say who holds the node");
            assert_eq!(
                refused.to_string(),
                NodeUnread("node-b".to_owned()).to_string(),
                "{prefix:?} {action:?}"
            );
        }
        store.fail_reads(None);
        assert_eq!(placement(&pair.own, &pair.node), PlacementView::Serving);
    }
}

/// Asserts that a node no frontend has held is still "no such node" when the store can be
/// read. Catches: a store error for every name.
#[tokio::test(start_paused = true)]
async fn an_action_on_a_node_no_frontend_holds_is_still_unknown() {
    let store = Racing::new();
    let farm = farm_over(&store);

    let placed = farm.place(&WorkerId::new("nobody"), NodeAction::Cordon);

    let refused = placed.expect_err("no frontend holds the node");
    assert_eq!(refused.to_string(), "no node nobody has registered");
}

/// Asserts that an action on a node another frontend holds, whose `cordon/` row the store
/// refuses, is a store error, and that no frontend then shows the node placed. Catches: a
/// drain the store refused answered as placed, which the node's own frontend never sees.
#[tokio::test(start_paused = true)]
async fn an_action_on_a_node_another_frontend_holds_fails_when_its_cordon_cannot_be_written() {
    for action in [
        NodeAction::Cordon,
        NodeAction::Drain(Duration::from_secs(600)),
    ] {
        let store = Racing::new();
        let pair = pair(&store);
        store.fail_writes(Some("cordon/"));

        let placed = pair.through.place(&pair.node, action);

        let refused = placed.expect_err("the store refused the cordon row");
        assert_eq!(refused.to_string(), UNWRITTEN, "{action:?}");
        store.fail_writes(None);
        pair.own.intake();
        assert_eq!(placement(&pair.own, &pair.node), PlacementView::Serving);
    }
}

/// Asserts that a frontend the node is not connected to shows a drain or cordon the store
/// refused as not placed, from the store's rows. Catches: a refused row that this frontend
/// reads back as the node's placement.
#[tokio::test(start_paused = true)]
#[cfg_attr(
    feature = "farm-core-js",
    ignore = "known-different-on-js: the detail of a node another frontend holds is not answered on the js core"
)]
async fn a_frontend_the_node_is_not_connected_to_shows_a_refused_cordon_as_serving() {
    for action in [
        NodeAction::Cordon,
        NodeAction::Drain(Duration::from_secs(600)),
    ] {
        let store = Racing::new();
        let pair = pair(&store);
        store.fail_writes(Some("cordon/"));

        let placed = pair.through.place(&pair.node, action);

        placed.expect_err("the store refused the cordon row");
        store.fail_writes(None);
        pair.own.intake();
        assert_eq!(placement(&pair.through, &pair.node), PlacementView::Serving);
    }
}

/// Asserts that an action on a node connected here, whose `cordon/` row the store refuses, is
/// a store error and leaves the scheduler as it was: the node reads as it did, and takes work
/// as it did. Catches: a node cordoned here that the caller was told is not.
#[tokio::test(start_paused = true)]
async fn an_action_on_a_node_connected_here_fails_when_its_cordon_cannot_be_written() {
    let store = Racing::new();
    let pair = pair(&store);
    store.fail_writes(Some("cordon/"));

    let placed = pair.own.place(&pair.node, NodeAction::Cordon);

    let refused = placed.expect_err("the store refused the cordon row");
    assert_eq!(refused.to_string(), UNWRITTEN);
    assert_eq!(placement(&pair.own, &pair.node), PlacementView::Serving);
}

/// Asserts that an uncordon whose row delete the store refuses is a store error and leaves a
/// cordoned node cordoned. Catches: a node returned to placement here while its row still
/// cordons it, which the next intake poll or restart cordons again.
#[tokio::test(start_paused = true)]
async fn an_uncordon_fails_and_leaves_the_node_cordoned_when_its_row_cannot_be_deleted() {
    let store = Racing::new();
    let pair = pair(&store);
    pair.own
        .place(&pair.node, NodeAction::Cordon)
        .expect("cordoned");
    store.fail_writes(Some("cordon/"));

    let placed = pair.own.place(&pair.node, NodeAction::Uncordon);

    let refused = placed.expect_err("the store refused the row delete");
    assert_eq!(refused.to_string(), UNWRITTEN);
    assert_eq!(placement(&pair.own, &pair.node), PlacementView::Cordoned);
}
