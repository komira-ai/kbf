//! The parts of the log no caller reaches through its public API.

use kbf_log::{Format, LogCommand};
use kbf_meta::{Applied, Command, Retention};
use kbf_raft::{
    Entry, LogId, LogIndex, Machine, Message, MessageKind, Payload, ServerId, Snapshot, Term,
    Transport,
};

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

/// An `AllocEpoch` command entry at `index`.
fn alloc_epoch(index: u64) -> Entry {
    let bytes = kbf_log::encode(Format::V1, &LogCommand::Meta(Command::AllocEpoch))
        .expect("an AllocEpoch encodes");
    Entry {
        id: LogId {
            term: Term(1),
            index: LogIndex(index),
        },
        payload: Payload::Command(bytes),
    }
}

/// Catches: answers kept for entries replayed before `keep_answers`, and an answer
/// that stays in the map once taken (`get(..).cloned()` for `remove`), which grows the
/// map by one entry per commit (every tick and every blob) for the server's lifetime.
#[test]
fn the_machine_keeps_an_answer_only_after_keep_answers_and_until_it_is_taken() {
    let mut machine = MetaMachine::new(Retention::default());
    machine.apply(&alloc_epoch(1));
    machine.keep_answers();
    assert_eq!(
        machine.take_answer(LogIndex(1)),
        None,
        "a replayed entry keeps no answer"
    );

    machine.apply(&alloc_epoch(2));
    assert!(
        matches!(machine.take_answer(LogIndex(2)), Some(Applied::Epoch(_))),
        "an entry applied after keep_answers is answered"
    );
    assert_eq!(
        machine.take_answer(LogIndex(2)),
        None,
        "an answer is taken once"
    );
}
