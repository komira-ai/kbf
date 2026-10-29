//! A farm taken out of service for a successor: what it stops doing, how it answers the
//! callers it kept, and the operator's cordon across the change.

use kbf_front::Backend;
use kbf_server::farm::NodeAction;
use kbf_server::fleet::PlacementView;
use kbf_state::{Cond, Txn};

use super::*;

/// A farm over `disk` whose cache the test keeps, so it can read its metrics.
fn farm_over_disk(disk: &Disk, wait: Duration) -> (TestFarm, Backend) {
    let backend = Backend::from(Arc::new(Cache::memory()));
    let store: Arc<dyn Store> = disk.store.clone();
    let farm = Farm::new(backend.clone(), wait, kbf_sched::FINISHED_RETENTION)
        .with_state(Some(StateStore {
            store,
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store");
    (farm, backend)
}

/// A request for more memory than the node has: it stays queued.
fn unplaceable(tag: u8) -> Request {
    Request {
        resources: Resources::new(1_000, 64 * GIB),
        ..request(tag)
    }
}

fn queue(farm: &TestFarm, request: Request) -> kbf_front::Ticket {
    farm.submit(Submission {
        request,
        call: None,
        described: Described::default(),
        evidence: Default::default(),
        max_wall: None,
    })
    .expect("queued")
}

fn done(ticket: &kbf_front::Ticket) -> Option<Finished> {
    match &*ticket.stage.borrow() {
        Stage::Done(finished) => Some(finished.clone()),
        _ => None,
    }
}

fn status(finished: &Finished) -> (i32, String) {
    match finished {
        Finished::Failed(status) => (status.code, status.message.clone()),
        other => panic!("{other:?}"),
    }
}

/// Catches: a fenced farm that still takes work or writes: it must queue no request
/// (UNAVAILABLE, which Buck2 asks again), grant nothing, accept no result (the daemon
/// sends it again to the successor), change nothing a heartbeat or a tick would, and
/// write nothing to the store at all (not a renewal, not a ping), since from then on the
/// successor's rows are the truth. Catches: the fence flag not checked in `admit`, in
/// `feed`, or in the session renewal.
#[tokio::test(start_paused = true)]
async fn a_fenced_farm_queues_nothing_accepts_nothing_and_writes_nothing() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&farm);
    submit(&farm, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    submit(&farm, unplaceable(2)).expect("queued, never placed");
    let rows = disk.all_rows();

    farm.fence();
    let writes = disk.store.commit_stats().txns;
    let refused = submit(&farm, request(3)).expect_err("a fenced farm queued a request");
    assert_eq!(refused.code(), Code::Unavailable);
    assert!(refused.message().contains("handing its work"), "{refused}");
    farm.tick().await;
    tokio::time::advance(kbf_server::farm::SESSION_RENEWAL * 2).await;
    assert!(farm.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert!(!accepted(&farm, lease).await, "a fenced farm took a result");
    assert!(starts(&mut inbox).is_empty());
    assert_eq!(
        disk.store.commit_stats().txns,
        writes,
        "a fenced farm wrote"
    );
    assert_eq!(disk.all_rows(), rows);

    // Not fenced, the same result is taken: the refusal is the fence's.
    let other = disk.farm(EpochPolicy::Keep);
    let (stream, _inbox) = register(&other);
    assert!(other.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert!(accepted(&other, lease).await);
}

/// Catches: daemon streams left running after a handover (a daemon would go on talking
/// to a process that no longer schedules), a status that does not reach them, and a
/// count that includes a stream already gone.
#[tokio::test(start_paused = true)]
async fn the_worker_streams_are_ended_with_a_status() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let (outbound, gone) = mpsc::unbounded_channel();
    drop(gone);
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    farm.register(
        &WorkerId::new("node-b"),
        DaemonInstance::new("daemon-b"),
        DaemonInstance::new(""),
        Reported::serving(Resources::new(4_000, 8 * GIB), caps),
        outbound,
        ServerMessage::default(),
    );
    let ended = farm.end_worker_streams(&Status::unavailable("handed over"));
    assert_eq!(ended, 1, "only the stream still open counts");
    let _welcome = inbox.recv().await.expect("the Welcome");
    let message = inbox.recv().await.expect("a message");
    assert_eq!(message.expect_err("a status").code(), Code::Unavailable);
}

/// Catches: a handover that goes ahead while a call is between its request and its
/// queueing, and one that never goes ahead: the farm is quiet only when no such call is
/// in flight.
#[tokio::test]
async fn a_farm_is_quiet_when_no_call_is_between_its_request_and_its_queueing() {
    let disk = Disk::new();
    let (farm, backend) = farm_over_disk(&disk, kbf_sched::UNSERVABLE_WAIT);
    assert!(farm.quiet());
    let busy = backend.metrics().execute_presubmit_in_flight.raise();
    assert!(!farm.quiet());
    drop(busy);
    assert!(farm.quiet());
}

/// Asserts that a fenced farm answers the callers it kept from what the successor wrote,
/// whatever the successor does: it finishes an operation (its `done/` row is the answer),
/// leaves one queued (nothing changes), or leaves no trace of one. A caller whose
/// operation has no trace is never answered by a look count or a timer: it waits through
/// any number of looks until the operation finishes or the client cancels. A look that
/// finds the operation again shows it executing, and a caller that cancels (drops its
/// stream) is no longer counted waiting. Each follow returns how many callers still wait.
#[tokio::test(start_paused = true)]
async fn a_fenced_farm_answers_its_callers_from_what_the_successor_wrote() {
    let disk = Disk::new();
    let (old, _) = farm_over_disk(&disk, kbf_sched::UNSERVABLE_WAIT);
    let (_, mut inbox) = register(&old);
    let ran = queue(&old, request(1));
    let queued = queue(&old, unplaceable(2));
    let vanishes = queue(&old, request(3));
    let flickers = queue(&old, request(4));
    let [lease, _, _] = starts(&mut inbox)[..] else {
        panic!("three leases expected");
    };
    old.fence();
    assert_eq!(old.follow().expect("follow"), 4, "all four wait");

    // The successor: the same store, the epoch kept; the daemon reconnects to it and
    // lists its lease.
    let (new, _) = farm_over_disk(&disk, kbf_sched::UNSERVABLE_WAIT);
    let (stream, _inbox) = register(&new);
    assert!(new.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert!(
        accepted(&new, lease).await,
        "the successor takes the result"
    );
    assert_eq!(old.follow().expect("follow"), 3);
    let finished = done(&ran).expect("answered from the done row");
    assert_eq!(
        status(&finished),
        (Code::InvalidArgument as i32, "test".to_owned())
    );
    assert!(done(&queued).is_none() && done(&vanishes).is_none());

    // Two operations lose their rows and stay lost, through many looks: nothing answers
    // them.
    let key = |prefix: &str, op: u64| format!("{prefix}{op:016x}");
    let flicker_row = disk
        .store
        .get(&key("op/", 3))
        .expect("store")
        .expect("its row");
    let erase = |op: u64| {
        let txn = Txn::new()
            .delete(key("op/", op), Cond::Any)
            .delete(key("queue/", op), Cond::Any);
        disk.store.txn(txn).expect("store").expect("erased");
    };
    erase(2);
    erase(3);
    assert_eq!(old.follow().expect("follow"), 3);
    for _ in 0..200 {
        assert_eq!(old.follow().expect("follow"), 3, "nobody is answered");
    }
    assert!(done(&queued).is_none() && done(&vanishes).is_none() && done(&flickers).is_none());

    // Operation 3 shows up again (leased: shown executing).
    let back = Txn::new().put(key("op/", 3), flicker_row.value, Cond::Any);
    disk.store.txn(back).expect("store").expect("back");
    assert_eq!(old.follow().expect("follow"), 3);
    assert!(matches!(&*flickers.stage.borrow(), Stage::Executing));

    // The caller of the other one cancels: it is no longer waiting.
    drop(vanishes);
    assert_eq!(old.follow().expect("follow"), 2);

    // Lost again, and its caller still waits; it cancels and is no longer counted.
    erase(3);
    for _ in 0..200 {
        assert_eq!(old.follow().expect("follow"), 2);
    }
    drop(flickers);
    assert_eq!(old.follow().expect("follow"), 1);
}

/// Catches: a farm with no state store that panics or claims callers waiting when told
/// to follow a successor that has no rows to read.
#[tokio::test]
async fn a_farm_with_no_state_store_follows_no_one() {
    let farm = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    farm.fence();
    assert_eq!(farm.follow().expect("follow"), 0);
}

/// Asserts what following the callers of a fenced farm does when the `done/` row of an
/// operation they wait on cannot be read: a corrupt store is never taken for the absence
/// of an operation. The legacy core fails the follow, naming the row, and leaves the caller
/// waiting. The js core reads the row's bytes and ends the caller with UNAVAILABLE, which
/// clients ask again, so that no caller waits for a result that will not come; the follow
/// itself succeeds, with nobody left waiting.
#[tokio::test(start_paused = true)]
async fn following_a_done_row_that_cannot_be_read_is_an_error_naming_the_row() {
    let disk = Disk::new();
    let (old, _) = farm_over_disk(&disk, kbf_sched::UNSERVABLE_WAIT);
    let waiting = queue(&old, unplaceable(1));
    old.fence();
    let key = "done/0000000000000000";
    let row = Txn::new().put(key.to_owned(), vec![0xff], Cond::Any);
    disk.store.txn(row).expect("store").expect("put");
    if old.scheduler_core() == kbf_server::SchedulerCore::Js {
        assert_eq!(old.follow().expect("the follow"), 0);
        let ended = done(&waiting).expect("the caller is answered");
        assert_eq!(
            status(&ended),
            (
                Code::Unavailable as i32,
                "the stored result of this operation cannot be read".to_owned()
            )
        );
        return;
    }
    let refused = old.follow().expect_err("an unreadable done row");
    assert!(refused.to_string().contains(key), "{refused}");
    assert!(done(&waiting).is_none());
}

/// What the operator did to `node-a`, as the farm shows it.
fn placement(farm: &TestFarm) -> PlacementView {
    farm.node_view(&node())
        .expect("the node registered")
        .placement
}

/// Catches: an operator's cordon or drain forgotten across a start (the successor of a
/// handover would offer leases to a node taken out of placement for an update), a drain
/// that comes back without its deadline (a node drained for an hour paused at once, or
/// never), one whose deadline had passed that does not pause at the first tick, and a
/// node returned to placement that comes back cordoned. Catches: no row written by
/// `place`; the deadline read as a duration; the row of an uncordon not deleted.
async fn a_cordon_survives_a_restart(backing: Backing) {
    let mut disk = Disk::over(backing);
    let before = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&before);
    submit(&before, request(1)).expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1, "the node holds a lease");

    // A plain cordon.
    before.place(&node(), NodeAction::Cordon).expect("cordon");
    let after = {
        disk.crash(before);
        disk.farm(EpochPolicy::Keep)
    };
    let (_stream, mut inbox) = register(&after);
    assert_eq!(placement(&after), PlacementView::Cordoned);
    submit(&after, request(2)).expect("queued");
    assert!(
        starts(&mut inbox).is_empty(),
        "a cordoned node was offered a lease after a restart"
    );

    // A drain with an hour to run keeps its deadline.
    let wall = |view: PlacementView| match view {
        PlacementView::Draining {
            deadline_unix_ms, ..
        } => deadline_unix_ms.expect("a deadline"),
        other => panic!("{other:?}"),
    };
    after
        .place(&node(), NodeAction::Drain(Duration::from_secs(3600)))
        .expect("drain");
    let drain_deadline = wall(placement(&after));
    disk.crash(after);
    let after = disk.farm(EpochPolicy::Keep);
    let (_stream, _inbox) = register(&after);
    let restored = wall(placement(&after));
    assert!(
        restored.abs_diff(drain_deadline) < 10_000,
        "the deadline moved from {drain_deadline} to {restored}"
    );

    // A drain whose deadline has passed pauses at the first tick.
    after
        .place(&node(), NodeAction::Drain(Duration::ZERO))
        .expect("drain");
    disk.crash(after);
    let after = disk.farm(EpochPolicy::Keep);
    let (stream, _inbox) = register(&after);
    assert!(after.heartbeat(&node(), stream, 1, Vec::new()).await);
    after.tick().await;
    assert!(
        matches!(placement(&after), PlacementView::DrainPaused { .. }),
        "{:?}",
        placement(&after)
    );

    // Back in placement: nothing is left to come back.
    after
        .place(&node(), NodeAction::Uncordon)
        .expect("uncordon");
    assert_eq!(disk.rows("cordon/"), 0);
    disk.crash(after);
    let after = disk.farm(EpochPolicy::Keep);
    let (_stream, _inbox) = register(&after);
    assert_eq!(placement(&after), PlacementView::Serving);
}

#[tokio::test(start_paused = true)]
async fn a_cordon_survives_a_restart_over_the_fake() {
    a_cordon_survives_a_restart(Backing::Fake).await;
}

#[tokio::test(start_paused = true)]
async fn a_cordon_survives_a_restart_over_sqlite() {
    a_cordon_survives_a_restart(Backing::Sqlite).await;
}

/// A farm over `disk` that tells `recorder` what happens to its operations.
fn recorded_farm(disk: &Disk, recorder: &kbf_front::Recorder) -> TestFarm {
    let store: Arc<dyn Store> = disk.store.clone();
    Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .recording(recorder.clone())
    .with_state(Some(StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }))
    .expect("a farm over the store")
}

/// Asserts that after a retire the other frontend's recorder has no record of a build the
/// retired frontend recorded, while the caller the retired frontend kept is still answered
/// from the `done/` row. See `docs/design/frontend-proxy.md` section 4 ("What a roll does
/// not carry": an invocation record is split between the processes). The invocation
/// record is built by a process's own recorder from the events its own farm sent, and no
/// row of the state store carries it. Catches: a build result that depended on the
/// recorder (the caller left waiting for a record that is not there), and a change that
/// starts carrying the record (this test then fails, and the doc line must change with it).
/// No change to the farm makes the successor find it, since nothing writes it to the store:
/// this test pins the documented loss; it cannot catch a regression that has no code to
/// regress.
#[tokio::test(start_paused = true)]
async fn a_successors_recorder_has_no_record_of_a_build_but_its_caller_is_answered() {
    let disk = Disk::new();
    let (before, recording) = kbf_front::Recorder::new(1 << 20, 64);
    tokio::spawn(recording.run());
    let metadata = kbf_front::CallMetadata {
        invocation_id: kbf_front::InvocationId::parse("build-1"),
        ..Default::default()
    };
    let key = kbf_invocation::RecordKey::owner(kbf_invocation::InvocationId::new("build-1"));
    let asked = request(1);
    let action = action_of(1);
    let call = kbf_front::ExecuteCall::new(
        &before,
        &metadata,
        &Default::default(),
        "main",
        action,
        action,
    )
    .expect("a recorded call");

    let old = recorded_farm(&disk, &before);
    let (_, mut inbox) = register(&old);
    let ticket = old
        .submit(Submission {
            request: asked,
            call: Some(call),
            described: Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("queued");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    before.caught_up().await;
    assert!(before.get(&key).is_some(), "the predecessor recorded it");

    old.fence();
    assert_eq!(old.follow().expect("follow"), 1);

    // The successor has a recorder of its own, as a new process does.
    let (after, recording) = kbf_front::Recorder::new(1 << 20, 64);
    tokio::spawn(recording.run());
    let new = recorded_farm(&disk, &after);
    let (stream, _inbox) = register(&new);
    assert!(new.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert!(
        accepted(&new, lease).await,
        "the successor takes the result"
    );
    after.caught_up().await;
    assert!(
        after.get(&key).is_none(),
        "the successor has no record of the build"
    );

    // The caller the predecessor kept is answered from the row, whatever the recorder says.
    assert_eq!(old.follow().expect("follow"), 0);
    let finished = done(&ticket).expect("answered from the done row");
    assert_eq!(
        status(&finished),
        (Code::InvalidArgument as i32, "test".to_owned())
    );
}
