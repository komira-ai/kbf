//! Tests of the adapter's hooks one by one: on an adapter no core drives where the hook is
//! told no view, and through a core where it is. The scripted sessions of
//! `tests/farm_core_adapter` run the hooks in the order a farm does and compare the text;
//! these are what those scripts do not reach.

use std::time::Duration;

use js_server_core::{Adapter as _, Stage as CoreStage};
use js_store::Ending;
use kbf_front::{Described, Stage};
use kbf_sched::NodePhase;
use kbf_types::{
    ActionKey, Answer, Effect, Failure, FarmTime, LeaseId, OperationId, Outcome, Refusal, WaiterId,
    Waiting, WorkerId,
};
use tonic::Code;

use super::dispatch;
use super::testing::{Flaky, adapter, enrolled, register, request, rig, rig_over, submit};
use crate::action_usage::Subject;
use crate::answer::{Detail, failed, finished_of, refused_as};
use crate::farm::durable::named;
use crate::test_log::capture;

const RETENTION: Duration = Duration::from_secs(30);

fn refusal(waiters: &[u64]) -> Refusal {
    Refusal {
        operation: OperationId(0),
        waiters: waiters.iter().map(|id| WaiterId(*id)).collect(),
        reason: "no node can run it".to_owned(),
    }
}

fn answer(lease: LeaseId, waiters: &[u64]) -> Answer {
    Answer {
        operation: OperationId(0),
        lease,
        waiters: waiters.iter().map(|id| WaiterId(*id)).collect(),
        outcome: Outcome::Failed(Failure::Infra),
        memory_runs: Vec::new(),
        farm_runs: Vec::new(),
    }
}

/// A `done/` row: a failed result with `message`.
fn done_row(message: &str) -> Vec<u8> {
    failed(Code::Internal, message).to_row()
}

/// Asserts that a worker's stream is open only while it is the newest the core numbered for
/// the worker and the response side of its channel lives: the core asks this to tell a
/// daemon that restarted from one that is still connected, and a replaced stream, a closed
/// one and a stranger must each read as not open.
#[tokio::test(start_paused = true)]
async fn a_stream_is_open_only_while_it_is_the_newest_and_its_receiver_lives() {
    let rig = rig();
    let worker = WorkerId::new("node-a");
    let first = register(&rig, "node-a", 0);
    let second = register(&rig, "node-a", 0);
    let open = |worker: &WorkerId, stream| rig.core.with(|_, a| a.stream_open(worker, stream));

    assert!(!open(&worker, first.stream), "replaced by the second");
    assert!(open(&worker, second.stream));
    assert!(
        !open(&WorkerId::new("node-b"), second.stream),
        "not registered"
    );
    drop(second.inbox);
    assert!(!open(&worker, second.stream), "its response side is gone");
}

/// Asserts that a quarantine raised and a quarantine cleared are each logged once as an
/// attention item naming the node, and that a phase that changes neither logs nothing.
#[test]
fn a_quarantine_raised_and_one_cleared_are_logged_as_attention_items() {
    let log = capture();
    let mut adapter = adapter();
    let worker = WorkerId::new("node-a");
    let (serving, quarantined) = (
        NodePhase::Serving,
        NodePhase::Quarantined("disk full".to_owned()),
    );

    adapter.phased(&worker, &serving, &quarantined);
    adapter.phased(&worker, &quarantined, &serving);
    adapter.phased(&worker, &serving, &serving);

    let text = log.text();
    assert!(
        text.contains("node node-a: quarantined: disk full"),
        "{text}"
    );
    assert!(
        text.contains("node node-a: no longer quarantined"),
        "{text}"
    );
    assert_eq!(text.matches("node node-a:").count(), 2, "{text}");
}

/// Asserts that a `Cancel` goes to the worker's newest stream and to no other worker: the
/// stream that has the lease listed is the one told to stop it.
#[tokio::test(start_paused = true)]
async fn a_cancel_goes_to_the_stream_of_the_worker_it_names() {
    let rig = rig();
    let mut a = register(&rig, "node-a", 0);
    let mut b = register(&rig, "node-b", 0);
    a.sent();
    b.sent();
    let lease = LeaseId::new(7, 99);

    rig.core
        .with(|view, adapter| adapter.cancel(view, &WorkerId::new("node-a"), lease));
    rig.core
        .with(|view, adapter| adapter.cancel(view, &WorkerId::new("node-c"), lease));

    let to_a = a.sent();
    assert!(
        matches!(&to_a[..], [kbf_proto::worker::server_message::Message::Cancel(c)]
            if c.lease_id.as_ref().is_some_and(|l| (l.term, l.seq) == (7, 99))),
        "{to_a:?}"
    );
    assert!(b.sent().is_empty(), "node-b was not named");
}

/// Asserts that an operation the store holds becomes a caller of its own with the action,
/// kind and `action usage:` subject the store's rows name, so that its `Start` names the
/// action the store holds and a client that asks by name is authorized against it.
#[tokio::test(start_paused = true)]
async fn an_operation_the_store_holds_becomes_a_caller_that_names_its_action() {
    let store = Flaky::new();
    let first = rig_over(&store, kbf_front::Recorder::off());
    let ticket = submit(&first, 3, 0).expect("queued");
    assert_eq!(ticket.name, "operations/0");
    drop(first);

    let second = rig_over(&store, kbf_front::Recorder::off());

    second.core.with(|_, adapter| {
        let waiters: Vec<_> = adapter.machine().waiter_ids().collect();
        assert_eq!(waiters.len(), 1, "one operation, one caller of its own");
        let waiter = adapter.machine().waiter(waiters[0]).expect("a caller");
        let key = ActionKey::try_from(&request(3).key).expect("an action's key");
        assert_eq!(waiter.key, key);
        let subject = Subject::of(Described::default()).0;
        assert_eq!(waiter.usage, subject);
    });
}

/// Asserts that an operation whose request names no action is given no caller, and that one
/// whose request does is given one. A caller with a guessed action would have a `Start` sent
/// for an action the operation never named.
#[test]
fn a_request_whose_key_is_not_an_actions_gets_no_caller() {
    let store = Flaky::new();
    let rig = rig_over(&store, kbf_front::Recorder::off());
    let mut foreign = request(1);
    foreign.key.dedup = "not a digest".to_owned();
    let log = capture();

    rig.core.with(|_, adapter| {
        adapter.adopted(OperationId(7), WaiterId(70), None, &foreign, None, None);
        assert_eq!(adapter.machine().waiter_ids().count(), 0, "no caller");
        adapter.adopted(OperationId(8), WaiterId(80), None, &request(2), None, None);
        assert_eq!(adapter.machine().waiter_ids().count(), 1, "a caller");
    });
    let text = log.text();
    assert_eq!(text.matches("no readable request").count(), 1, "{text}");
}

/// Asserts that a parcel the store kept with an operation is its subject when it reads and
/// the empty shape when there is none or it does not: an operation written by an older build
/// is logged by the `action usage:` line of an action that said nothing.
#[tokio::test(start_paused = true)]
async fn a_parcel_that_is_not_a_subject_leaves_the_empty_shape() {
    let rig = rig();
    submit(&rig, 3, 0).expect("queued");
    let empty = Subject::of(Described::default()).0;
    let shaped = Subject::of(Described {
        arguments: vec!["/bin/run".to_owned(), "x".to_owned()],
        outputs: vec!["out/x".to_owned()],
        label: Default::default(),
    })
    .0;
    assert_ne!(shaped, empty, "the two subjects differ");
    let parcel = crate::farm::durable::subject::encode(&shaped);

    rig.core.with(|_, adapter| {
        let asked = request(3);
        adapter.adopted(
            OperationId(0),
            WaiterId(10),
            Some(&parcel),
            &asked,
            None,
            None,
        );
        adapter.adopted(
            OperationId(0),
            WaiterId(11),
            Some(b"not a subject"),
            &asked,
            None,
            None,
        );
        adapter.adopted(OperationId(0), WaiterId(12), None, &asked, None, None);
        let usage = |id| {
            adapter
                .machine()
                .waiter(WaiterId(id))
                .map(|w| w.usage.clone())
        };
        assert_eq!(usage(10), Some(shaped));
        assert_eq!(usage(11), Some(empty.clone()));
        assert_eq!(usage(12), Some(empty));
    });
}

/// Asserts that an operation's ending is what its callers are told, as the `done/` row's
/// bytes, with the action of its first caller as the `doneof/` row's: the store keeps the
/// two as long as each other, and a client that asks for a finished operation by name is
/// authorized against the second.
#[test]
fn an_ending_is_what_the_callers_are_told_and_names_the_action_of_the_first_caller() {
    let mut adapter = adapter();
    let _stage = enrolled(&mut adapter, 0, 0, 1);
    let refused = refusal(&[0]);

    let ending = adapter.ending(&Effect::Refuse(refused.clone()), RETENTION);

    let key = ActionKey::try_from(&request(1).key).expect("an action's key");
    let want =
        Ending::new(refused_as(&refused).to_row(), RETENTION).with_parcel(named::encode(&key));
    assert_eq!(ending, Some(want));
}

/// Asserts that an ending of callers the adapter does not know names no action: the store
/// then has no `doneof/` row for the operation, as for a result an older build wrote.
#[test]
fn an_ending_of_callers_the_adapter_does_not_know_names_no_action() {
    let adapter = adapter();
    let refused = refusal(&[9]);

    let ending = adapter.ending(&Effect::Refuse(refused.clone()), RETENTION);

    let want = Ending::new(refused_as(&refused).to_row(), RETENTION);
    assert_eq!(ending, Some(want));
}

/// Asserts that an effect that finishes nothing has no ending, so the core writes nothing
/// for it: the `Waiting` of an operation is shown to its callers and is no row.
#[test]
fn an_effect_that_finishes_nothing_has_no_ending() {
    let adapter = adapter();
    let waiting = Waiting {
        operation: OperationId(0),
        reason: None,
    };

    assert_eq!(adapter.ending(&Effect::Waiting(waiting), RETENTION), None);
}

/// Asserts that an answer is ended with what its report made of it: the detail parked for
/// its lease decides the bytes of the `done/` row, an answer with no detail is the fixed
/// failure of its outcome, and what is parked for a lease that no answer waits for any more
/// is dropped, so that the next answer of that lease is not made from it.
#[test]
fn an_answer_is_ended_with_the_detail_parked_for_its_lease() {
    let mut adapter = adapter();
    let (kept, dropped) = (LeaseId::new(7, 1), LeaseId::new(7, 2));
    adapter.park(kept, Detail::Invalid("bad input".to_owned()));
    adapter.park(dropped, Detail::Invalid("bad input".to_owned()));
    let row_of = |adapter: &super::KbfAdapter, lease| {
        let effect = Effect::Answer(answer(lease, &[]));
        adapter
            .ending(&effect, RETENTION)
            .map(|ending| ending.done().to_vec())
    };
    let invalid = finished_of(
        &answer(kept, &[]),
        Some(&Detail::Invalid("bad input".to_owned())),
    );
    let unrun = finished_of(&answer(kept, &[]), None);
    assert_ne!(invalid, unrun);

    assert_eq!(row_of(&adapter, kept), Some(invalid.to_row()));
    adapter.retain_parcels(&|lease| lease == kept);
    assert_eq!(row_of(&adapter, kept), Some(invalid.to_row()));
    assert_eq!(row_of(&adapter, dropped), Some(unrun.to_row()), "dropped");
    adapter.forget_parcel(kept);
    assert_eq!(row_of(&adapter, kept), Some(unrun.to_row()), "forgotten");
}

/// Asserts that the callers still waiting are those whose channel has a receiver and is not
/// done, in the order asked: a caller that gave up, one that was answered and one the
/// adapter never heard of are not.
#[test]
fn the_callers_still_waiting_are_those_listening_and_not_answered() {
    let mut adapter = adapter();
    let _live = enrolled(&mut adapter, 0, 0, 1);
    let gone = enrolled(&mut adapter, 1, 0, 1);
    drop(gone);
    let _done = enrolled(&mut adapter, 2, 0, 1);
    let ids = [0, 1, 2, 9].map(WaiterId);
    adapter.show_callers(&[WaiterId(2)], &CoreStage::Done(done_row("answered")));

    assert_eq!(adapter.waiting_among(&ids), [WaiterId(0)]);
}

/// Asserts that the stage the core shows for an operation reaches the callers the scheduler
/// holds for it as the farm's own stage: waiting for a worker, waiting with the streams
/// kept.
#[tokio::test(start_paused = true)]
async fn the_stage_the_core_shows_reaches_the_callers_of_the_operation() {
    let rig = rig();
    let mut ticket = submit(&rig, 1, 0).expect("queued");
    let operation = OperationId(0);
    let show = |stage: CoreStage| {
        rig.core
            .with(|view, adapter| adapter.show(view, operation, &stage));
    };

    show(CoreStage::WaitingKept("busy".to_owned()));
    assert_eq!(
        *ticket.stage.borrow_and_update(),
        Stage::WaitingKept("busy".to_owned())
    );
    show(CoreStage::Executing);
    assert_eq!(*ticket.stage.borrow(), Stage::Executing);
}

/// Asserts that callers are shown a stage once: a caller the adapter does not know is
/// skipped, and a stage already shown wakes nobody.
#[test]
fn callers_are_shown_a_stage_once() {
    let mut adapter = adapter();
    let mut stage = enrolled(&mut adapter, 0, 0, 1);
    let waiting = CoreStage::Waiting("busy".to_owned());

    adapter.show_callers(&[WaiterId(0), WaiterId(7)], &waiting);
    assert_eq!(
        *stage.borrow_and_update(),
        Stage::Waiting("busy".to_owned())
    );
    adapter.show_callers(&[WaiterId(0)], &waiting);
    assert!(
        !stage.has_changed().expect("open"),
        "shown again: nobody woken"
    );
    adapter.show_callers(&[WaiterId(0)], &CoreStage::Done(done_row("ended")));
    assert!(matches!(&*stage.borrow(), Stage::Done(_)));
}

/// What an unreadable `done/` row is shown as: UNAVAILABLE, which clients retry.
fn unreadable_result_stage() -> Stage {
    let message = "the stored result of this operation cannot be read";
    Stage::Done(failed(Code::Unavailable, message))
}

/// How many times the log says a stored result cannot be read.
fn count_of(log: &crate::test_log::Capture) -> usize {
    log.text().matches("a stored result cannot be read").count()
}

/// Asserts that a `done/` row the adapter cannot decode ends the callers with UNAVAILABLE
/// ("the stored result of this operation cannot be read"), which clients retry, instead of
/// leaving them waiting: the callers a frontend follows (`show_callers`, then `followed`
/// for the invocation record, which logs nothing more), and the watch that follows a name.
/// A lookup by name is answered not found instead, so that the client sends a fresh
/// Execute call and does not retry the name. Each look at the row logs it at error once and
/// counts it, so two lookups by name are two logs; a readable row is not counted. The paused
/// clock makes a hang a failed test: a stage that is not done by the time the bound passes
/// fails it.
#[tokio::test(start_paused = true)]
async fn an_unreadable_done_row_ends_the_caller_with_a_retryable_error_and_does_not_hang() {
    let bound = Duration::from_secs(60);
    let done = |mut stage: tokio::sync::watch::Receiver<Stage>| async move {
        let reached = tokio::time::timeout(bound, stage.wait_for(|s| matches!(s, Stage::Done(_))));
        let shown = reached.await.expect("the caller was ended").expect("open");
        shown.clone()
    };
    let garbage = b"garbage".to_vec();

    // Follow: a caller of an operation another frontend ran.
    let log = capture();
    let mut adapter = adapter();
    let stage = enrolled(&mut adapter, 0, 0, 1);
    adapter.show_callers(&[WaiterId(0)], &CoreStage::Done(garbage.clone()));
    assert_eq!(done(stage).await, unreadable_result_stage());
    assert_eq!(adapter.unreadable_results(), 1);
    assert_eq!(count_of(&log), 1);

    // Name lookup, from the core: a finished operation whose `done/` row was overwritten.
    let store = Flaky::new();
    let first = rig_over(&store, kbf_front::Recorder::off());
    let mut node = register(&first, "node-a", 0);
    let finished = submit(&first, 1, 0).expect("queued");
    let lease = super::testing::started(&node.sent()).expect("started");
    let ack = dispatch::report(
        &first.core,
        &first.backend,
        &super::testing::wall(),
        &|| FarmTime::from_millis(0),
        &WorkerId::new("node-a"),
        super::testing::ran(lease, 0),
    )
    .await;
    assert!(ack.expect("admitted").is_some());
    let key = js_store::keys::done(OperationId(0));
    super::testing::overwrite(store.plain(), &key, garbage.clone());
    let second = rig_over(&store, kbf_front::Recorder::off());
    drop(log);
    let log = capture();
    // A client asking by name is told not found, so that it sends a fresh Execute call: at
    // each look the row is logged at error once and counted.
    assert!(dispatch::wait(&second.core, &finished.name).is_none());
    assert_eq!(second.core.with(|_, a| a.unreadable_results()), 1);
    assert_eq!(count_of(&log), 1);
    assert!(dispatch::wait(&second.core, &finished.name).is_none());
    assert_eq!(second.core.with(|_, a| a.unreadable_results()), 2);
    assert_eq!(count_of(&log), 2, "per look, not per operation");

    // The watch that follows a name shows the row as an error to the callers it holds.
    drop(log);
    let log = capture();
    let mut adapter = super::testing::adapter();
    let watched = adapter.watch_operation(OperationId(3), &Stage::Executing);
    adapter.show_watched(OperationId(3), &CoreStage::Done(garbage.clone()));
    assert_eq!(done(watched).await, unreadable_result_stage());
    assert!(adapter.watched().is_empty(), "its result has been shown");
    assert_eq!(adapter.unreadable_results(), 1);
    assert_eq!(count_of(&log), 1);

    // Followed: the invocation record ends as the caller was told.
    let (recorder, recording) = kbf_front::Recorder::new(1 << 20, 64);
    tokio::spawn(recording.run());
    let rig = rig_over(&Flaky::new(), recorder.clone());
    let ticket =
        super::testing::submit_recorded(&rig, &recorder, request(1), "build-1").expect("queued");
    drop(ticket);
    drop(log);
    let log = capture();
    rig.core.with(|_, adapter| {
        assert!(adapter.recording.is_recorded(OperationId(0)));
        adapter.followed(OperationId(0), &garbage);
        assert!(!adapter.recording.is_recorded(OperationId(0)), "told");
        assert_eq!(adapter.unreadable_results(), 0, "followed counts nothing");
    });
    assert_eq!(count_of(&log), 0, "followed does not log the row again");
    recorder.caught_up().await;
    let key = kbf_invocation::RecordKey::owner(kbf_invocation::InvocationId::new("build-1"));
    let action = ActionKey::try_from(&request(1).key)
        .expect("an action")
        .action;
    let outcome = recorder
        .get(&key)
        .expect("a record")
        .row(&action)
        .expect("a row")
        .outcome
        .clone();
    let want = kbf_invocation::Outcome::Failed {
        code: Code::Unavailable as i32,
        message: "the stored result of this operation cannot be read".to_owned(),
    };
    assert_eq!(outcome, Some(want));

    // A readable row is neither logged nor counted.
    let mut adapter = super::testing::adapter();
    let _stage = enrolled(&mut adapter, 0, 0, 1);
    adapter.show_callers(&[WaiterId(0)], &CoreStage::Done(done_row("ended")));
    assert_eq!(adapter.unreadable_results(), 0);
}

/// Asserts that an operation watched by name is shown what the store says until it is done,
/// and is then no longer listed; one nobody holds a ticket on any more is dropped from the
/// list; and a stage for an operation nobody watches is shown to nobody.
#[test]
fn an_operation_watched_by_name_is_listed_until_it_is_done_or_nobody_holds_it() {
    let mut adapter = adapter();
    let (a, b) = (OperationId(3), OperationId(4));
    let mut ticket = adapter.watch_operation(a, &Stage::Queued);
    let abandoned = adapter.watch_operation(b, &Stage::Queued);
    assert_eq!(adapter.watched(), [a, b]);
    drop(abandoned);
    assert_eq!(adapter.watched(), [a], "nobody holds a ticket on the other");

    adapter.show_watched(a, &CoreStage::Executing);
    assert_eq!(*ticket.borrow_and_update(), Stage::Executing);
    adapter.show_watched(OperationId(99), &CoreStage::Executing);
    adapter.show_watched(a, &CoreStage::Done(done_row("ended")));
    assert!(matches!(&*ticket.borrow(), Stage::Done(_)));
    assert!(adapter.watched().is_empty(), "its result has been shown");
    let again = adapter.watch_operation(a, &Stage::Executing);
    assert!(
        matches!(&*again.borrow(), Stage::Executing),
        "shown to a new ticket"
    );
}

/// Asserts that the callers of an operation another frontend ran are kept until the core
/// says it holds the operation no longer, and then forgotten with their channels.
#[test]
fn callers_kept_for_a_finished_operation_are_forgotten_when_the_core_drops_it() {
    let mut adapter = adapter();
    let _stage = enrolled(&mut adapter, 0, 5, 1);

    adapter.keep_finished(OperationId(5), vec![WaiterId(0)]);
    adapter.forget_dropped(&|_| true);
    assert!(
        adapter.machine().waiter(WaiterId(0)).is_some(),
        "still held"
    );
    adapter.forget_dropped(&|_| false);
    assert!(adapter.machine().waiter(WaiterId(0)).is_none(), "dropped");
}

/// Asserts that the end of an operation another frontend ran is told to the recorder once,
/// as the callers' answer, with no lease.
#[tokio::test(start_paused = true)]
async fn the_end_of_an_operation_another_frontend_ran_is_recorded_once() {
    let (recorder, recording) = kbf_front::Recorder::new(1 << 20, 64);
    tokio::spawn(recording.run());
    let rig = rig_over(&Flaky::new(), recorder.clone());
    let ticket =
        super::testing::submit_recorded(&rig, &recorder, request(1), "build-1").expect("queued");
    let operation = OperationId(0);
    let key = kbf_invocation::RecordKey::owner(kbf_invocation::InvocationId::new("build-1"));
    let action = ActionKey::try_from(&request(1).key)
        .expect("an action")
        .action;
    drop(ticket);

    rig.core.with(|_, adapter| {
        assert!(adapter.recording.is_recorded(operation));
        adapter.followed(operation, &done_row("ended by the other frontend"));
        assert!(!adapter.recording.is_recorded(operation), "told");
        adapter.followed(operation, &done_row("again"));
    });

    recorder.caught_up().await;
    let record = recorder.get(&key).expect("a record");
    let outcome = record.row(&action).expect("a row").outcome.clone();
    let want = kbf_invocation::Outcome::Failed {
        code: Code::Internal as i32,
        message: "ended by the other frontend".to_owned(),
    };
    assert_eq!(outcome, Some(want));
}
