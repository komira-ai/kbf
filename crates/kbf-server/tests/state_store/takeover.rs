//! A successor farm over the store its predecessor left: the lease epoch is kept, so a
//! `Result` of a lease the predecessor granted (and the daemon still holds) is taken by
//! the successor, and the `Result` of a lease the successor gave up is refused.

use kbf_proto::worker::ResultAckReason as Reason;

use super::*;

/// Catches: a successor whose lease epoch changes on recovery (every lease the daemon
/// holds is of another epoch, so its `Result` is refused and the run is wasted), one that
/// does not rebuild the leases the daemon holds (the resent `Result` is unknown), one
/// that takes the `Result` of a lease it gave up and granted again (two runs write one
/// result), and a refusal that gives no reason. Two farms run over one store; the first is
/// fenced, as a handed-over process is, and the second recovers from the rows.
#[tokio::test(start_paused = true)]
async fn a_successor_takes_the_predecessors_lease_result_and_refuses_a_stale_one() {
    let disk = Disk::new();
    let first = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&first);
    submit(&first, request(1)).expect("submitted");
    submit(&first, request(2)).expect("submitted");
    let [held, undelivered] = starts(&mut inbox)[..] else {
        panic!("two leases expected");
    };
    first.fence();
    drop(inbox);

    let second = disk.farm(EpochPolicy::Keep);
    assert_eq!(
        second.lease_epoch(),
        held.term,
        "the successor's epoch is the predecessor's"
    );
    let (stream, mut inbox) = register(&second);
    // The daemon received the first `Start` only: the other is given up and granted again.
    assert!(second.heartbeat(&node(), stream, 1, vec![held]).await);
    let regranted = starts(&mut inbox);
    assert_eq!(regranted, [LeaseId::new(held.term, 2)]);

    let stale = second
        .report(&node(), invalid(undelivered))
        .await
        .expect("an ack for the given-up lease");
    assert!(!stale.accepted);
    assert!(
        matches!(
            Reason::try_from(stale.reason),
            Ok(Reason::Stale | Reason::UnknownLease)
        ),
        "{stale:?}"
    );

    let resent = second
        .report(&node(), invalid(held))
        .await
        .expect("an ack for the resent result");
    assert!(resent.accepted);
    assert_eq!(Reason::try_from(resent.reason), Ok(Reason::Accepted));
    assert!(accepted(&second, regranted[0]).await);
    assert_eq!(disk.rows("op/"), 0);
}
