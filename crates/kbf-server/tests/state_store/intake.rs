//! Several frontends over one state store, each with daemons of its own: what a frontend
//! takes in from the others at its intake poll (`Farm::intake`). Work queued through one
//! frontend runs on a daemon of another, a frontend that loses a race to start work forgets
//! it, callers get the results other frontends record, the work of a frontend that died is
//! requeued, and what an operator sets through one frontend holds on the others. Each test
//! runs over the fake, a SQLite file and a Postgres schema (the `restarts!` list of the
//! parent module).

use kbf_sched::fence::LEASE_GRACE;
use kbf_server::farm::NodeAction;
use kbf_server::fleet::PlacementView;

use super::*;

/// A caller of `farm` that waits for the action `tag`.
fn caller(farm: &TestFarm, tag: u8) -> kbf_front::Ticket {
    let submission = Submission {
        request: request(tag),
        call: None,
        described: Described::default(),
        evidence: Default::default(),
        max_wall: None,
    };
    farm.submit(submission).expect("submitted")
}

/// The leases `farm` holds.
fn held(farm: &TestFarm) -> Vec<LeaseId> {
    let views = farm.lease_views(None);
    views.leases.iter().map(|view| view.lease).collect()
}

/// Whether `farm` accepts the result `worker` reports for `lease`.
async fn finished_by(farm: &TestFarm, worker: &WorkerId, lease: LeaseId) -> bool {
    let ack = farm.report(worker, invalid(lease)).await;
    ack.is_some_and(|ack| ack.accepted)
}

/// Removes the session row of `worker`, as the store's clock does when no frontend renews it.
fn expire_session(disk: &Disk, worker: &str) {
    let expired = Txn::new().delete(format!("session/{worker}"), kbf_state::Cond::Any);
    disk.store.txn(expired).expect("store").expect("delete");
}

/// What the operator did to `worker`, as `farm` shows it.
fn placement(farm: &TestFarm, worker: &WorkerId) -> PlacementView {
    farm.node_view(worker)
        .expect("the node registered")
        .placement
}

/// The store's clock, in milliseconds.
fn store_ms(disk: &Disk) -> u64 {
    let now = disk.store.now().as_duration();
    u64::try_from(now.as_millis()).expect("a time that fits")
}

/// The message of the failure a caller was answered with.
fn answered_with(ticket: &kbf_front::Ticket) -> String {
    let Finished::Failed(status) = ended(ticket) else {
        panic!("the caller was answered with something else");
    };
    status.message
}

/// Asserts that an operation queued through one frontend runs on a daemon connected to
/// another: the frontend with the daemon takes the operation in at its poll and places it,
/// the result is recorded once, and the caller of the first frontend gets it at its poll.
/// Before the poll the daemon is offered nothing.
pub(super) async fn an_operation_queued_through_one_frontend_runs_on_a_daemon_of_another(
    backing: Backing,
) {
    let disk = Disk::over(backing);
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let ticket = caller(&a, 1);
    let (_, mut inbox) = register(&b);
    assert!(
        starts(&mut inbox).is_empty(),
        "work reached a daemon unheard"
    );

    b.intake();
    b.tick().await;
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    assert!(!matches!(*ticket.stage.borrow(), Stage::Done(_)));
    assert!(finished_by(&b, &node(), lease).await);
    a.intake();

    assert_eq!(answered_with(&ticket), "test");
    assert_eq!(a.queued_operations(), 0);
    assert!(held(&a).is_empty());
    assert_eq!((disk.rows("op/"), disk.rows("lease/")), (0, 0));
    assert_eq!((disk.rows("queue/"), disk.rows("done/")), (0, 1));
}

/// Asserts that a frontend that loses a grant race in the store forgets the placement and
/// the operation follows the store: the frontend that lost starts nothing, holds no lease and
/// no booking for the operation, and keeps it no longer; the operation runs once, on the
/// daemon of the frontend that won, and the caller of the one that lost gets its result.
pub(super) async fn a_lost_grant_race_forgets_the_placement_and_the_operation_runs_once(
    backing: Backing,
) {
    let disk = Disk::over(backing);
    let (node_a, node_b) = (WorkerId::new("node-a"), WorkerId::new("node-b"));
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let ticket = caller(&a, 1);
    b.intake();
    let (_, mut inbox_b) = register_as(&b, &node_b);
    let [won] = starts(&mut inbox_b)[..] else {
        panic!("one lease expected");
    };

    let (_, mut inbox_a) = register_as(&a, &node_a);

    assert!(
        starts(&mut inbox_a).is_empty(),
        "the loser started the operation"
    );
    assert!(held(&a).is_empty(), "the lost placement was kept");
    assert_eq!(a.lease_views(Some(&node_a)).leases.len(), 0);
    assert_eq!(a.queued_operations(), 0, "the loser kept the operation");
    assert_eq!(a.dropped_writes(), 1);
    assert_eq!(disk.rows("lease/"), 1);

    assert!(finished_by(&b, &node_b, won).await);
    a.intake();

    assert_eq!(answered_with(&ticket), "test");
    assert!(starts(&mut inbox_a).is_empty(), "the operation ran twice");
    assert_eq!((disk.rows("lease/"), disk.rows("done/")), (0, 1));
}

/// Asserts that clients waiting on one frontend for an operation another frontend runs are
/// shown it executing, and get the result that frontend records, each of them, and that the
/// frontend still answers a request for the operation by name afterwards.
pub(super) async fn a_client_waiting_on_one_frontend_gets_the_result_another_recorded(
    backing: Backing,
) {
    let disk = Disk::over(backing);
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let first = caller(&a, 1);
    let twin = caller(&a, 1);
    let (_, mut inbox) = register(&b);
    b.intake();
    b.tick().await;
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };

    a.intake();

    assert!(matches!(*first.stage.borrow(), Stage::Executing));
    assert!(matches!(*twin.stage.borrow(), Stage::Executing));
    assert!(finished_by(&b, &node(), lease).await);
    a.intake();
    assert_eq!(answered_with(&first), "test");
    assert_eq!(answered_with(&twin), "test");
    let waiting = a.wait(&first.name).expect("the caller is still known");
    assert!(matches!(*waiting.stage.borrow(), Stage::Done(_)));
}

/// Asserts that a frontend killed while its daemon runs an operation loses nothing: the
/// operation's lease stays while the node's session row is there, and while the node has
/// none until the wait is over on the store's clock (not before), then the lease is requeued
/// with a loss counted, and the operation runs again on a daemon of a frontend that remains.
pub(super) async fn a_frontend_killed_mid_run_loses_nothing(backing: Backing) {
    let disk = Disk::over(backing);
    let (node_a, node_b) = (WorkerId::new("node-a"), WorkerId::new("node-b"));
    let wait = Duration::from_secs(1);
    let a = disk.farm(EpochPolicy::Keep);
    let b = disk.farm(EpochPolicy::Keep).with_orphan_wait(wait);
    let (_, inbox_a) = register_as(&a, &node_a);
    let _ticket = caller(&a, 1);
    let [lease] = held(&a)[..] else {
        panic!("one lease expected");
    };
    drop(inbox_a);
    drop(a);

    b.intake();
    assert_eq!(disk.rows("lease/"), 1, "a lease of a node with a session");
    expire_session(&disk, "node-a");
    b.intake();
    b.intake();
    assert_eq!(
        (disk.rows("lease/"), disk.rows("queue/")),
        (1, 0),
        "requeued before the wait was over"
    );

    disk.pass(wait + Duration::from_millis(100));
    b.intake();
    assert_eq!((disk.rows("lease/"), disk.rows("queue/")), (0, 1));
    assert_eq!(disk.rows("loss/"), 1);
    let (_, mut inbox_b) = register_as(&b, &node_b);
    b.intake();
    b.tick().await;
    let [again] = starts(&mut inbox_b)[..] else {
        panic!("the operation did not run again");
    };
    assert_ne!(again, lease);
    assert!(finished_by(&b, &node_b, again).await);
    assert_eq!((disk.rows("op/"), disk.rows("done/")), (0, 1));
}

/// Asserts that a cordon set through one frontend for a node connected to another stops the
/// grants of the other at its next poll, that the node is placed again when the cordon is
/// lifted, and that a node no frontend knows cannot be placed.
pub(super) async fn a_cordon_set_through_one_frontend_stops_grants_on_the_other(backing: Backing) {
    let disk = Disk::over(backing);
    let node_b = WorkerId::new("node-b");
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let (_, mut inbox) = register_as(&b, &node_b);
    let unknown = a.place(&WorkerId::new("node-z"), NodeAction::Cordon);
    assert!(unknown.is_err(), "a node no frontend knows was placed");
    assert_eq!(disk.rows("cordon/"), 0);

    a.place(&node_b, NodeAction::Cordon)
        .expect("a node of another frontend is placed");
    b.intake();
    b.intake();
    submit(&b, request(1)).expect("queued");

    assert!(starts(&mut inbox).is_empty(), "a cordoned node was granted");
    assert_eq!(placement(&b, &node_b), PlacementView::Cordoned);
    a.place(&node_b, NodeAction::Uncordon).expect("returned");
    b.intake();
    assert_eq!(starts(&mut inbox).len(), 1, "the node was not placed again");
    assert_eq!(placement(&b, &node_b), PlacementView::Serving);
}

/// Asserts that a drain started through one frontend keeps its deadline on the store's clock
/// (the row holds the store's time plus the time the drain was given, not this host's clock),
/// and that the frontend the node is connected to applies it, ending as far from now as the
/// deadline is from the store's time.
pub(super) async fn a_drain_deadline_is_on_the_stores_clock_and_applied_by_the_other(
    backing: Backing,
) {
    let disk = Disk::over(backing);
    let node_b = WorkerId::new("node-b");
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let (_, mut inbox) = register_as(&b, &node_b);
    submit(&b, request(1)).expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1, "the node holds a lease");
    let within = 3_600_000;
    let before = store_ms(&disk);

    a.place(&node_b, NodeAction::Drain(Duration::from_millis(within)))
        .expect("a drain");

    let after = store_ms(&disk);
    let row = disk.store.get("cordon/node-b").expect("read");
    let value = row.expect("a cordon row").value;
    let [1, deadline @ ..] = &value[..] else {
        panic!("not a drain row: {value:?}");
    };
    let deadline = u64::from_be_bytes(<[u8; 8]>::try_from(deadline).expect("eight bytes"));
    let wanted = before + within..=after + within;
    assert!(
        wanted.contains(&deadline),
        "{deadline} is not within {wanted:?}"
    );
    b.intake();
    let ends = |view: PlacementView| match view {
        PlacementView::Draining {
            deadline_unix_ms, ..
        } => deadline_unix_ms.expect("a deadline"),
        other => panic!("{other:?}"),
    };
    let wall = b.wall_now().as_millis();
    let off = ends(placement(&b, &node_b)).abs_diff(wall + within);
    assert!(off < 10_000, "the drain ends {off} ms from its time");
}

/// Asserts that starting a frontend over a store with a lease of a running one takes the
/// lease up from no one and gives it up never, however long the frontend runs, and that the
/// lease is taken up when its daemon says `Hello` to the new frontend.
pub(super) async fn starting_a_frontend_leaves_the_leases_of_the_others_alone(backing: Backing) {
    let disk = Disk::over(backing);
    let a = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&a);
    submit(&a, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };

    let b = disk.farm(EpochPolicy::Keep);
    assert!(held(&b).is_empty(), "a start took a lease up");
    for _ in 0..3 {
        tokio::time::advance(LEASE_GRACE).await;
        b.tick().await;
        b.intake();
    }

    assert_eq!((disk.rows("lease/"), disk.rows("queue/")), (1, 0));
    assert!(
        held(&b).is_empty(),
        "the lease was taken up without a Hello"
    );
    let (stream, _inbox) = register(&b);
    assert_eq!(held(&b), [lease]);
    assert!(b.heartbeat(&node(), stream, 1, vec![lease]).await);
}

/// Asserts that a frontend reports the epoch the store holds when an operator raised it
/// past the one it serves (it serves the epoch it started under, and the operator restarts
/// it), and that a frontend started after the raise serves it and reports nothing.
pub(super) async fn the_epoch_an_operator_raised_is_reported_by_the_other_frontend(
    backing: Backing,
) {
    let disk = Disk::over(backing);
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    b.intake();
    assert_eq!(b.epoch_ahead(), None);

    let bumped = a.bump_epoch().expect("a bump").expect("a state store");

    assert_eq!(b.epoch_ahead(), None, "reported before the poll");
    b.intake();
    assert_eq!(b.epoch_ahead(), Some(bumped.epoch));
    assert_eq!(b.lease_epoch(), bumped.previous);
    let restarted = disk.farm(EpochPolicy::Keep);
    restarted.intake();
    assert_eq!(restarted.epoch_ahead(), None);
    assert_eq!(restarted.lease_epoch(), bumped.epoch);
}
