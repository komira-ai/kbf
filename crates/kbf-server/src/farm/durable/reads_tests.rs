//! Reads of a node that another frontend of the state store drained: whichever frontend a read
//! lands on (a load balancer picks it), it shows the drain and the leases the node still holds,
//! before any intake poll has run.

use std::time::Duration;

use super::blocks_tests::{Racing, farm_over};
use super::intake_tests::{Inbox, caller, register};
use super::*;
use crate::farm::{NodeAction, NodeUnread};
use crate::fleet::{NodeDetail, PlacementView};

/// The answer of `GET /v1/nodes/{id}` on `farm`, whose store can be read.
fn detail(farm: &Farm, node: &WorkerId) -> Option<NodeDetail> {
    farm.node_detail(node).expect("the store can be read")
}

/// Two frontends over one store, and a node connected to the second (`own`) that the first
/// (`through`) drained, holding one lease if `leased`. No intake poll has run. The node's
/// stream and the lease's caller are kept open for as long as the value lives.
struct Drained {
    through: Farm,
    own: Farm,
    node: WorkerId,
    _stream: Inbox,
    _waiting: Option<kbf_front::Ticket>,
}

fn drained_through_the_other_frontend(store: &Arc<Racing>, leased: bool) -> Drained {
    let (through, own) = (farm_over(store), farm_over(store));
    let node = WorkerId::new("node-b");
    let stream = register(&own, "node-b");
    let waiting = leased.then(|| caller(&own, 1));
    let placed = own.lock().sched.leases_on(&node).len();
    assert_eq!(placed, usize::from(leased), "the node's leases");
    through
        .place(&node, NodeAction::Drain(Duration::from_secs(600)))
        .expect("the store knows the node");
    Drained {
        through,
        own,
        node,
        _stream: stream,
        _waiting: waiting,
    }
}

/// Asserts that the frontend a node is connected to answers a drain written through another
/// frontend at once, with the node's lease: it does not say `serving` until its next intake
/// poll. Catches: a host's updater that drains through one frontend and reads through the
/// node's own one takes the node for returned to placement and gives up the update.
#[tokio::test(start_paused = true)]
async fn the_nodes_own_frontend_shows_a_drain_another_frontend_wrote_at_once() {
    let store = Racing::new();
    let drained = drained_through_the_other_frontend(&store, true);

    let read = detail(&drained.own, &drained.node).expect("a registered node");

    assert!(
        matches!(&read.node.placement, PlacementView::Draining { leases, .. } if leases.len() == 1),
        "{:?}",
        read.node.placement
    );
    assert_eq!(read.leases.len(), 1);
}

/// Asserts that a frontend the node is not connected to answers it from the state store: the
/// drain its `cordon/` row holds and the lease the node holds on its own frontend, never "no
/// such node" and never no lease. Catches: an updater whose read lands there takes the node
/// for one with no daemon (a 404) or for one with no lease left, and switches it while its
/// work still runs.
#[tokio::test(start_paused = true)]
#[cfg_attr(
    feature = "farm-core-js",
    ignore = "known-different-on-js: the detail of a node another frontend holds is not answered on the js core"
)]
async fn a_frontend_the_node_is_not_connected_to_answers_its_drain_and_leases_from_the_store() {
    let store = Racing::new();
    let drained = drained_through_the_other_frontend(&store, true);

    let read = detail(&drained.through, &drained.node).expect("the store knows the node");

    assert!(
        matches!(&read.node.placement, PlacementView::Draining { leases, .. } if leases.len() == 1),
        "{:?}",
        read.node.placement
    );
    assert_eq!(read.leases.len(), 1);
    assert_eq!(read.node.lease_count, 1);
    assert!(
        !read.node.connected,
        "the node is not connected to this frontend"
    );
}

/// Asserts that a node another frontend holds, drained with no lease left, reads as drained on
/// a frontend it is not connected to; returned to placement it reads as serving there; and
/// cordoned it reads as cordoned. Catches: a read from the store that does not follow the
/// node's `cordon/` row.
#[tokio::test(start_paused = true)]
#[cfg_attr(
    feature = "farm-core-js",
    ignore = "known-different-on-js: the detail of a node another frontend holds is not answered on the js core"
)]
async fn a_frontend_the_node_is_not_connected_to_follows_its_cordon_row() {
    let store = Racing::new();
    let Drained { through, node, .. } = &drained_through_the_other_frontend(&store, false);

    let drained = detail(through, node).expect("the store knows the node");
    assert_eq!(drained.node.placement, PlacementView::Drained);
    assert!(drained.leases.is_empty());

    through
        .place(node, NodeAction::Uncordon)
        .expect("the store knows the node");
    let serving = detail(through, node).expect("the store knows the node");
    assert_eq!(serving.node.placement, PlacementView::Serving);

    through
        .place(node, NodeAction::Cordon)
        .expect("the store knows the node");
    let cordoned = detail(through, node).expect("the store knows the node");
    assert_eq!(cordoned.node.placement, PlacementView::Cordoned);
}

/// Asserts that a read of a node another frontend holds is an error when the store's lease
/// rows cannot be read, not the node with no lease. Catches: an updater that switches its node
/// because a failed read said nothing runs on it.
#[tokio::test(start_paused = true)]
#[cfg_attr(
    feature = "farm-core-js",
    ignore = "known-different-on-js: the detail of a node another frontend holds is not answered on the js core"
)]
async fn a_node_another_frontend_holds_is_unreadable_when_its_leases_cannot_be_read() {
    let store = Racing::new();
    let drained = drained_through_the_other_frontend(&store, true);
    store.fail_reads(Some("lease/"));

    let read = drained.through.node_detail(&drained.node);

    assert_eq!(read, Err(NodeUnread("node-b".to_owned())));
}

/// Asserts that a read of a node another frontend holds is an error when the store cannot
/// tell who holds it: its `owner/` row cannot be read. Catches: a frontend that takes a store
/// it cannot read for "nobody holds the node" and answers "no such node", which a storage
/// host's updater takes for no daemon and no lease.
#[tokio::test(start_paused = true)]
#[cfg_attr(
    feature = "farm-core-js",
    ignore = "known-different-on-js: the detail of a node another frontend holds is not answered on the js core"
)]
async fn a_node_another_frontend_holds_is_unreadable_when_its_owner_cannot_be_read() {
    let store = Racing::new();
    let drained = drained_through_the_other_frontend(&store, true);
    store.fail_reads(Some("owner/"));

    let read = drained.through.node_detail(&drained.node);

    assert_eq!(read, Err(NodeUnread("node-b".to_owned())));
}

/// Asserts that a read of a node another frontend holds is an error when no row of the store
/// can be read. Catches: as above, with the whole store down.
#[tokio::test(start_paused = true)]
async fn a_node_another_frontend_holds_is_unreadable_when_the_store_cannot_be_read() {
    let store = Racing::new();
    let drained = drained_through_the_other_frontend(&store, true);
    store.fail_reads(Some(""));

    let read = drained.through.node_detail(&drained.node);

    assert_eq!(read, Err(NodeUnread("node-b".to_owned())));
}

/// Asserts that a read of a node another frontend holds is an error when its `cordon/` row
/// cannot be read. Catches: a node read as serving because its drain could not be seen.
#[tokio::test(start_paused = true)]
async fn a_node_another_frontend_holds_is_unreadable_when_its_cordon_cannot_be_read() {
    let store = Racing::new();
    let drained = drained_through_the_other_frontend(&store, true);
    store.fail_reads(Some("cordon/"));

    let read = drained.through.node_detail(&drained.node);

    assert_eq!(read, Err(NodeUnread("node-b".to_owned())));
}

/// Asserts that a node connected to this frontend is still answered, from this frontend's
/// own scheduler, when the store cannot be read. Catches: a frontend that refuses every read
/// of its own nodes while its store is down.
#[tokio::test(start_paused = true)]
async fn a_node_connected_here_is_answered_when_the_store_cannot_be_read() {
    let store = Racing::new();
    let drained = drained_through_the_other_frontend(&store, true);
    store.fail_reads(Some(""));

    let read = detail(&drained.own, &drained.node).expect("a node connected here");

    assert_eq!(read.leases.len(), 1);
    assert!(read.node.connected);
}

/// Asserts that the node's own frontend still answers it, with its lease, when its cached
/// ownership has expired and its `owner/` row cannot be read. Catches: a frontend that refuses
/// a read of a node connected to it because the store cannot say who holds it.
#[tokio::test(start_paused = true)]
async fn a_node_connected_here_is_answered_when_its_owner_row_cannot_be_read() {
    let store = Racing::new();
    let drained = drained_through_the_other_frontend(&store, true);
    tokio::time::advance(OWNER_CACHE_TTL + Duration::from_millis(1)).await;
    store.fail_reads(Some("owner/"));

    let read = detail(&drained.own, &drained.node).expect("a node connected here");

    assert_eq!(read.leases.len(), 1);
}

/// Asserts that a frontend that handed its work over (fenced) answers a node connected to it
/// from its own scheduler, as it holds every node it hears from. Catches: a fenced frontend
/// that sends a read of its own node to the store.
#[tokio::test(start_paused = true)]
async fn a_fenced_frontend_answers_a_node_connected_to_it() {
    let store = Racing::new();
    let drained = drained_through_the_other_frontend(&store, true);
    drained.own.fence();

    let read = detail(&drained.own, &drained.node).expect("a node connected here");

    assert_eq!(read.leases.len(), 1);
    assert!(read.node.connected);
}

/// Asserts that a node no frontend of the store has held is not found, on a frontend with a
/// store. Catches: a read from the store that answers for any name.
#[tokio::test(start_paused = true)]
async fn a_node_no_frontend_holds_is_not_found() {
    let store = Racing::new();
    let farm = farm_over(&store);

    assert_eq!(detail(&farm, &WorkerId::new("nobody")), None);
}

/// Asserts that a node whose `cordon-owner/` row cannot be read is still answered, with its
/// placement, and shown as the operator's. Catches: a read of a node that fails, or hides
/// the cordon, because the owner could not be read.
#[tokio::test(start_paused = true)]
async fn an_owner_row_that_cannot_be_read_shows_the_operator() {
    let store = Racing::new();
    let drained = drained_through_the_other_frontend(&store, true);
    store.fail_reads(Some("cordon-owner/"));

    let read = detail(&drained.own, &drained.node).expect("a node connected here");

    assert_eq!(read.owner, Some(crate::fleet::Owner::Operator));
    assert!(matches!(
        read.node.placement,
        PlacementView::Draining { .. }
    ));
}
