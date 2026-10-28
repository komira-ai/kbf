//! The block of lease sequences a scheduler of a shared state store is given
//! ([`Scheduler::use_seq_block`]): leases are numbered inside it and not past its end.

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, Input, Request, Scheduler};
use kbf_types::{
    ControlRecord, Digest, DigestFunction, Effect, LeaseGrant, LeaseKind, Qos, Resources,
    StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;
const TERM: u64 = 3;

fn request(tag: u8) -> Request {
    Request {
        key: kbf_types::work_key(
            "main",
            Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        ),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

fn feed(s: &mut Scheduler, event: Event) -> Vec<Effect> {
    s.apply(Input::new(kbf_types::FarmTime::default(), event))
}

/// A scheduler of term `TERM` with a worker that has room for every request and `queued`
/// operations waiting.
fn with_queue(queued: u8) -> Scheduler {
    let mut s = Scheduler::new(TERM);
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let up = Event::WorkerUp {
        worker: WorkerId::new("node-a"),
        instance: DaemonInstance::new("daemon"),
        predecessor_stopped: false,
        capacity: Resources::new(64_000, 64 * GIB),
        caps,
    };
    assert!(feed(&mut s, up).is_empty());
    for tag in 0..queued {
        let submit = Event::Submit {
            waiter: WaiterId(u64::from(tag)),
            request: request(tag),
        };
        assert!(feed(&mut s, submit).is_empty());
    }
    s
}

/// The grants one tick proposes, in the order they are numbered.
fn grants(s: &mut Scheduler) -> Vec<LeaseGrant> {
    feed(s, Event::Tick)
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Commit(ControlRecord::Lease(grant)) => Some(grant),
            _ => None,
        })
        .collect()
}

fn seqs(grants: &[LeaseGrant]) -> Vec<u64> {
    grants.iter().map(|grant| grant.lease.seq).collect()
}

/// Asserts that a scheduler which was never given a block places every queued operation
/// and numbers the leases from 0: the limit is for the scheduler of a shared store only.
#[test]
fn a_scheduler_given_no_block_places_without_a_limit() {
    let mut s = with_queue(3);
    assert_eq!(s.next_seq(), 0);
    assert_eq!(s.seqs_left(), u64::MAX);

    let placed = grants(&mut s);

    assert_eq!(seqs(&placed), [0, 1, 2]);
    assert_eq!(s.next_seq(), 3);
}

/// Asserts that a round places no more leases than the block has left, numbers them from
/// the block's start, and leaves the rest of the queue waiting; and that the next block
/// places them, numbered from its own start (the sequences between the blocks are not
/// handed out).
#[test]
fn a_round_places_no_more_leases_than_the_block_has_left() {
    let mut s = with_queue(5);
    s.use_seq_block(10..12);
    assert_eq!(s.next_seq(), 10);
    assert_eq!(s.seqs_left(), 2);

    let first = grants(&mut s);

    assert_eq!(seqs(&first), [10, 11]);
    assert_eq!(s.seqs_left(), 0);
    assert_eq!(s.queued_len(), 3, "the operations past the block wait");
    assert!(grants(&mut s).is_empty(), "a spent block places nothing");

    s.use_seq_block(20..30);
    assert_eq!(s.next_seq(), 20);
    let second = grants(&mut s);

    assert_eq!(seqs(&second), [20, 21, 22]);
    assert_eq!(s.seqs_left(), 7);
    assert_eq!(s.queued_len(), 0);
    assert!(second.iter().all(|grant| grant.lease.term == TERM));
}

/// Asserts that a block that starts behind the next sequence does not move the numbering
/// back: a sequence already handed out is never handed out again.
#[test]
fn a_block_behind_the_next_sequence_does_not_move_it_back() {
    let mut s = with_queue(4);
    s.use_seq_block(0..3);
    assert_eq!(seqs(&grants(&mut s)), [0, 1, 2]);

    s.use_seq_block(1..9);

    assert_eq!(s.next_seq(), 3);
    assert_eq!(s.seqs_left(), 6);
    assert_eq!(seqs(&grants(&mut s)), [3]);
}

/// Asserts that a block that ends before the next sequence leaves no room, rather than a
/// huge one: nothing is placed until a block past the numbering is given.
#[test]
fn a_block_that_ends_behind_the_next_sequence_has_no_room() {
    let mut s = with_queue(3);
    s.use_seq_block(5..7);
    assert_eq!(seqs(&grants(&mut s)), [5, 6]);

    s.use_seq_block(0..4);

    assert_eq!(s.next_seq(), 7);
    assert_eq!(s.seqs_left(), 0);
    assert!(grants(&mut s).is_empty());
    assert_eq!(s.queued_len(), 1);
}
