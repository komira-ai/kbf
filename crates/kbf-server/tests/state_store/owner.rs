//! Daemons that move between frontends sharing one state store. The frontend a daemon last
//! said `Hello` to owns its node: only that frontend writes the node's leases, and only it
//! acknowledges the daemon's heartbeats, and only while the other frontends can see the
//! daemon's session row. Each test runs over the fake, a SQLite file and a Postgres schema
//! (the `restarts!` list of the parent module).

use kbf_sched::fence::{HANDOVER_GRACE, LEASE_GRACE};
use kbf_server::farm::NodeAction;
use kbf_server::{ACK_WINDOW, OWNER_CACHE_TTL, SESSION_RENEWAL};

use super::*;

/// Registers `worker` as the daemon process `instance`, with room for four actions.
fn register_instance(farm: &TestFarm, worker: &WorkerId, instance: &str) -> (StreamId, Inbox) {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let reported = Reported::serving(Resources::new(4_000, 8 * GIB), caps);
    let (outbound, inbox) = mpsc::unbounded_channel();
    let stream = farm.register(
        worker,
        DaemonInstance::new(instance),
        DaemonInstance::new(""),
        reported,
        outbound,
        ServerMessage::default(),
    );
    (stream, inbox)
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

fn lease_row(lease: LeaseId) -> String {
    format!("lease/{:016x}.{:016x}", lease.term, lease.seq)
}

/// The counter of the `owner/node-a` row: how many times a daemon has said `Hello` as the
/// node to a frontend.
fn owner_counter(disk: &Disk) -> u8 {
    let row = disk.store.get("owner/node-a").expect("read");
    let value = row.expect("an owner row").value;
    value[15]
}

fn has_lease(disk: &Disk, lease: LeaseId) -> bool {
    let row = disk.store.get(&lease_row(lease)).expect("read");
    row.is_some()
}

fn session_row(disk: &Disk) -> Versioned {
    let row = disk.store.get("session/node-a").expect("read");
    row.expect("a session row")
}

/// Asserts that a daemon that says `Hello` to another frontend keeps the lease it runs: the
/// frontend it moved to takes the lease up from the store (it did not hold the operation),
/// keeps it while the daemon lists it, and takes the daemon's result, which finishes the
/// operation.
pub(super) async fn a_daemon_that_moves_to_another_frontend_keeps_its_lease(backing: Backing) {
    let disk = Disk::over(backing);
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let (_, mut inbox) = register(&a);
    submit(&a, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    assert!(held(&b).is_empty(), "the second frontend held the lease");

    let (stream, _inbox) = register(&b);

    assert_eq!(held(&b), [lease], "the lease was not taken up");
    assert_eq!(owner_counter(&disk), 2);
    assert!(b.heartbeat(&node(), stream, 1, vec![lease]).await);
    let taken = finished_by(&b, &node(), lease).await;
    assert!(taken, "the lease was lost in the move");
    assert_eq!((disk.rows("op/"), disk.rows("lease/")), (0, 0));
    assert_eq!((disk.rows("done/"), disk.rows("lease-by-op/")), (1, 0));
}

/// Asserts that the frontend a daemon has left refuses to write for the daemon's node, and
/// nothing else: its late result for the node is dropped (not acknowledged, no row written),
/// the result of a node it still owns is taken, it takes new work, and the frontend the
/// daemon moved to takes the dropped result.
pub(super) async fn the_frontend_a_daemon_left_drops_only_that_nodes_writes(backing: Backing) {
    let disk = Disk::over(backing);
    let (node_a, node_b) = (node(), WorkerId::new("node-b"));
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let (_, mut inbox_a) = register_as(&a, &node_a);
    submit(&a, request(1)).expect("submitted");
    let [one] = starts(&mut inbox_a)[..] else {
        panic!("one lease expected");
    };
    a.place(&node_a, NodeAction::Cordon).expect("cordoned");
    let (_, mut inbox_b) = register_as(&a, &node_b);
    submit(&a, request(2)).expect("submitted");
    let [two] = starts(&mut inbox_b)[..] else {
        panic!("one lease expected");
    };
    let (stream, _inbox) = register_as(&b, &node_a);

    let left = finished_by(&a, &node_a, one).await;
    assert!(!left, "the frontend the daemon left took its result");
    assert_eq!(a.dropped_writes(), 1);
    assert_eq!((disk.rows("lease/"), disk.rows("done/")), (2, 0));

    let stayed = finished_by(&a, &node_b, two).await;
    assert!(stayed, "the result of a node it still owns was dropped");
    assert_eq!(a.dropped_writes(), 1);
    assert_eq!((disk.rows("lease/"), disk.rows("done/")), (1, 1));
    submit(&a, request(3)).expect("a superseded frontend refused work");

    assert!(b.heartbeat(&node_a, stream, 1, vec![one]).await);
    let moved = finished_by(&b, &node_a, one).await;
    assert!(moved, "the new owner refused the result");
    assert_eq!(disk.rows("done/"), 2);
}

/// Asserts that two frontends never both grant to one node: after the daemon says `Hello`
/// to the second, the first, which still believes the node is its own, is refused its grant
/// (nothing is written, no `Start` goes out), and the second grants.
pub(super) async fn two_frontends_never_both_grant_to_the_same_worker(backing: Backing) {
    let disk = Disk::over(backing);
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let (_, mut inbox_a) = register(&a);
    let (_, mut inbox_b) = register(&b);

    submit(&a, request(1)).expect("queued");

    let sent = starts(&mut inbox_a);
    assert!(sent.is_empty(), "the old owner sent a Start");
    assert_eq!(a.dropped_writes(), 1);
    assert_eq!((disk.rows("lease/"), disk.rows("queue/")), (0, 1));

    submit(&b, request(2)).expect("queued");
    let [lease] = starts(&mut inbox_b)[..] else {
        panic!("one lease expected");
    };
    assert_eq!((disk.rows("lease/"), disk.rows("queue/")), (1, 1));
    assert!(has_lease(&disk, lease));
}

/// Asserts that a daemon that says `Hello` again to the frontend that holds its lease takes
/// nothing in: the lease stays the one lease the frontend holds, and the node's heartbeat that
/// lists it is acknowledged.
pub(super) async fn a_hello_to_the_frontend_that_holds_the_lease_takes_nothing_in(
    backing: Backing,
) {
    let disk = Disk::over(backing);
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    submit(&farm, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };

    let (stream, _inbox) = register(&farm);

    assert_eq!(held(&farm), [lease], "the lease was taken in again");
    assert_eq!(owner_counter(&disk), 2);
    assert!(farm.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert_eq!(disk.rows("lease/"), 1);
}

/// Asserts that a lease the scheduler of the frontend a daemon has left gives up, because the
/// daemon fell silent there, is not written: the give-up is dropped and counted once, it is
/// not tried again, and the lease stays in the store for the frontend the daemon moved to.
pub(super) async fn a_give_up_for_a_node_the_daemon_left_is_dropped_once(backing: Backing) {
    let disk = Disk::over(backing);
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let (_, mut inbox) = register(&a);
    submit(&a, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    let (_stream, _inbox) = register(&b);
    tokio::time::advance(LEASE_GRACE + Duration::from_secs(1)).await;

    a.tick().await;

    assert_eq!(a.dropped_writes(), 1, "the give-up was not dropped");
    assert!(has_lease(&disk, lease), "the give-up reached the lease");
    assert_eq!(disk.rows("queue/"), 0);

    tokio::time::advance(Duration::from_secs(1)).await;
    a.tick().await;
    assert_eq!(a.dropped_writes(), 1, "the give-up was tried again");
}

/// Asserts that a request that joins an operation another frontend runs finds the
/// operation's lease through its index row, and reads none of the `lease/` rows to do it,
/// and that it leaves the lease to the frontend that owns the node: it holds none, and the
/// lease row stands.
pub(super) async fn a_joined_submit_finds_its_lease_through_the_index(backing: Backing) {
    let disk = Disk::over(backing);
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let (_, mut inbox) = register(&a);
    submit(&a, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    let scans = disk.store.lease_scans.load(Ordering::SeqCst);

    submit(&b, request(1)).expect("joined");

    let read = disk.store.lease_scans.load(Ordering::SeqCst) - scans;
    assert_eq!(read, 0, "the lease rows were read to find the lease");
    assert!(held(&b).is_empty(), "the joiner took the lease up");
    assert_eq!(held(&a), [lease], "the owner lost its lease");
    assert_eq!(disk.rows("lease/"), 1);
}

/// Asserts that a frontend stops acknowledging a daemon's heartbeats once its last write of
/// the daemon's session row is older than the ack window (the store takes no writes), while
/// the heartbeats are still taken, and acknowledges again when a renewal is written.
pub(super) async fn acks_stop_when_session_renewals_fail(backing: Backing) {
    let disk = Disk::over(backing);
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, _inbox) = register(&farm);
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);

    disk.store.set_down(true);
    tokio::time::advance(ACK_WINDOW - Duration::from_secs(1)).await;
    let inside = farm.heartbeat(&node(), stream, 2, Vec::new()).await;
    assert!(inside, "withheld inside the window");
    tokio::time::advance(Duration::from_secs(2)).await;
    let outside = farm.heartbeat(&node(), stream, 3, Vec::new()).await;
    assert!(!outside, "acknowledged with the session row unwritten");

    disk.store.set_down(false);
    let renewed = farm.heartbeat(&node(), stream, 4, Vec::new()).await;
    assert!(renewed, "not acknowledged after the renewal was written");
}

/// Asserts that a registration whose session row the store refused is not acknowledged until
/// the row is written, and is acknowledged when the next renewal puts the row.
pub(super) async fn an_ack_waits_for_the_session_row(backing: Backing) {
    let disk = Disk::over(backing);
    let farm = disk.farm(EpochPolicy::Keep);
    disk.store.fail_after(1);
    let (stream, _inbox) = register(&farm);
    let written = disk.rows("session/");
    assert_eq!(written, 0, "the registration was written");

    let down = farm.heartbeat(&node(), stream, 1, Vec::new()).await;
    assert!(!down, "acknowledged with no session row");

    disk.store.set_down(false);
    let up = farm.heartbeat(&node(), stream, 2, Vec::new()).await;
    assert!(up, "not acknowledged once the row was written");
    assert_eq!(disk.rows("session/"), 1);
}

/// Asserts that the frontend a daemon has left stops acknowledging its heartbeats once its
/// look at the owner row is a cache TTL old (not before), does not extend the node's session
/// row, and that the frontend the daemon moved to acknowledges and renews it.
pub(super) async fn acks_stop_when_the_daemon_moves(backing: Backing) {
    let disk = Disk::over(backing);
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let (stream_a, _inbox_a) = register(&a);
    assert!(a.heartbeat(&node(), stream_a, 1, Vec::new()).await);
    let (stream_b, _inbox_b) = register(&b);

    let cached = a.heartbeat(&node(), stream_a, 2, Vec::new()).await;
    assert!(cached, "withheld inside the cache TTL");
    tokio::time::advance(OWNER_CACHE_TTL).await;
    let moved = a.heartbeat(&node(), stream_a, 3, Vec::new()).await;
    assert!(!moved, "acknowledged for a node it left");
    assert!(b.heartbeat(&node(), stream_b, 1, Vec::new()).await);

    let before = session_row(&disk).version;
    tokio::time::advance(SESSION_RENEWAL * 2).await;
    let left = a.heartbeat(&node(), stream_a, 4, Vec::new()).await;
    assert!(!left, "acknowledged for a node it left");
    let kept = session_row(&disk).version;
    assert_eq!(kept, before, "the old owner renewed");
    assert!(b.heartbeat(&node(), stream_b, 2, Vec::new()).await);
    let renewed = session_row(&disk).version;
    assert!(renewed > before, "the new owner did not renew");
}

/// Asserts that the lease a daemon does not list, after it moved to a frontend as another
/// process, is kept until the handover grace and the owner cache TTL have passed since its
/// `Hello`, and then given up and granted again.
pub(super) async fn a_new_process_that_moves_waits_before_its_unlisted_lease_is_lost(
    backing: Backing,
) {
    let disk = Disk::over(backing);
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let (_, mut inbox_a) = register_instance(&a, &node(), "daemon-1");
    submit(&a, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox_a)[..] else {
        panic!("one lease expected");
    };
    let (stream, mut inbox) = register_instance(&b, &node(), "daemon-2");
    let wait = HANDOVER_GRACE + OWNER_CACHE_TTL;

    assert!(b.heartbeat(&node(), stream, 1, Vec::new()).await);
    tokio::time::advance(wait - Duration::from_secs(1)).await;
    assert!(b.heartbeat(&node(), stream, 2, Vec::new()).await);
    let early = starts(&mut inbox);
    assert!(early.is_empty(), "given up inside the wait");
    assert!(has_lease(&disk, lease));

    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(b.heartbeat(&node(), stream, 3, Vec::new()).await);
    let [again] = starts(&mut inbox)[..] else {
        panic!("the lease was not given up and granted again");
    };
    assert_ne!(again, lease);
    assert!(!has_lease(&disk, lease));
    assert!(has_lease(&disk, again));
}

/// Asserts that a daemon's `Result` for its operation's current lease is accepted, or not
/// acknowledged at all, and never refused: a daemon forgets a `Result` on any
/// acknowledgement, so a refused current-lease `Result` is lost, and its lease stays on the
/// node, counted, until the node falls silent. The frontend the daemon moved to opened
/// first, so it holds the lower block of lease sequences: after the handover grace it gives
/// up the lease it took up and grants the operation again with a lower sequence than that
/// lease's, as when a daemon restarts during a roll.
pub(super) async fn a_result_for_a_lease_numbered_below_the_one_taken_up_is_never_refused(
    backing: Backing,
) {
    let disk = Disk::over(backing);
    let b = disk.farm(EpochPolicy::Keep);
    let a = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox_a) = register_instance(&a, &node(), "daemon-1");
    submit(&a, request(1)).expect("submitted");
    let [taken_up] = starts(&mut inbox_a)[..] else {
        panic!("one lease expected");
    };
    let (stream, mut inbox) = register_instance(&b, &node(), "daemon-2");
    assert_eq!(held(&b), [taken_up], "the lease was not taken up");
    assert!(b.heartbeat(&node(), stream, 1, Vec::new()).await);
    tokio::time::advance(HANDOVER_GRACE + OWNER_CACHE_TTL + Duration::from_secs(1)).await;
    assert!(b.heartbeat(&node(), stream, 2, Vec::new()).await);
    let [again] = starts(&mut inbox)[..] else {
        panic!("the lease was not given up and granted again");
    };
    assert!(
        again.seq < taken_up.seq,
        "the grant again is not below the lease taken up: {again:?} {taken_up:?}"
    );

    let ack = b.report(&node(), invalid(again)).await;

    assert!(
        ack.as_ref().is_none_or(|ack| ack.accepted),
        "the current lease's result was refused: {ack:?}"
    );
}

/// Asserts that the lease a daemon does not list, after it moved to a frontend as the same
/// process, is given up at once, as it is when a daemon reconnects.
pub(super) async fn the_same_process_that_moves_loses_an_unlisted_lease_at_once(backing: Backing) {
    let disk = Disk::over(backing);
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let (_, mut inbox_a) = register_instance(&a, &node(), "daemon-1");
    submit(&a, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox_a)[..] else {
        panic!("one lease expected");
    };
    let (stream, mut inbox) = register_instance(&b, &node(), "daemon-1");

    assert!(b.heartbeat(&node(), stream, 1, Vec::new()).await);

    let [again] = starts(&mut inbox)[..] else {
        panic!("the lease was not given up and granted again");
    };
    assert_ne!(again, lease);
    assert!(!has_lease(&disk, lease));
}

/// Asserts that a claim the store refused at the `Hello` is made when the store takes writes
/// again, by the feed that finds it reachable, and not before; that the heartbeats are not
/// acknowledged meanwhile; and that a node whose stream closed in the meantime is not claimed.
pub(super) async fn a_claim_the_store_refused_is_made_when_it_returns(backing: Backing) {
    let disk = Disk::over(backing);
    let farm = disk.farm(EpochPolicy::Keep);
    disk.store.set_down(true);
    let (stream, _inbox) = register(&farm);
    let down = farm.heartbeat(&node(), stream, 1, Vec::new()).await;
    assert!(!down, "acknowledged for a node it has not claimed");
    assert_eq!(disk.rows("owner/"), 0);

    disk.store.set_down(false);
    let up = farm.heartbeat(&node(), stream, 2, Vec::new()).await;
    assert!(up, "not acknowledged after the claim");
    assert_eq!(owner_counter(&disk), 1);

    let other = WorkerId::new("node-b");
    disk.store.set_down(true);
    let (_, gone) = register_as(&farm, &other);
    drop(gone);
    disk.store.set_down(false);
    farm.tick().await;
    let claimed = disk.rows("owner/");
    assert_eq!(claimed, 1, "a node that left was claimed");
}

/// Asserts that a node whose owner row cannot be read is not claimed, and the claim is not
/// tried again until the node says `Hello` again: its heartbeats are not acknowledged, and
/// they are once a `Hello` finds the row right.
pub(super) async fn an_owner_row_that_cannot_be_read_blocks_the_claim_until_the_next_hello(
    backing: Backing,
) {
    let disk = Disk::over(backing);
    let farm = disk.farm(EpochPolicy::Keep);
    let damaged = Txn::new().put("owner/node-a", vec![1, 2, 3], kbf_state::Cond::Any);
    disk.store.txn(damaged).expect("store").expect("put");
    let (stream, _inbox) = register(&farm);
    let first = farm.heartbeat(&node(), stream, 1, Vec::new()).await;
    assert!(!first, "acknowledged for a node it could not claim");

    let repaired = Txn::new().delete("owner/node-a", kbf_state::Cond::Any);
    disk.store.txn(repaired).expect("store").expect("gone");
    let second = farm.heartbeat(&node(), stream, 2, Vec::new()).await;
    assert!(!second, "the claim was tried again without a Hello");

    let (stream, _inbox) = register(&farm);
    let third = farm.heartbeat(&node(), stream, 1, Vec::new()).await;
    assert!(third, "not acknowledged after the next Hello");
    assert_eq!(owner_counter(&disk), 1);
}
