//! What the daemon does with a `ResultAck`: it forgets the Result on any ack, whatever the
//! reason says, makes no resend decision from `accepted`, and keeps a bounded number of
//! Results for resending.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Event, FakeRuntime};
use kbf_proto::worker::{LeaseId, ResultAckReason};
use support::{Harness, PROMPT};

const LONG: Duration = Duration::from_secs(60);

fn lease(term: u64, seq: u64) -> LeaseId {
    LeaseId { term, seq }
}

/// The reason of the next `ResultAcknowledged` the daemon emits, with its lease and
/// `accepted`.
async fn acknowledged(h: &mut Harness) -> (kbf_types::LeaseId, bool, ResultAckReason) {
    h.event(PROMPT, |e| match e {
        Event::ResultAcknowledged {
            lease,
            accepted,
            reason,
        } => Some((*lease, *accepted, *reason)),
        _ => None,
    })
    .await
    .expect("a ResultAcknowledged event")
    .1
}

/// Catches: a daemon that keeps the Result of a stale attempt for ever (it would resend
/// it on every stream and list the lease in every heartbeat), one that decides to resend
/// from `accepted: false`, and one that does not surface the reason. Every refusing
/// reason is acknowledged with `accepted: false`; after each, the lease is no longer
/// listed and the next stream resends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_result_is_forgotten_and_never_resent() {
    let mut h = Harness::start("ack-refused", Duration::from_millis(50), LONG).await;
    let mut peer = h.welcomed().await;
    let reasons = [
        ResultAckReason::Stale,
        ResultAckReason::UnknownLease,
        ResultAckReason::Mismatch,
        ResultAckReason::Duplicate,
    ];
    for (n, reason) in reasons.into_iter().enumerate() {
        let seq = 1 + n as u64;
        peer.start(1, seq, "action");
        peer.result(PROMPT).await.expect("a Result");
        peer.ack_result_reason(Some((1, seq)), false, reason as i32);
        let (id, accepted, said) = acknowledged(&mut h).await;
        assert_eq!(
            (id, accepted, said),
            (kbf_types::LeaseId::new(1, seq), false, reason)
        );
    }
    // Every lease is forgotten: a heartbeat after the last ack lists none.
    let after = tokio::time::Instant::now();
    assert!(peer.heartbeat_after(after).await.running.is_empty());
    peer.close();

    let mut second = h.welcomed().await;
    assert!(
        second.result(Duration::from_millis(400)).await.is_none(),
        "a refused Result was resent"
    );
}

/// Catches: a daemon that fails on a `ResultAck` with no reason (a server that predates
/// the field) or one it does not know, or keeps the Result for it: both are acknowledged
/// and forgotten, and the event says the reason is unspecified.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ack_without_a_known_reason_forgets_the_result() {
    let mut h = Harness::start("ack-unspecified", Duration::from_millis(50), LONG).await;
    let mut peer = h.welcomed().await;
    for (seq, wire) in [(1, 0), (2, 99)] {
        peer.start(1, seq, "action");
        peer.result(PROMPT).await.expect("a Result");
        peer.ack_result_reason(Some((1, seq)), true, wire);
        let (_, accepted, said) = acknowledged(&mut h).await;
        assert!(accepted);
        assert_eq!(said, ResultAckReason::Unspecified, "wire value {wire}");
    }
    let after = tokio::time::Instant::now();
    assert!(peer.heartbeat_after(after).await.running.is_empty());
}

/// Catches: a daemon whose set of unacknowledged Results grows without bound while a
/// server never acknowledges (a Result is kept for every lease it ever ran). With room
/// for two, the third Result is sent once and not kept: the heartbeat lists the two
/// oldest, and the next stream resends those two, not the third; an ack frees room.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unacknowledged_results_are_bounded_and_the_oldest_stay() {
    let runtime = Arc::new(FakeRuntime::new(Duration::from_millis(50)));
    let mut h = Harness::with_max_unacked("ack-bounded", runtime, LONG, 2).await;
    let mut first = h.welcomed().await;
    for seq in 1..=3 {
        first.start(1, seq, "action");
        // One at a time, so the order the Results were kept in is the order of the leases.
        first.result(PROMPT).await.expect("a Result");
    }
    let after = tokio::time::Instant::now();
    let mut listed = first.heartbeat_after(after).await.running;
    listed.sort_by_key(|l| l.seq);
    assert_eq!(listed, [lease(1, 1), lease(1, 2)]);
    first.close();

    let mut second = h.welcomed().await;
    let mut resent = Vec::new();
    while let Some((_, result)) = second.result(Duration::from_millis(400)).await {
        resent.push(result.lease_id.expect("a lease id"));
    }
    resent.sort_by_key(|l| l.seq);
    assert_eq!(resent, [lease(1, 1), lease(1, 2)]);

    // Room again: an ack frees a slot, and the next Result is kept.
    second.ack_result(Some((1, 1)), true);
    acknowledged(&mut h).await;
    second.start(1, 4, "action");
    second.result(PROMPT).await.expect("a Result");
    let after = tokio::time::Instant::now();
    let mut listed = second.heartbeat_after(after).await.running;
    listed.sort_by_key(|l| l.seq);
    assert_eq!(listed, [lease(1, 2), lease(1, 4)]);
}
