//! Several frontends over one state store, each with daemons of its own, acting on the same
//! operation: the rows they write are conditional on the rows they read, so one that loses a
//! race writes nothing and the other's work stands; and an operation is judged unservable
//! from the daemons of every frontend, not of one. Each test runs over the fake, a SQLite
//! file and a Postgres schema (the `restarts!` list of the parent module).

use super::*;

/// The key of the row a lease is held in.
fn lease_row(lease: LeaseId) -> String {
    format!("lease/{:016x}.{:016x}", lease.term, lease.seq)
}

/// Whether `farm` accepts the result `worker` reports for `lease`.
async fn finished_by(farm: &TestFarm, worker: &WorkerId, lease: LeaseId) -> bool {
    let ack = farm.report(worker, invalid(lease)).await;
    ack.is_some_and(|ack| ack.accepted)
}

/// A caller of `farm` that waits for the action `tag`.
fn caller(farm: &TestFarm, tag: u8) -> kbf_front::Ticket {
    let submission = Submission {
        request: request(tag),
        call: None,
        described: Described::default(),
        evidence: Default::default(),
    };
    farm.submit(submission).expect("submitted")
}

/// Moves the clock past the wait that ends in a refusal for want of a daemon.
async fn wait_out_the_unservable_wait() {
    tokio::time::advance(kbf_sched::UNSERVABLE_WAIT + Duration::from_secs(1)).await;
}

/// Removes the session row of `worker`, as the store's clock does when the frontend it is
/// connected to stops renewing it.
fn expire_session(disk: &Disk, worker: &str) {
    let expired = Txn::new().delete(format!("session/{worker}"), kbf_state::Cond::Any);
    disk.store.txn(expired).expect("store").expect("delete");
}

/// Asserts that a lease a frontend gives up after another frontend has already given it up
/// and granted the operation again queues nothing: the operation stays under its new lease,
/// the old lease's result is refused, and the new lease's result finishes the operation,
/// which is then neither queued nor run again.
pub(super) async fn a_second_give_up_queues_nothing_beside_the_new_lease(backing: Backing) {
    let disk = Disk::over(backing);
    let (node_a, node_b) = (WorkerId::new("node-a"), WorkerId::new("node-b"));
    let first = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox_a) = register_as(&first, &node_a);
    submit(&first, request(1)).expect("submitted");
    let [old] = starts(&mut inbox_a)[..] else {
        panic!("one lease expected");
    };
    let second = disk.farm(EpochPolicy::Keep);
    tokio::time::advance(kbf_sched::fence::LEASE_GRACE + Duration::from_secs(1)).await;

    // The second frontend gives up the lease whose daemon it never heard, and runs the
    // operation on a daemon of its own.
    let (_, mut inbox_b) = register_as(&second, &node_b);
    let [new] = starts(&mut inbox_b)[..] else {
        panic!("one lease expected");
    };
    assert_ne!(new, old);
    assert_eq!((disk.rows("lease/"), disk.rows("queue/")), (1, 0));

    // The first gives up the old lease too, for the daemon it has not heard either.
    first.tick().await;
    assert_eq!(disk.rows("queue/"), 0, "a running operation was queued");
    assert_eq!(disk.rows("lease/"), 1);
    let held = disk.store.get(&lease_row(new)).expect("read");
    assert!(held.is_some(), "the new lease's row was removed");
    assert!(!finished_by(&first, &node_a, old).await);

    assert!(finished_by(&second, &node_b, new).await);
    assert_eq!((disk.rows("op/"), disk.rows("lease/")), (0, 0));
    assert_eq!((disk.rows("queue/"), disk.rows("done/")), (0, 1));
}

/// Asserts that a daemon's result that reaches two frontends finishes the operation once:
/// the second frontend's finish finds the first's result and is acknowledged as a
/// duplicate, and both frontends hold the one result.
pub(super) async fn a_result_reaching_two_frontends_is_acknowledged_once(backing: Backing) {
    let disk = Disk::over(backing);
    let first = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&first);
    submit(&first, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    let second = disk.farm(EpochPolicy::Keep);
    let (stream, _inbox) = register(&second);
    assert!(second.heartbeat(&node(), stream, 1, vec![lease]).await);

    assert!(accepted(&first, lease).await);
    assert!(
        accepted(&second, lease).await,
        "the duplicate was not acknowledged"
    );

    let operation = OperationId(0);
    let sent = first.ended(operation).expect("read");
    let Some(Finished::Failed(status)) = &sent else {
        panic!("the result was not the original report");
    };
    assert_eq!(status.message, "test");
    assert_eq!(second.ended(operation).expect("read"), sent);
    assert_eq!((disk.rows("op/"), disk.rows("lease/")), (0, 0));
    assert_eq!((disk.rows("queue/"), disk.rows("done/")), (0, 1));
}

/// Asserts that an operation no daemon of its own frontend could run is not refused while a
/// daemon connected to another frontend could: the session rows of the store decide, the
/// refusal waits another unservable wait each time it comes due, and it is made once no
/// session in the store could run the operation.
pub(super) async fn a_daemon_of_another_frontend_holds_back_the_refusal(backing: Backing) {
    let disk = Disk::over(backing);
    let first = disk.farm(EpochPolicy::Keep);
    let second = disk.farm(EpochPolicy::Keep);
    let (_, _inbox) = register_as(&second, &WorkerId::new("node-b"));
    let ticket = caller(&first, 1);

    for _ in 0..2 {
        wait_out_the_unservable_wait().await;
        first.tick().await;
        assert_eq!((disk.rows("op/"), disk.rows("done/")), (1, 0));
        assert!(!matches!(*ticket.stage.borrow(), Stage::Done(_)));
        assert_eq!(first.queued_operations(), 1, "the operation left the queue");
    }

    expire_session(&disk, "node-b");
    wait_out_the_unservable_wait().await;
    first.tick().await;
    let Finished::Failed(status) = ended(&ticket) else {
        panic!("the caller was answered with something else");
    };
    assert_eq!(status.code, Code::FailedPrecondition as i32);
    assert_eq!((disk.rows("op/"), disk.rows("queue/")), (0, 0));
    assert_eq!(disk.rows("done/"), 1);
}

/// Asserts that a refusal loses to a grant another frontend made: the store names no daemon
/// that could run the operation, the frontend refuses it, and the write is refused because
/// the operation is no longer queued. The operation stays, runs under the other frontend's
/// lease and finishes.
pub(super) async fn a_refusal_loses_to_a_grant_made_by_another_frontend(backing: Backing) {
    let disk = Disk::over(backing);
    let node_b = WorkerId::new("node-b");
    let first = disk.farm(EpochPolicy::Keep);
    submit(&first, request(1)).expect("queued, no daemon here");
    let second = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register_as(&second, &node_b);
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    expire_session(&disk, "node-b");

    wait_out_the_unservable_wait().await;
    first.tick().await;

    assert_eq!(first.dropped_writes(), 1, "the refusal was written");
    assert_eq!((disk.rows("op/"), disk.rows("lease/")), (1, 1));
    assert_eq!(disk.rows("done/"), 0);
    assert!(finished_by(&second, &node_b, lease).await);
    assert_eq!((disk.rows("op/"), disk.rows("lease/")), (0, 0));
    assert_eq!(disk.rows("done/"), 1);
}

/// Asserts that a refusal is held back while the session rows of the store cannot be read:
/// it is never made on a guess, and it is made once they can be read and name no daemon
/// that could run the operation.
pub(super) async fn daemon_sessions_that_cannot_be_read_hold_back_a_refusal(backing: Backing) {
    let disk = Disk::over(backing);
    let farm = disk.farm(EpochPolicy::Keep);
    let ticket = caller(&farm, 1);
    let broken = Txn::new().put("session/broken", vec![0xff], kbf_state::Cond::Any);
    disk.store.txn(broken).expect("store").expect("put");

    wait_out_the_unservable_wait().await;
    farm.tick().await;

    assert_eq!((disk.rows("op/"), disk.rows("done/")), (1, 0));
    assert!(!matches!(*ticket.stage.borrow(), Stage::Done(_)));
    expire_session(&disk, "broken");
    wait_out_the_unservable_wait().await;
    farm.tick().await;
    let Finished::Failed(status) = ended(&ticket) else {
        panic!("the caller was answered with something else");
    };
    assert_eq!(status.code, Code::FailedPrecondition as i32);
    assert_eq!((disk.rows("op/"), disk.rows("done/")), (0, 1));
}
