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
