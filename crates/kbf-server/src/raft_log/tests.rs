//! The parts of the log no caller reaches through its public API.

use kbf_meta::Retention;
use kbf_raft::{LogId, Machine, Message, MessageKind, ServerId, Snapshot, Term, Transport};

use super::NoTransport;
use super::machine::MetaMachine;

/// Catches: a transport that does something with a message. A single voter's core
/// never sends, so nothing reaches it; this is what it would do.
#[test]
fn the_single_voter_transport_drops_what_it_is_given() {
    let msg = Message {
        term: Term(1),
        kind: MessageKind::VoteResponse { granted: true },
    };
    NoTransport.send(ServerId(2), msg);
}

/// Catches: a machine that answers a snapshot with some bytes (an empty encoding, say),
/// which would let the host compact away every entry the snapshot claims to hold.
#[test]
#[should_panic(expected = "takes no snapshot")]
fn the_machine_refuses_to_take_a_snapshot() {
    let _ = MetaMachine::new(Retention::default()).snapshot();
}

/// Catches: a machine that restores from a snapshot it cannot read, starting empty.
#[test]
fn the_machine_refuses_to_restore_a_snapshot() {
    let snapshot = Snapshot {
        base: LogId::default(),
        state: Vec::new(),
    };
    let refused = MetaMachine::new(Retention::default()).restore(&snapshot);
    assert!(refused.is_err());
}
