//! When the farm acknowledges a `Result`: only once the state store has taken the row
//! that records it. A daemon forgets a `Result` on its `ResultAck`, so an ack sent for a
//! record the store refused loses the run if the farm then restarts or hands over; a
//! daemon that keeps the `Result` resends it, and the farm answers the copy once.

use super::*;

/// The `ResultAck`s waiting in `inbox`, oldest first.
fn acks(inbox: &mut Inbox) -> Vec<worker::ResultAck> {
    let mut acks = Vec::new();
    while let Ok(message) = inbox.try_recv() {
        if let Some(server_message::Message::ResultAck(ack)) = message.expect("no error").message {
            acks.push(ack);
        }
    }
    acks
}

fn submit_ticket(farm: &TestFarm, tag: u8) -> kbf_front::Ticket {
    farm.submit(Submission {
        request: request(tag),
        call: None,
        described: Described::default(),
        evidence: Default::default(),
        max_wall: None,
    })
    .expect("submitted")
}

fn wire(lease: LeaseId) -> Option<worker::LeaseId> {
    Some(worker::LeaseId {
        term: lease.term,
        seq: lease.seq,
    })
}

/// An OK result of `lease` with no outputs, which the farm takes as a completed run.
fn ok(lease: LeaseId) -> worker::Result {
    worker::Result {
        status: Some(rpc::Status::default()),
        action_result: Some(kbf_proto::reapi::ActionResult::default()),
        ..invalid(lease)
    }
}

/// Catches: a farm that acknowledges a `Result` whose record the store refused (the
/// daemon forgets a result that is not durable), and one that never acknowledges it once
/// the store takes it (the daemon lists the lease in every heartbeat for ever). While the
/// store is down no ack is sent, for the `Result` or for a copy of it; once a heartbeat
/// finds the store back, the caller is answered and the one ack goes out, `accepted`.
/// A failed run here; a completed one below.
#[tokio::test(start_paused = true)]
async fn a_result_is_acknowledged_only_after_the_store_takes_it() {
    held_then_acknowledged(invalid).await;
}

/// The same for a completed run, whose finish is also what the held record carries.
#[tokio::test(start_paused = true)]
async fn an_ok_result_is_acknowledged_only_after_the_store_takes_it() {
    held_then_acknowledged(ok).await;
}

async fn held_then_acknowledged(result: fn(LeaseId) -> worker::Result) {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&farm);
    let ticket = submit_ticket(&farm, 1);
    let lease = starts(&mut inbox)[0];

    disk.store.set_down(true);
    let held = farm.report(&node(), result(lease)).await;
    assert_eq!(held, None, "an ack for a result the store has not taken");
    let copy = farm.report(&node(), result(lease)).await;
    assert_eq!(copy, None, "an ack for a copy of a held result");
    assert!(acks(&mut inbox).is_empty());
    assert!(
        !matches!(*ticket.stage.borrow(), Stage::Done(_)),
        "the caller was answered before the store took the result"
    );

    disk.store.set_down(false);
    assert!(farm.heartbeat(&node(), stream, 1, vec![lease]).await);
    let sent = acks(&mut inbox);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].lease_id, wire(lease));
    assert!(sent[0].accepted);
    assert!(matches!(
        ended(&ticket),
        Finished::Failed(_) | Finished::Ran(_)
    ));
    assert_eq!(disk.rows("done/"), 1);
}

/// Catches: a farm that loses a result the daemon sent just before its stream died, or
/// answers its caller twice: the stream drops while the store holds the record, the
/// daemon (which never saw an ack) registers again and resends, and the store comes
/// back. One record is committed, the caller is answered once, the ack goes to the new
/// stream, and a further copy is acknowledged at once, not accepted.
#[tokio::test(start_paused = true)]
async fn a_result_resent_after_the_stream_dropped_is_recorded_once() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let ticket = submit_ticket(&farm, 1);
    let lease = starts(&mut inbox)[0];

    disk.store.set_down(true);
    assert_eq!(farm.report(&node(), invalid(lease)).await, None);
    drop(inbox);

    let (second, mut inbox) = register(&farm);
    assert_eq!(
        farm.report(&node(), invalid(lease)).await,
        None,
        "an ack for the resend of a held result"
    );
    disk.store.set_down(false);
    assert!(farm.heartbeat(&node(), second, 1, vec![lease]).await);
    let sent = acks(&mut inbox);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].accepted);
    assert!(matches!(ended(&ticket), Finished::Failed(_)));
    assert_eq!(disk.rows("done/"), 1);

    let again = farm
        .report(&node(), invalid(lease))
        .await
        .expect("a copy of a recorded result is acknowledged at once");
    assert!(!again.accepted);
    assert_eq!(again.lease_id, wire(lease));
    assert_eq!(disk.rows("done/"), 1);
}

/// Catches: a farm that hands the ack of a held result to a node that never held the
/// lease. While the store is down, node-a's result is held; node-b, which was not granted
/// the lease, names it too. When the store takes the record the one ack goes to node-a's
/// stream, `accepted`, and node-b is sent none.
#[tokio::test(start_paused = true)]
async fn a_copy_of_a_held_result_from_another_node_is_not_acknowledged() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream_a, mut inbox) = register(&farm);
    let other = WorkerId::new("node-b");
    let (stream_b, mut other_inbox) = register_as(&farm, &other);
    let _ticket = submit_ticket(&farm, 1);
    let mut starts_a = starts(&mut inbox);
    let mut starts_b = starts(&mut other_inbox);
    let (holder, stream, holder_inbox, lease) = match (starts_a.pop(), starts_b.pop()) {
        (Some(lease), None) => (node(), stream_a, &mut inbox, lease),
        (None, Some(lease)) => (other.clone(), stream_b, &mut other_inbox, lease),
        both => panic!("one node is granted the lease: {both:?}"),
    };
    let stranger = if holder == node() { other } else { node() };

    disk.store.set_down(true);
    assert_eq!(farm.report(&holder, invalid(lease)).await, None);
    assert_eq!(farm.report(&stranger, invalid(lease)).await, None);

    disk.store.set_down(false);
    assert!(farm.heartbeat(&holder, stream, 1, vec![lease]).await);
    let sent = acks(holder_inbox);
    assert_eq!(sent.len(), 1, "the holder is acknowledged: {sent:?}");
    assert!(sent[0].accepted);
    let stray = if holder == node() {
        &mut other_inbox
    } else {
        &mut inbox
    };
    assert!(
        acks(stray).is_empty(),
        "a node that never held the lease was acknowledged"
    );
}

/// Catches: a farm that drops a committed record over its cap and still acknowledges the
/// `Result` (the daemon forgets a result nothing holds). With room for one held record,
/// the second result is dropped and the farm logs and counts it: no ack for it, while the
/// first result's ack still goes out when the store takes it.
#[tokio::test(start_paused = true)]
async fn a_result_whose_record_is_dropped_over_the_cap_is_not_acknowledged() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep).with_held_cap(1);
    let (stream, mut inbox) = register(&farm);
    let _first = submit_ticket(&farm, 1);
    let _second = submit_ticket(&farm, 2);
    let leases = starts(&mut inbox);
    assert_eq!(leases.len(), 2);

    disk.store.set_down(true);
    assert_eq!(farm.report(&node(), invalid(leases[0])).await, None);
    assert_eq!(
        farm.report(&node(), invalid(leases[1])).await,
        None,
        "an ack for a record dropped over the cap"
    );
    assert_eq!(farm.dropped_writes(), 1);

    disk.store.set_down(false);
    assert!(farm.heartbeat(&node(), stream, 1, leases.clone()).await);
    let sent = acks(&mut inbox);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].lease_id, wire(leases[0]));
}
