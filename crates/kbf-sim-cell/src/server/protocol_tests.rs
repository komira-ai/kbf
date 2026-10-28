//! The server's protocol handling, driven message by message: who may report a lease's
//! result, how a failed run is answered, and which messages the server leaves alone.

use kbf_types::{Failure, LeaseId, Outcome, Resources};

use super::tests::{input, server};
use super::*;

/// A daemon "daemon-0" up on stream 1 and a client's `Execute` of action 0: returns
/// the lease the server started.
fn started(s: &mut Server) -> LeaseId {
    s.apply(input(0, Event::Start));
    let daemon = NodeId::from("daemon-0");
    let hello = Msg::Hello {
        stream: 1,
        instance: "daemon-0-1".to_owned(),
        capacity: Resources::new(2_000, 4 << 30),
    };
    s.apply(input(
        10,
        Event::Message {
            from: daemon,
            msg: hello,
        },
    ));
    let execute = Msg::Execute {
        action: crate::action(0),
    };
    s.apply(input(
        20,
        Event::Message {
            from: NodeId::from("client-0"),
            msg: execute,
        },
    ));
    s.take_outputs()
        .into_iter()
        .find_map(|o| match o {
            Output::Send {
                msg: Msg::Start { lease, .. },
                ..
            } => Some(lease),
            _ => None,
        })
        .expect("the server started the action on the daemon")
}

/// What `from` sent back to the server's replies: every message the server sent.
fn sent(s: &mut Server) -> Vec<(NodeId, Msg)> {
    s.take_outputs()
        .into_iter()
        .filter_map(|o| match o {
            Output::Send { to, msg } => Some((to, msg)),
            _ => None,
        })
        .collect()
}

fn report(s: &mut Server, from: &str, lease: LeaseId, outcome: Outcome) -> Vec<(NodeId, Msg)> {
    let msg = Msg::Report {
        stream: 1,
        lease,
        action: crate::action(0),
        outcome,
    };
    s.apply(input(
        30,
        Event::Message {
            from: NodeId::from(from),
            msg,
        },
    ));
    sent(s)
}

/// The messages in `sent` addressed to `who`.
fn to<'a>(sent: &'a [(NodeId, Msg)], who: &str) -> Vec<&'a Msg> {
    let who = NodeId::from(who);
    sent.iter()
        .filter(|(t, _)| *t == who)
        .map(|(_, m)| m)
        .collect()
}

/// Catches: a result accepted from a worker that does not hold the lease (a stale
/// daemon, or one that took another's lease id), which would answer the action on
/// the word of a run the scheduler never gave that worker.
#[test]
fn a_result_from_a_worker_that_does_not_hold_the_lease_is_refused() {
    let mut s = server(Vec::new());
    let lease = started(&mut s);
    let done = Outcome::Completed {
        action_result: crate::output_of(&crate::action(0)).into(),
    };
    let sent = report(&mut s, "daemon-1", lease, done);
    assert_eq!(sent.len(), 1, "{sent:?}");
    let refused = to(&sent, "daemon-1");
    assert_eq!(
        format!("{refused:?}"),
        format!("[ResultAck {{ lease: {lease:?}, accepted: false }}]")
    );
    let sent = report(&mut s, "daemon-0", lease, done);
    let ack = to(&sent, "daemon-0");
    assert_eq!(
        format!("{ack:?}"),
        format!("[ResultAck {{ lease: {lease:?}, accepted: true }}]")
    );
    let answered = to(&sent, "client-0");
    let want = format!(
        "[Answered {{ action: {:?}, cached: false }}]",
        crate::action(0)
    );
    assert_eq!(format!("{answered:?}"), want);
}

/// Catches: the callers of a failed run left unanswered, or answered as a cache hit.
#[test]
fn a_failed_result_answers_its_callers_as_run() {
    let mut s = server(Vec::new());
    let lease = started(&mut s);
    let sent = report(&mut s, "daemon-0", lease, Outcome::Failed(Failure::Invalid));
    let answered = to(&sent, "client-0");
    let want = format!(
        "[Answered {{ action: {:?}, cached: false }}]",
        crate::action(0)
    );
    assert_eq!(format!("{answered:?}"), want);
}

/// Catches: a message meant for a daemon or a client changing the server's farm
/// state or being answered (the server serves only what the protocol sends to it).
#[test]
fn a_message_for_a_daemon_or_a_client_is_ignored() {
    let mut s = server(Vec::new());
    s.apply(input(0, Event::Start));
    let digest = crate::action(3);
    for msg in [
        Msg::Welcome {
            stream: 1,
            epoch: 1,
            interval_ms: 100,
        },
        Msg::Answered {
            action: digest,
            cached: false,
        },
        Msg::Reset { stream: 1 },
    ] {
        let from = NodeId::from("daemon-0");
        s.apply(input(5, Event::Message { from, msg }));
        assert!(sent(&mut s).is_empty());
    }
    let up = s.up.as_ref().expect("the server is up");
    assert!(up.streams.is_empty());
    assert!(up.callers.is_empty());
}
