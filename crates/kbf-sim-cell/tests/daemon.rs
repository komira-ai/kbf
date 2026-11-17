//! The fake daemon's rules for which `Start` it takes, driven through its public
//! state-machine interface.

use kbf_sim::{Event, NodeId, NodeInput};
use kbf_sim_cell::{FakeDaemon, Msg, action};
use kbf_types::{FarmTime, LeaseId, OperationId, Resources, StateMachine};

fn daemon() -> FakeDaemon {
    FakeDaemon::new(
        NodeId::from("server"),
        "d-1".to_owned(),
        Resources::new(1_000, 1),
    )
}

fn at(millis: u64, event: Event<Msg>) -> NodeInput<Msg> {
    NodeInput {
        now: FarmTime::from_millis(millis),
        entropy: 1,
        event,
    }
}

fn message(millis: u64, msg: Msg) -> NodeInput<Msg> {
    at(
        millis,
        Event::Message {
            from: NodeId::from("server"),
            msg,
        },
    )
}

fn welcome(stream: u64) -> Msg {
    Msg::Welcome {
        stream,
        epoch: 7,
        interval_ms: 5_000,
    }
}

fn start(seq: u64) -> Msg {
    Msg::Start {
        stream: 1,
        lease: LeaseId::new(1, seq),
        operation: OperationId(seq),
        action: action(seq),
    }
}

fn running(d: &FakeDaemon) -> Vec<LeaseId> {
    d.unfenced_runs(FarmTime::from_millis(10))
        .map(|(lease, _)| lease)
        .collect()
}

/// A `Welcome` for a stream the daemon did not open is ignored, so a `Start` on its
/// stream finds the daemon not welcomed and runs nothing.
#[test]
fn a_welcome_for_another_stream_leaves_the_daemon_unwelcomed() {
    let mut d = daemon();
    d.apply(at(0, Event::Start));
    d.apply(message(1, welcome(9)));
    d.apply(message(2, start(1)));
    assert!(running(&d).is_empty());
}

/// A `Start` for a lease the daemon reported and had acknowledged runs nothing, and
/// neither does one that arrives once contact is lost; a lease already running is not
/// started twice.
#[test]
fn a_start_is_taken_once_while_contact_holds_and_never_for_a_reported_lease() {
    let mut d = daemon();
    d.apply(at(0, Event::Start));
    d.apply(message(1, welcome(1)));
    d.apply(message(2, start(1)));
    d.apply(message(3, start(1)));
    assert_eq!(running(&d), [LeaseId::new(1, 1)]);
    d.apply(at(
        9_000,
        Event::Timer {
            tag: FakeDaemon::POLL,
        },
    ));
    assert!(d.reported().contains(&LeaseId::new(1, 1)));
    d.apply(message(
        9_001,
        Msg::ResultAck {
            lease: LeaseId::new(1, 1),
            accepted: true,
        },
    ));
    d.apply(message(9_002, start(1)));
    assert!(running(&d).is_empty(), "a reported lease ran again");

    let mut d = daemon();
    d.apply(at(0, Event::Start));
    d.apply(message(1, welcome(1)));
    d.apply(message(39_999, start(2)));
    d.apply(message(40_000, start(3)));
    assert_eq!(running(&d), [LeaseId::new(1, 2)], "taken with contact lost");
}
