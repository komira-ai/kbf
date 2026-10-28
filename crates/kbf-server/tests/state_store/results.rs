//! Results across the frontends that share one state store: a client that submits through
//! one frontend and waits through another gets the result, an operation's name is answered
//! by every frontend, a request that joins an operation another frontend leased follows it
//! through the store, and a finished result is answered after a reconnect for as long as the
//! store keeps it. Each test runs over the fake, a SQLite file and a Postgres schema (the
//! `restarts!` list of the parent module), except where it moves the store's clock.

use kbf_sched::fence::LEASE_GRACE;
use kbf_server::DONE_FLOOR;

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

/// What `ticket` shows now.
fn shown(ticket: &kbf_front::Ticket) -> Stage {
    ticket.stage.borrow().clone()
}

/// The message of the failure a caller was answered with.
fn answered_with(ticket: &kbf_front::Ticket) -> String {
    let Finished::Failed(status) = ended(ticket) else {
        panic!("the caller was answered with something else");
    };
    status.message
}

/// Asserts that a client that submits through one frontend and waits through another gets the
/// result: the operation queued through the first runs on a daemon of the second, and the
/// client, cut from the first, asks a third frontend (which holds nothing) for the operation
/// by its name, is shown it executing, and gets the result the second frontend records; the
/// first frontend's own caller gets it at its poll.
pub(super) async fn a_client_waiting_through_another_frontend_gets_the_result(backing: Backing) {
    let disk = Disk::over(backing);
    let (a, b, c) = (
        disk.farm(EpochPolicy::Keep),
        disk.farm(EpochPolicy::Keep),
        disk.farm(EpochPolicy::Keep),
    );
    let (_, mut inbox) = register(&b);
    let submitted = caller(&a, 1);
    let name = submitted.name.clone();
    b.intake();
    b.tick().await;
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };

    let waiting = c.wait(&name).expect("any frontend answers a name");

    assert_eq!(shown(&waiting), Stage::Executing);
    assert!(finished_by(&b, &node(), lease).await);
    c.intake();
    assert_eq!(answered_with(&waiting), "test");
    a.intake();
    assert_eq!(answered_with(&submitted), "test");
    assert_eq!((disk.rows("done/"), disk.rows("doneof/")), (1, 1));
}

/// Asserts that a name in the form `operations/<id>` is answered by every frontend of the
/// store at every stage of the operation: queued, executing and finished, each with the
/// instance and action it runs, by a frontend that holds the operation and by one that holds
/// nothing, and by one started after the finish; and that a name nobody gave is not found.
pub(super) async fn a_name_in_the_new_form_is_answered_by_every_frontend_at_every_stage(
    backing: Backing,
) {
    let disk = Disk::over(backing);
    let (a, b, c) = (
        disk.farm(EpochPolicy::Keep),
        disk.farm(EpochPolicy::Keep),
        disk.farm(EpochPolicy::Keep),
    );
    let submitted = caller(&a, 1);
    let name = submitted.name.clone();
    assert!(name.starts_with("operations/"), "{name}");
    assert!(!name.contains('-'), "{name} is in the old form");

    for farm in [&b, &c] {
        let queued = farm.wait(&name).expect("queued in the store");
        assert_eq!(shown(&queued), Stage::Queued);
        assert_eq!(queued.instance, "main");
        assert_eq!(queued.action, action_of(1));
    }
    let (_, mut inbox) = register(&b);
    b.intake();
    b.tick().await;
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    let running = c.wait(&name).expect("leased in the store");
    assert_eq!(shown(&running), Stage::Executing);
    assert!(finished_by(&b, &node(), lease).await);
    // The frontend that queued the operation learns of the finish at its poll.
    a.intake();
    let late = disk.farm(EpochPolicy::Keep);

    for farm in [&a, &b, &c, &late] {
        let kept = farm.wait(&name).expect("the result is kept");
        assert_eq!(answered_with(&kept), "test");
    }
    assert_eq!(answered_with(&running), "test");
    assert!(c.wait("operations/987654").is_none());
}

/// Asserts that a name in the old form, `operations/{term}-{waiter}`, is still answered by the
/// frontend that issued it, for the caller that frontend holds, with the instance and action
/// of its operation and the stage that caller shows, and that a name of that form which no
/// caller of the frontend has is not found.
pub(super) async fn an_old_form_name_is_answered_by_the_issuing_frontend(backing: Backing) {
    let disk = Disk::over(backing);
    let farm = disk.farm(EpochPolicy::Keep);
    let submitted = caller(&farm, 1);
    // The first caller of a process is waiter 0, and the term of a farm over a store with no
    // fence is the store's epoch.
    let epoch = farm.lease_epoch();
    let old = format!("operations/{epoch}-0");
    assert_ne!(old, submitted.name, "the farm gave the new form");

    let waited = farm.wait(&old).expect("the frontend holds the caller");

    assert_eq!(waited.name, old);
    assert_eq!(waited.instance, "main");
    assert_eq!(waited.action, action_of(1));
    // No worker is connected, so the caller is told why it waits; the old name shows the
    // stage of that caller, not one of its own.
    assert_eq!(
        shown(&waited),
        Stage::Waiting("no worker is connected".to_owned())
    );
    assert_eq!(shown(&waited), shown(&submitted));
    assert!(farm.wait(&format!("operations/{epoch}-9")).is_none());
}

/// Asserts that a request that joins an operation another frontend leased on its daemon
/// follows it through the store and never takes the lease up, however long the joiner runs on:
/// the joiner holds no lease, the lease row stands and the operation is not queued again, and
/// the joiner gets the result once, from the store, when the other frontend records it.
pub(super) async fn a_joiner_of_a_leased_operation_never_deletes_its_lease(backing: Backing) {
    let disk = Disk::over(backing);
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let (_, mut inbox) = register(&a);
    let leased = caller(&a, 1);
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };

    let joined = caller(&b, 1);

    assert_eq!(joined.name, leased.name);
    assert_eq!(shown(&joined), Stage::Executing);
    assert!(held(&b).is_empty(), "the joiner took the lease up");
    for _ in 0..3 {
        tokio::time::advance(LEASE_GRACE).await;
        b.tick().await;
        b.intake();
        let rows = (disk.rows("lease/"), disk.rows("queue/"));
        assert_eq!(rows, (1, 0), "the joiner gave up the lease");
    }
    assert!(finished_by(&a, &node(), lease).await);
    assert_eq!(shown(&joined), Stage::Executing, "answered before the poll");
    b.intake();
    assert_eq!(answered_with(&joined), "test");
    b.intake();
    assert_eq!(answered_with(&joined), "test");
    assert_eq!((disk.rows("lease/"), disk.rows("done/")), (0, 1));
}

/// Asserts that a finished result is answered by name to a client that reconnects to another
/// frontend after the finish, within the least time a result is kept ([`DONE_FLOOR`], at
/// least ten minutes), and that it is not once the store has expired it. Over the fake only:
/// the store's clock is moved by hand, so the expiry of the `doneof/` row over Postgres is not
/// exercised.
#[tokio::test(start_paused = true)]
async fn a_finished_result_is_answered_after_a_reconnect_within_the_floor() {
    let disk = Disk::new();
    let a = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&a);
    let submitted = caller(&a, 1);
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    assert!(finished_by(&a, &node(), lease).await);
    let name = submitted.name.clone();

    let reconnected = disk.farm(EpochPolicy::Keep);
    let kept = reconnected.wait(&name).expect("answered at once");
    assert_eq!(answered_with(&kept), "test");
    disk.pass(DONE_FLOOR - Duration::from_secs(1));
    let within = disk.farm(EpochPolicy::Keep);
    let still = within.wait(&name).expect("answered within the floor");
    assert_eq!(answered_with(&still), "test");

    disk.pass(Duration::from_secs(2));
    let after = disk.farm(EpochPolicy::Keep);
    assert!(after.wait(&name).is_none(), "answered after the floor");
    assert_eq!((disk.rows("done/"), disk.rows("doneof/")), (0, 0));
}
