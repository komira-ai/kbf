//! The drain sequence over a farm on a fake state store: the order of its steps, what each
//! one does and when it ends. Time is paused, so a wait of minutes costs nothing.

use futures::FutureExt as _;
use futures::future::{AbortHandle, AbortRegistration};
use kbf_front::{Backend, Cache, Described, Dispatch, Submission};
use kbf_proto::worker::{ServerMessage, server_message};
use kbf_sched::{DaemonInstance, Request};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Cond, ManualClock, Store, Txn};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources, WorkerId};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tonic::{Code, Status};

use super::*;
use crate::drain::Phase;
use crate::farm::{EpochPolicy, Farm, Reported};
use crate::health::Readiness;
use crate::state_store::StateStore;

type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, Status>>;

/// The two waits, in seconds.
fn timing(lb_wait: u64, deadline: u64) -> DrainTiming {
    DrainTiming {
        lb_wait: Duration::from_secs(lb_wait),
        deadline: Duration::from_secs(deadline),
    }
}

/// A serving frontend's parts, as the drain stands them down: a farm over a state store, a
/// drain with the listeners' stops, and the sequence over them.
struct Rig {
    backend: Backend,
    farm: Arc<Farm>,
    drain: Arc<Drain>,
    readiness: Arc<Readiness>,
    closing: kbf_front::Closing,
    /// Completes when the drain sends the REAPI listener GOAWAY.
    goaway: oneshot::Receiver<()>,
    worker: Stop,
    api: Stop,
    stand: Stand,
    sequence: Sequence,
    /// Kept so the sweeper is not aborted before it is told to be.
    _sweeping: AbortRegistration,
}

impl Rig {
    fn new(timing: DrainTiming) -> Self {
        let store = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
        // A store opened for a writer holds a generation.
        let value = 7_u64.to_be_bytes().to_vec();
        let generation = Txn::new().put(kbf_state::GENERATION, value, Cond::Any);
        store.txn(generation).expect("store").expect("put");
        let backend = Backend::from(Arc::new(Cache::memory()));
        let farm = Farm::new(
            backend.clone(),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        )
        .with_state(Some(StateStore {
            store: Arc::new(store),
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store");
        let farm = Arc::new(farm);
        let (closer, closing) = kbf_front::closing();
        let readiness = Arc::new(Readiness::default());
        let (drain, goaway) = Drain::new(closer, Arc::clone(&readiness));
        let drain = Arc::new(drain);
        let (sweeper, sweeping) = AbortHandle::new_pair();
        let (worker, api) = (Stop::new(), Stop::new());
        let stand = Stand {
            drain: Arc::clone(&drain),
            farm: Arc::clone(&farm),
            worker: worker.clone(),
            sweeper,
            wait: Duration::from_secs(5),
        };
        let sequence = Sequence::new(stand.clone(), api.clone(), timing);
        Self {
            backend,
            farm,
            drain,
            readiness,
            closing,
            goaway,
            worker,
            api,
            stand,
            sequence,
            _sweeping: sweeping,
        }
    }

    /// The retire mode of this frontend, which holds the sequence, as the frontend keeps it.
    fn retire(&self) -> OnceLock<Retire> {
        let retire = OnceLock::new();
        let _ = retire.set(Retire::new(self.stand.clone(), self.sequence.clone()));
        retire
    }
}

/// A request to run.
fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

/// Queues `request` on `farm`.
fn submit(farm: &Farm, request: Request) -> Result<kbf_front::Ticket, Status> {
    farm.submit(Submission {
        request,
        call: None,
        described: Described::default(),
        evidence: Default::default(),
    })
}

/// Registers the node `node-a` on the rig's farm, as a daemon's first stream does.
fn register(rig: &Rig) -> Inbox {
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, inbox) = mpsc::unbounded_channel();
    rig.farm.register(
        &WorkerId::new("node-a"),
        DaemonInstance::new("daemon"),
        DaemonInstance::new(""),
        Reported::serving(Resources::new(4_000, 8 << 30), caps),
        outbound,
        Default::default(),
    );
    inbox
}

/// How many `Start`s are waiting in `inbox`.
fn starts(inbox: &mut Inbox) -> usize {
    let messages = std::iter::from_fn(|| inbox.try_recv().ok());
    let sent = |message: &Result<ServerMessage, Status>| {
        let message = message.as_ref().expect("no error").message.as_ref();
        matches!(message, Some(server_message::Message::Start(_)))
    };
    messages.filter(sent).count()
}

/// Asserts that `begin` says it was the first (a second call is not) and answers not ready
/// before it returns; that in the wait the frontend still places work on its node, refuses
/// nothing and sends no GOAWAY; that when the wait is over new Execute calls are refused,
/// the REAPI, worker and operator listeners are sent GOAWAY, and its node is placed nothing
/// more; and that an open execution keeps the drain going until it is gone, when the drain
/// ends, closing.
#[tokio::test(start_paused = true)]
async fn the_wait_serves_everything_and_the_end_of_it_refuses_and_sends_goaway() {
    let mut rig = Rig::new(timing(10, 900));
    let mut inbox = register(&rig);
    let open = rig.backend.metrics().execute.open_executions.raise();

    assert!(rig.sequence.begin());
    assert!(!rig.sequence.begin(), "a second begin was a first");
    assert_eq!(rig.drain.phase(), Phase::LbWait);
    assert!(rig.readiness.is_stopping(), "still ready after begin");
    tokio::time::sleep(Duration::from_millis(9_999)).await;
    assert_eq!(rig.drain.phase(), Phase::LbWait);
    assert!(!rig.closing.draining(), "refused calls in the wait");
    assert!(rig.goaway.try_recv().is_err(), "GOAWAY in the wait");
    assert!(rig.worker.fired().now_or_never().is_none());
    assert!(rig.api.fired().now_or_never().is_none());
    let _served = submit(&rig.farm, request(1)).expect("queued in the wait");
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 1, "the wait did not place the work");

    tokio::time::sleep(Duration::from_millis(2)).await;
    assert_eq!(rig.drain.phase(), Phase::Draining);
    assert!(rig.closing.draining(), "new calls taken after the wait");
    assert_eq!(rig.goaway.try_recv(), Ok(()));
    assert!(rig.worker.fired().now_or_never().is_some());
    assert!(rig.api.fired().now_or_never().is_some());
    let _refused = submit(&rig.farm, request(2)).expect("queued");
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 0, "the end of the wait placed work");

    assert!(rig.drain.sequence_finished().now_or_never().is_none());
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(rig.drain.phase(), Phase::Draining, "ended while open");
    drop(open);
    let ended = tokio::time::timeout(Duration::from_secs(10), rig.drain.sequence_finished());
    ended
        .await
        .expect("the drain ends once the execution is gone");
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Asserts that with an execution that never ends, the drain waits the whole deadline and
/// the clients' grace and no more; that the worker's stream is then ended UNAVAILABLE, so
/// its daemon dials another frontend; and that the farm is fenced and takes no work.
#[tokio::test(start_paused = true)]
async fn at_the_deadline_the_streams_that_are_left_are_ended() {
    let rig = Rig::new(timing(0, 30));
    let mut inbox = register(&rig);
    let _open = rig.backend.metrics().execute.open_executions.raise();
    let began = Instant::now();

    assert!(rig.sequence.begin());
    rig.drain.sequence_finished().await;

    let waited = began.elapsed();
    let least = Duration::from_secs(30) + CLOSE_GRACE;
    assert!(waited >= least, "ended early: {waited:?}");
    assert!(waited < Duration::from_secs(40), "ended late: {waited:?}");
    assert_eq!(rig.drain.phase(), Phase::Closing);
    let ended: Vec<Code> = std::iter::from_fn(|| inbox.try_recv().ok())
        .filter_map(|message| message.err())
        .map(|status| status.code())
        .collect();
    assert_eq!(ended, [Code::Unavailable]);
    assert!(submit(&rig.farm, request(1)).is_err(), "still takes work");
}

/// Asserts that a second stop signal (`Drain::close`) skips the waits: with a wait of ten
/// minutes and a deadline of fifteen, and an execution that never ends, the drain is over at
/// once.
#[tokio::test(start_paused = true)]
async fn a_second_signal_skips_both_waits() {
    let rig = Rig::new(timing(600, 900));
    let _open = rig.backend.metrics().execute.open_executions.raise();
    let began = Instant::now();

    assert!(rig.sequence.begin());
    rig.drain.close();
    rig.drain.sequence_finished().await;

    // Only the clients' grace, for the execution that was open, is waited.
    let most = CLOSE_GRACE + Duration::from_secs(1);
    assert!(began.elapsed() < most, "{:?}", began.elapsed());
}

/// Asserts that a drain with no execution open goes straight through: with a wait of zero
/// and no execution, the drain is over without waiting for a deadline of fifteen minutes.
#[tokio::test(start_paused = true)]
async fn a_drain_with_nothing_open_does_not_wait_for_the_deadline() {
    let rig = Rig::new(timing(0, 900));
    let began = Instant::now();

    assert!(rig.sequence.begin());
    rig.drain.sequence_finished().await;

    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Asserts that at a stop signal a frontend with no state store does the plain drain, at
/// once, with no sequence.
#[tokio::test(start_paused = true)]
async fn a_stop_without_a_state_store_drains_at_once() {
    let rig = Rig::new(timing(10, 900));
    let none = OnceLock::new();

    stopped(&rig.drain, &none, Duration::from_secs(1)).await;

    assert!(!rig.drain.is_sequenced());
    assert_eq!(rig.drain.phase(), Phase::Closing);
    assert!(rig.closing.draining());
}

/// Asserts that at a stop signal a frontend with a state store begins the sequence (a second
/// beginning, as the endpoint's, is not a first), waits out the sequence and returns when it
/// is over.
#[tokio::test(start_paused = true)]
async fn a_stop_over_a_state_store_runs_the_sequence() {
    let rig = Rig::new(timing(3, 900));
    let retire = rig.retire();
    let began = Instant::now();

    stopped(&rig.drain, &retire, Duration::MAX).await;

    assert!(rig.drain.is_sequenced());
    assert!(
        began.elapsed() >= Duration::from_secs(3),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(rig.drain.phase(), Phase::Closing);
    assert!(rig.readiness.is_stopping());
    let retire = retire.get().expect("set");
    assert!(!retire.drain(), "the endpoint began it a second time");
}

/// Asserts that the stop timeout (`--shutdown-timeout-secs`) bounds a sequence that a stop
/// signal began: an execution that never ends is closed when it runs out, long before the
/// deadline.
#[tokio::test(start_paused = true)]
async fn the_stop_timeout_closes_the_streams_of_a_sequence() {
    let rig = Rig::new(timing(0, 900));
    let retire = rig.retire();
    let _open = rig.backend.metrics().execute.open_executions.raise();
    let began = Instant::now();

    stopped(&rig.drain, &retire, Duration::from_secs(5)).await;

    let waited = began.elapsed();
    assert!(waited >= Duration::from_secs(5), "ended early: {waited:?}");
    assert!(waited < Duration::from_secs(60), "ended late: {waited:?}");
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Asserts that a task of the sequence that panics still ends it: the drain is not over
/// while the task holds its guard, and is over once the task is gone.
#[tokio::test(start_paused = true)]
async fn a_sequence_task_that_panics_still_ends_the_sequence() {
    let rig = Rig::new(timing(0, 900));
    let ends = Ends(Arc::clone(&rig.drain));
    assert!(rig.drain.sequence_finished().now_or_never().is_none());

    let task = tokio::spawn(async move {
        let _ends = ends;
        panic!("a step of the sequence failed");
    });

    assert!(task.await.is_err(), "the task did not panic");
    rig.drain.sequence_finished().await;
}

/// Asserts that the defaults are the ones the flags document: ten seconds of wait and
/// fifteen minutes of deadline.
#[test]
fn the_defaults_are_ten_seconds_and_fifteen_minutes() {
    let timing = DrainTiming::default();
    assert_eq!(timing.lb_wait, Duration::from_secs(10));
    assert_eq!(timing.deadline, Duration::from_secs(900));
}

/// What begins the drain in the tests below: a stop signal, or `POST /v1/frontend/drain`.
/// Both end in the same sequence ([`Retire::drain`]), and these tests run each.
#[derive(Clone, Copy, Debug)]
enum Begin {
    Signal,
    Endpoint,
}

/// The `Execution` service over the rig's farm, as the REAPI listener serves it.
fn execution(rig: &Rig) -> kbf_front::ExecutionService<Farm> {
    kbf_front::ExecutionService::with_authorizers(
        rig.backend.clone(),
        Arc::clone(&rig.farm),
        rig.closing.clone(),
        Arc::new(kbf_auth::Authorizers::allow_all()),
    )
}

/// Opens a WaitExecution stream on `ticket`'s operation and follows it in a task, which
/// ends with the stream: `Ok` when the stream ended cleanly (the operation is done) and
/// the status when it ended with an error.
async fn follow(
    rig: &Rig,
    ticket: &kbf_front::Ticket,
) -> tokio::task::JoinHandle<Result<(), Status>> {
    use futures::StreamExt as _;
    use kbf_proto::reapi::execution_server::Execution as _;
    let request = kbf_proto::reapi::WaitExecutionRequest {
        name: ticket.name.clone(),
    };
    let mut stream = execution(rig)
        .wait_execution(tonic::Request::new(request))
        .await
        .expect("WaitExecution")
        .into_inner();
    tokio::spawn(async move {
        while let Some(operation) = stream.next().await {
            operation?;
        }
        Ok(())
    })
}

/// Runs the stop (or the endpoint's drain) in a task of its own, which ends with it.
fn begin(rig: &Rig, how: Begin, timeout: Duration) -> tokio::task::JoinHandle<()> {
    let retire = Arc::new(rig.retire());
    let drain = Arc::clone(&rig.drain);
    tokio::spawn(async move {
        match how {
            Begin::Signal => stopped(&drain, &retire, timeout).await,
            Begin::Endpoint => {
                retire.get().expect("set").drain();
                drain.sequence_finished().await;
            }
        }
    })
}

/// Lets the tasks that are ready run, and `by` of paused time pass.
async fn pass(by: Duration) {
    tokio::time::sleep(by).await;
}

/// A result for `lease`, which the farm accepts without a cache write: the operation
/// is done.
fn finished(lease: kbf_types::LeaseId) -> kbf_proto::worker::Result {
    kbf_proto::worker::Result {
        lease_id: Some(kbf_proto::worker::LeaseId {
            term: lease.term,
            seq: lease.seq,
        }),
        status: Some(kbf_proto::google::rpc::Status {
            code: Code::InvalidArgument as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        action_result: None,
        action_digest: None,
        memory_kill: 0,
        lost: 0,
    }
}

/// Puts one operation under a running lease and one in the queue with every worker
/// cordoned, and opens a stream on each. Returns the running one's lease and both
/// streams, queued first.
async fn one_running_one_queued(
    rig: &Rig,
) -> (
    kbf_types::LeaseId,
    tokio::task::JoinHandle<Result<(), Status>>,
    tokio::task::JoinHandle<Result<(), Status>>,
) {
    let mut inbox = register(rig);
    let running = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 1, "the first operation did not start");
    rig.farm
        .place(&WorkerId::new("node-a"), crate::farm::NodeAction::Cordon)
        .expect("cordoned");
    let queued = submit(&rig.farm, request(2)).expect("queued");
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 0, "a cordoned node was given work");
    let views = rig.farm.lease_views(None);
    let [view] = &views.leases[..] else {
        panic!("one lease expected");
    };
    let lease = view.lease;
    let queued = follow(rig, &queued).await;
    let running = follow(rig, &running).await;
    (lease, queued, running)
}

/// Asserts that a stop, or the endpoint's drain, with one operation running and one queued
/// behind a cordoned worker: leaves both streams alone while the load balancer wait runs;
/// when it is over ends the queued stream UNAVAILABLE at once, naming the operation, while
/// the running one stays open; keeps the drain going while the lease runs, far past the
/// load balancer wait; and, once the lease is done, answers the running stream with its
/// done operation and ends the drain.
async fn a_queued_stream_ends_at_once_and_a_running_one_is_kept(how: Begin) {
    let rig = Rig::new(timing(10, 900));
    let (lease, queued, running) = one_running_one_queued(&rig).await;
    let began = Instant::now();
    let stop = begin(&rig, how, Duration::MAX);

    pass(Duration::from_secs(9)).await;
    assert!(!queued.is_finished(), "{how:?}: ended in the wait");
    assert!(!running.is_finished());

    pass(Duration::from_secs(2)).await;
    assert!(queued.is_finished(), "{how:?}: the queued stream is open");
    let ended = queued.await.expect("task").expect_err("ended");
    assert_eq!(ended.code(), Code::Unavailable, "{how:?}: {ended:?}");
    assert!(ended.message().contains("operations/"), "{ended:?}");
    assert!(
        began.elapsed() < Duration::from_secs(12),
        "{how:?}: {:?}",
        began.elapsed()
    );

    pass(Duration::from_secs(120)).await;
    assert!(!running.is_finished(), "{how:?}: the running stream ended");
    assert!(!stop.is_finished(), "{how:?}: exited with a lease running");
    assert_eq!(rig.drain.phase(), Phase::Draining);

    rig.farm
        .report(&WorkerId::new("node-a"), finished(lease))
        .await;
    let ended = tokio::time::timeout(Duration::from_secs(10), stop);
    ended
        .await
        .expect("the stop ends once the lease is done")
        .expect("task");
    running
        .await
        .expect("task")
        .expect("the done operation, then the end");
}

#[tokio::test(start_paused = true)]
async fn a_stop_ends_the_queued_stream_at_once_and_keeps_the_running_one() {
    a_queued_stream_ends_at_once_and_a_running_one_is_kept(Begin::Signal).await;
}

#[tokio::test(start_paused = true)]
async fn the_endpoints_drain_ends_the_queued_stream_at_once_and_keeps_the_running_one() {
    a_queued_stream_ends_at_once_and_a_running_one_is_kept(Begin::Endpoint).await;
}

/// Asserts that with only a queued operation, behind a cordoned worker, the stop (or the
/// endpoint's drain) is over by the load balancer wait and a small margin, with no second
/// signal and no stop timeout, and not at the drain deadline.
async fn only_a_queued_operation_does_not_hold_the_stop(how: Begin) {
    let rig = Rig::new(timing(10, 900));
    let _inbox = register(&rig);
    rig.farm
        .place(&WorkerId::new("node-a"), crate::farm::NodeAction::Cordon)
        .expect("cordoned");
    let queued = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.tick().await;
    let stream = follow(&rig, &queued).await;
    let began = Instant::now();
    let stop = begin(&rig, how, Duration::MAX);

    let waited = tokio::time::timeout(Duration::from_secs(60), stop).await;
    waited
        .expect("the stop ends long before the deadline")
        .expect("task");
    let took = began.elapsed();
    assert!(
        took >= Duration::from_secs(10),
        "{how:?}: before the wait: {took:?}"
    );
    assert!(
        took < Duration::from_secs(10) + Duration::from_secs(5),
        "{how:?}: {took:?}"
    );
    let ended = stream.await.expect("task").expect_err("ended");
    assert_eq!(ended.code(), Code::Unavailable, "{how:?}: {ended:?}");
    assert_eq!(rig.drain.phase(), Phase::Closing);
    // Ending the stream did not cancel the operation: it is queued still, for the
    // frontends that take over its work.
    assert_eq!(rig.farm.queued_operations(), 1, "{how:?}");
}

#[tokio::test(start_paused = true)]
async fn a_stop_with_only_a_queued_operation_ends_within_the_lb_wait() {
    only_a_queued_operation_does_not_hold_the_stop(Begin::Signal).await;
}

#[tokio::test(start_paused = true)]
async fn the_endpoints_drain_with_only_a_queued_operation_ends_within_the_lb_wait() {
    only_a_queued_operation_does_not_hold_the_stop(Begin::Endpoint).await;
}

/// Asserts that a stop whose running action outlasts `--drain-deadline`, with no stop
/// timeout, ends at the deadline (and the clients' grace) and no earlier: the running
/// stream is then ended UNAVAILABLE.
async fn a_running_action_is_cut_at_the_drain_deadline(how: Begin) {
    let rig = Rig::new(timing(0, 30));
    let mut inbox = register(&rig);
    let running = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 1, "the operation did not start");
    let running = follow(&rig, &running).await;
    let began = Instant::now();
    let stop = begin(&rig, how, Duration::MAX);

    pass(Duration::from_secs(29)).await;
    assert!(!running.is_finished());
    assert!(!stop.is_finished(), "{how:?}: ended before the deadline");

    let waited = tokio::time::timeout(Duration::from_secs(60), stop).await;
    waited
        .expect("the stop ends at the deadline")
        .expect("task");
    let took = began.elapsed();
    assert!(took >= Duration::from_secs(30), "{how:?}: {took:?}");
    let latest = Duration::from_secs(30) + CLOSE_GRACE + Duration::from_secs(2);
    assert!(took < latest, "{how:?}: {took:?}");
    let ended = running.await.expect("task").expect_err("ended");
    assert_eq!(ended.code(), Code::Unavailable, "{how:?}: {ended:?}");
}

#[tokio::test(start_paused = true)]
async fn a_stop_ends_at_the_drain_deadline_when_a_running_action_outlasts_it() {
    a_running_action_is_cut_at_the_drain_deadline(Begin::Signal).await;
}

#[tokio::test(start_paused = true)]
async fn the_endpoints_drain_ends_at_the_deadline_when_a_running_action_outlasts_it() {
    a_running_action_is_cut_at_the_drain_deadline(Begin::Endpoint).await;
}

/// Asserts that an operation a worker can take but that is only queued (the node is full)
/// keeps its stream through the drain, which no longer ends it: the stream is open past the
/// load balancer wait and until the deadline, which ends it with the stop. Buck2 does not
/// recover a stream that ends after the call opened, so a queued operation is not ended
/// early.
async fn a_queued_operation_a_worker_can_take_keeps_its_stream(how: Begin) {
    let rig = Rig::new(timing(0, 30));
    let mut inbox = register(&rig);
    for tag in 1..=4 {
        let _running = submit(&rig.farm, request(tag)).expect("queued");
    }
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 4, "the node did not fill");
    let queued = submit(&rig.farm, request(5)).expect("queued");
    rig.farm.tick().await;
    assert!(matches!(*queued.stage.borrow(), kbf_front::Stage::Queued));
    let stream = follow(&rig, &queued).await;
    let began = Instant::now();
    let stop = begin(&rig, how, Duration::MAX);

    pass(Duration::from_secs(29)).await;
    assert!(
        !stream.is_finished(),
        "{how:?}: a queued stream was ended early"
    );
    assert!(
        !stop.is_finished(),
        "{how:?}: the stop ended before the deadline"
    );
    tokio::time::timeout(Duration::from_secs(60), stop)
        .await
        .expect("the stop ends at the deadline")
        .expect("task");
    assert!(began.elapsed() >= Duration::from_secs(30), "{how:?}");
    let ended = stream.await.expect("task").expect_err("ended");
    assert_eq!(ended.code(), Code::Unavailable, "{how:?}: {ended:?}");
}

#[tokio::test(start_paused = true)]
async fn a_stop_keeps_the_stream_of_a_queued_operation_a_worker_can_take() {
    a_queued_operation_a_worker_can_take_keeps_its_stream(Begin::Signal).await;
}

#[tokio::test(start_paused = true)]
async fn the_endpoints_drain_keeps_the_stream_of_a_queued_operation_a_worker_can_take() {
    a_queued_operation_a_worker_can_take_keeps_its_stream(Begin::Endpoint).await;
}

/// Asserts that an operation that had a lease, lost it to a silent node and now waits
/// behind a cordoned worker keeps its stream: it reads `WaitingAfterStart`, not `Waiting`,
/// and the stream lasts until the deadline, as that of any operation that has run.
async fn a_requeued_operation_keeps_its_stream(how: Begin) {
    let rig = Rig::new(timing(0, 30));
    let mut inbox = register(&rig);
    let ticket = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 1, "the operation did not start");
    rig.farm
        .place(&WorkerId::new("node-a"), crate::farm::NodeAction::Cordon)
        .expect("cordoned");
    // The node goes silent past the lease grace (60 s): the lease is given up and requeued.
    pass(Duration::from_secs(70)).await;
    rig.farm.tick().await;
    assert!(
        matches!(
            *ticket.stage.borrow(),
            kbf_front::Stage::WaitingAfterStart(_)
        ),
        "{how:?}: {:?}",
        *ticket.stage.borrow()
    );
    let stream = follow(&rig, &ticket).await;
    let began = Instant::now();
    let stop = begin(&rig, how, Duration::MAX);

    pass(Duration::from_secs(29)).await;
    assert!(
        !stream.is_finished(),
        "{how:?}: an operation that ran was ended early"
    );
    assert!(!stop.is_finished(), "{how:?}");
    tokio::time::timeout(Duration::from_secs(60), stop)
        .await
        .expect("the stop ends at the deadline")
        .expect("task");
    assert!(began.elapsed() >= Duration::from_secs(30), "{how:?}");
}

#[tokio::test(start_paused = true)]
async fn a_stop_keeps_the_stream_of_an_operation_requeued_after_a_lost_lease() {
    a_requeued_operation_keeps_its_stream(Begin::Signal).await;
}

#[tokio::test(start_paused = true)]
async fn the_endpoints_drain_keeps_the_stream_of_an_operation_requeued_after_a_lost_lease() {
    a_requeued_operation_keeps_its_stream(Begin::Endpoint).await;
}

/// Asserts the order of a lease's start: at the point the `Start` is about to leave, the
/// operation's callers already read `Executing`, so a drain that ends the streams of
/// operations that cannot start never ends one whose `Start` is in flight.
#[tokio::test(start_paused = true)]
async fn a_start_is_never_sent_before_its_callers_read_executing() {
    let rig = Rig::new(timing(0, 30));
    let _inbox = register(&rig);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    crate::farm::BEFORE_START.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |stages| {
            record.lock().expect("lock").push(stages);
        }));
    });
    let _ticket = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.tick().await;
    crate::farm::BEFORE_START.with(|hook| hook.borrow_mut().take());
    assert_eq!(
        *seen.lock().expect("lock"),
        [[kbf_front::Stage::Executing]],
        "the Start left before its callers read Executing"
    );
}
