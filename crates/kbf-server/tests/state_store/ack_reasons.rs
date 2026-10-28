//! The reason a `ResultAck` gives. The lease id is the attempt (a re-grant of an operation
//! gets a new lease id), so each way a `Result` can be refused has its own reason, and a
//! `Result` of an earlier attempt never reaches the action cache or the operation.

use kbf_proto::worker::ResultAckReason as Reason;

use super::*;

fn reason_of(ack: &worker::ResultAck) -> Reason {
    Reason::try_from(ack.reason).expect("a known reason")
}

/// A started lease on a farm over a fresh store, with its caller's ticket.
async fn started() -> (Disk, TestFarm, Inbox, LeaseId, kbf_front::Ticket) {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let ticket = farm
        .submit(Submission {
            request: request(1),
            call: None,
            described: Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("submitted");
    let lease = starts(&mut inbox)[0];
    (disk, farm, inbox, lease, ticket)
}

/// Catches: a farm that leaves the reason unspecified for the Result it accepts (a
/// daemon could not tell it from a server that predates the field), or reports it
/// accepted without `accepted`.
#[tokio::test(start_paused = true)]
async fn an_accepted_result_says_accepted() {
    let (_disk, farm, _inbox, lease, ticket) = started().await;
    let ack = farm.report(&node(), invalid(lease)).await.expect("an ack");
    assert!(ack.accepted);
    assert_eq!(reason_of(&ack), Reason::Accepted);
    assert!(matches!(ended(&ticket), Finished::Failed(_)));
}

/// Catches: a copy of an answered operation's Result that is called stale or unknown (the
/// first valid writer won, and the daemon's log would say the lease was lost), or one
/// that is accepted a second time.
#[tokio::test(start_paused = true)]
async fn a_copy_of_an_answered_result_is_a_duplicate() {
    let (disk, farm, _inbox, lease, _ticket) = started().await;
    assert!(accepted(&farm, lease).await);
    let copy = farm.report(&node(), invalid(lease)).await.expect("an ack");
    assert!(!copy.accepted);
    assert_eq!(reason_of(&copy), Reason::Duplicate);
    assert_eq!(disk.rows("done/"), 1);
}

/// Catches: a farm that takes the Result of a lease it gave up (the operation is run
/// again under a new lease, and two runs would write one result), one that ignores the
/// lease's sequence and compares only the operation, and one that says duplicate or
/// unknown for it. The lease the node lost is granted again as `seq` 1; the Result of the
/// first is STALE, answers nobody and writes nothing, and the Result of the second is
/// accepted.
#[tokio::test(start_paused = true)]
async fn the_result_of_a_lease_given_up_and_granted_again_is_stale() {
    let (disk, farm, _inbox, first, ticket) = started().await;
    let (_, mut inbox) = lose_lease(&farm).await;
    let second = starts(&mut inbox)[0];
    assert_eq!(second, LeaseId::new(first.term, first.seq + 1));

    let stale = farm.report(&node(), invalid(first)).await.expect("an ack");
    assert!(!stale.accepted);
    assert_eq!(reason_of(&stale), Reason::Stale);
    assert!(
        !matches!(*ticket.stage.borrow(), Stage::Done(_)),
        "answered"
    );
    assert_eq!(disk.rows("done/"), 0);

    assert!(accepted(&farm, second).await);
    assert_eq!(disk.rows("done/"), 1);
}

/// Catches: a farm that takes the Result of a lease from a node other than the one that
/// holds it, and one that says unknown for it (the lease is known; it is not that node's).
#[tokio::test(start_paused = true)]
async fn the_result_of_another_nodes_lease_is_stale() {
    let (disk, farm, _inbox, lease, ticket) = started().await;
    let other = WorkerId::new("node-b");
    let ack = farm.report(&other, invalid(lease)).await.expect("an ack");
    assert!(!ack.accepted);
    assert_eq!(reason_of(&ack), Reason::Stale);
    assert!(
        !matches!(*ticket.stage.borrow(), Stage::Done(_)),
        "answered"
    );
    assert_eq!(disk.rows("done/"), 0);
    assert!(accepted(&farm, lease).await, "the holder's result is taken");
}

/// Catches: a farm that says stale or duplicate for a lease it never granted (a lease of
/// another epoch, or of a process before a restart): the daemon's WARN line would blame
/// a lost lease, not a lease this server does not know.
#[tokio::test(start_paused = true)]
async fn the_result_of_a_lease_the_farm_never_granted_is_unknown() {
    let (_disk, farm, _inbox, lease, _ticket) = started().await;
    let foreign = LeaseId::new(lease.term + 1, lease.seq);
    let ack = farm
        .report(&node(), invalid(foreign))
        .await
        .expect("an ack");
    assert!(!ack.accepted);
    assert_eq!(reason_of(&ack), Reason::UnknownLease);
    assert_eq!(
        ack.lease_id,
        Some(worker::LeaseId {
            term: foreign.term,
            seq: foreign.seq
        })
    );
}

/// Catches: a farm that takes a Result naming another action than its lease runs, and
/// one that gives a mismatch the reason of a stale lease. The refusal does not consume
/// the lease: the Result that names its own action is then accepted.
#[tokio::test(start_paused = true)]
async fn a_result_naming_another_action_is_a_mismatch() {
    let (disk, farm, _inbox, lease, _ticket) = started().await;
    let other = Digest::new(DigestFunction::Sha256, [9; 32], 9);
    let wrong = worker::Result {
        action_digest: Some(kbf_front::digest_to_proto(&other)),
        ..invalid(lease)
    };
    let ack = farm.report(&node(), wrong).await.expect("an ack");
    assert!(!ack.accepted);
    assert_eq!(reason_of(&ack), Reason::Mismatch);
    assert_eq!(disk.rows("done/"), 0);
    assert!(accepted(&farm, lease).await);
}

/// Opens a farm over `disk` the way a restarted process does, registers the node, and
/// submits `tag`'s operation: the farm, the operation's ticket, and every lease it started.
fn restarted(
    disk: &Disk,
    epoch: EpochPolicy,
    tag: u8,
) -> (TestFarm, Inbox, kbf_front::Ticket, Vec<LeaseId>) {
    let farm = disk.farm(epoch);
    let (_, mut inbox) = register(&farm);
    let ticket = farm
        .submit(Submission {
            request: request(tag),
            call: None,
            described: Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("submitted");
    let leases = starts(&mut inbox);
    (farm, inbox, ticket, leases)
}

/// A `Result` of `lease` that names the action of `request(tag)`.
fn result_naming(lease: LeaseId, tag: u8) -> worker::Result {
    worker::Result {
        action_digest: Some(kbf_front::digest_to_proto(&action_of(tag))),
        ..invalid(lease)
    }
}

/// Catches: a farm restarted over a state store that grants a lease id its predecessor
/// already granted (the daemon still holds the predecessor's id for operation X, and its
/// `Result` would finish whichever operation the new farm gave that id to), and one that
/// applies a predecessor's `Result` to an operation other than the lease's own. The new
/// process never grants X's id again: a bumped epoch gives it another term, and a kept
/// one numbers from the block of sequences it reserved past its predecessor's. The farm still holds X's lease, restored from
/// its row (the daemon may still run it), so a `Result` of that id that names Y's action
/// is a MISMATCH and ends nothing; the one that names X's action ends X alone, and Y is
/// still open until Y's own lease reports; an id no row holds is UNKNOWN_LEASE.
async fn a_predecessor_process_s_lease_id_never_names_another_operation(backing: Backing) {
    for epoch in [EpochPolicy::Bump, EpochPolicy::Keep] {
        let mut disk = Disk::over(backing);
        let (before, _inbox, _x, first) = restarted(&disk, EpochPolicy::Keep, 1);
        let [x_lease] = first[..] else {
            panic!("one lease expected");
        };
        disk.crash(before);

        let (after, _inbox, y, second) = restarted(&disk, epoch, 2);
        let [y_lease] = second[..] else {
            panic!("one lease for Y expected, got {second:?}");
        };
        assert_ne!(y_lease, x_lease, "{epoch:?}: a lease id was granted twice");
        match epoch {
            EpochPolicy::Bump => assert_ne!(y_lease.term, x_lease.term, "the term was kept"),
            EpochPolicy::Keep => assert_eq!(y_lease, LeaseId::new(x_lease.term, ID_BLOCK)),
        }

        // X's lease, with Y's action: refused, and nothing is answered.
        let ack = after
            .report(&node(), result_naming(x_lease, 2))
            .await
            .expect("an ack");
        assert!(!ack.accepted, "{epoch:?}: {ack:?}");
        assert_eq!(reason_of(&ack), Reason::Mismatch, "{epoch:?}");
        assert!(!matches!(*y.stage.borrow(), Stage::Done(_)), "Y answered");
        assert_eq!(disk.rows("done/"), 0, "{epoch:?}");

        // An id no process granted and no row holds (another term): unknown, nothing ends.
        let foreign = LeaseId::new(y_lease.term + 1000, x_lease.seq);
        let ack = after
            .report(&node(), result_naming(foreign, 2))
            .await
            .expect("an ack");
        assert!(!ack.accepted, "{epoch:?}: {ack:?}");
        assert_eq!(reason_of(&ack), Reason::UnknownLease, "{epoch:?}");
        assert!(!matches!(*y.stage.borrow(), Stage::Done(_)), "Y answered");
        assert_eq!(disk.rows("done/"), 0, "{epoch:?}");

        // X's lease, with X's action: X's own result, and Y is still open.
        let ack = after
            .report(&node(), result_naming(x_lease, 1))
            .await
            .expect("an ack");
        assert!(ack.accepted, "{epoch:?}: {ack:?}");
        assert!(!matches!(*y.stage.borrow(), Stage::Done(_)), "Y answered");
        assert_eq!(disk.rows("done/"), 1, "{epoch:?}: X's result only");

        // Y's own lease ends Y.
        assert!(accepted(&after, y_lease).await, "{epoch:?}");
        assert!(matches!(ended(&y), Finished::Failed(_)));
        assert_eq!(disk.rows("done/"), 2, "{epoch:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_predecessor_process_s_lease_id_never_names_another_operation_in_memory() {
    a_predecessor_process_s_lease_id_never_names_another_operation(Backing::Fake).await;
}

#[tokio::test(start_paused = true)]
async fn a_predecessor_process_s_lease_id_never_names_another_operation_in_sqlite() {
    a_predecessor_process_s_lease_id_never_names_another_operation(Backing::Sqlite).await;
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

/// Catches: a refusal that logs the same line whatever the reason (an operator reading
/// the log could not tell a stale attempt, which is normal after a re-grant, from a lease
/// the server never granted, which is not), or no line at all. Each reason logs its own.
#[test]
fn each_refusal_logs_its_own_line() {
    let duplicate = logs(async {
        let (_disk, farm, _inbox, lease, _ticket) = started().await;
        assert!(accepted(&farm, lease).await);
        assert!(farm.report(&node(), invalid(lease)).await.is_some());
    });
    assert!(duplicate.contains("result is a copy"), "{duplicate}");

    let stale = logs(async {
        let (_disk, farm, _inbox, lease, _ticket) = started().await;
        let other = WorkerId::new("node-b");
        assert!(farm.report(&other, invalid(lease)).await.is_some());
    });
    assert!(stale.contains("stale attempt"), "{stale}");

    let unknown = logs(async {
        let (_disk, farm, _inbox, lease, _ticket) = started().await;
        let foreign = LeaseId::new(lease.term + 1, lease.seq);
        assert!(farm.report(&node(), invalid(foreign)).await.is_some());
    });
    assert!(unknown.contains("unknown lease"), "{unknown}");

    let mismatch = logs(async {
        let (_disk, farm, _inbox, lease, _ticket) = started().await;
        let other = Digest::new(DigestFunction::Sha256, [9; 32], 9);
        let wrong = worker::Result {
            action_digest: Some(kbf_front::digest_to_proto(&other)),
            ..invalid(lease)
        };
        assert!(farm.report(&node(), wrong).await.is_some());
    });
    assert!(mismatch.contains("names another action"), "{mismatch}");
}

/// Catches: a farm that lets a lease it gave up (the operation holds a newer lease on the
/// same node, so only the sequence tells them apart) past the standing check and leaves
/// the refusal to the scheduler's newest-grant check: the answer is the same STALE, but
/// the farm has already treated the Result as its operation's, here as a memory kill it
/// logs and would count. The refused Result gets its reason and no other line.
#[test]
fn a_given_up_lease_s_result_is_refused_before_it_is_read() {
    let said = logs(async {
        let (disk, farm, _inbox, first, ticket) = started().await;
        let (_, mut inbox) = lose_lease(&farm).await;
        let second = starts(&mut inbox)[0];
        assert_ne!(first, second);

        let killed = worker::Result {
            memory_kill: worker::MemoryKill::OwnLimit as i32,
            ..invalid(first)
        };
        let ack = farm.report(&node(), killed).await.expect("an ack");
        assert!(!ack.accepted);
        assert_eq!(reason_of(&ack), Reason::Stale);
        assert!(!matches!(*ticket.stage.borrow(), Stage::Done(_)));
        assert_eq!(disk.rows("done/"), 0);
        assert!(accepted(&farm, second).await);
    });
    assert!(said.contains("stale attempt"), "{said}");
    assert!(!said.contains("killed for memory"), "{said}");
}
