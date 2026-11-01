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
        deadline: Some(Duration::from_secs(deadline)),
    }
}

/// The load balancer wait in seconds, and no deadline: the default.
fn no_deadline(lb_wait: u64) -> DrainTiming {
    DrainTiming {
        lb_wait: Duration::from_secs(lb_wait),
        deadline: None,
    }
}

/// A serving frontend's parts, as the drain stands them down: a farm over a state store, a
/// drain with the listeners' stops, and the sequence over them.
struct Rig {
    /// The state store this frontend shares with [`other_frontend`]s.
    shared: Arc<Fake>,
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
        let shared = Arc::new(store);
        let backend = Backend::from(Arc::new(Cache::memory()));
        let farm = Farm::new(
            backend.clone(),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        )
        .with_state(Some(StateStore {
            store: Arc::clone(&shared) as Arc<dyn Store>,
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
            shared,
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
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
        whole_worker: false,
    }
}

/// Queues `request` on `farm`.
fn submit(farm: &Farm, request: Request) -> Result<kbf_front::Ticket, Status> {
    farm.submit(Submission {
        request,
        call: None,
        described: Described::default(),
        evidence: Default::default(),
        max_wall: None,
    })
}

/// Registers the node `node-a` on the rig's farm, as a daemon's first stream does.
fn register(rig: &Rig) -> Inbox {
    register_stream(rig).1
}

/// [`register`], and the id of the stream the node is registered on.
fn register_stream(rig: &Rig) -> (crate::farm::StreamId, Inbox) {
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, inbox) = mpsc::unbounded_channel();
    let stream = rig.farm.register(
        &WorkerId::new("node-a"),
        DaemonInstance::new("daemon"),
        DaemonInstance::new(""),
        Reported::serving(Resources::new(4_000, 8 << 30), caps),
        outbound,
        Default::default(),
    );
    (stream, inbox)
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
/// signal began, with no drain deadline: an execution that never ends is closed when it
/// runs out.
#[tokio::test(start_paused = true)]
async fn the_stop_timeout_closes_the_streams_of_a_sequence() {
    let rig = Rig::new(no_deadline(0));
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

/// Asserts that the defaults are the ones the flags document: ten seconds of wait and no
/// deadline.
#[test]
fn the_defaults_are_ten_seconds_and_no_deadline() {
    let timing = DrainTiming::default();
    assert_eq!(timing.lb_wait, Duration::from_secs(10));
    assert_eq!(timing.deadline, None);
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

/// Asserts that a stop whose streams are all kept is still going on, whatever deadline it
/// has passed, then ends it as a second signal does and waits for it.
async fn force_stop(rig: &Rig, stop: tokio::task::JoinHandle<()>) {
    pass_feeding(rig, 5).await;
    assert!(!stop.is_finished(), "a deadline ended the stop");
    rig.drain.close();
    tokio::time::timeout(Duration::from_secs(60), stop)
        .await
        .expect("the second signal ends the stop")
        .expect("task");
}

/// Lets the tasks that are ready run, and `by` of paused time pass.
async fn pass(by: Duration) {
    tokio::time::sleep(by).await;
}

/// Lets `secs` seconds of paused time pass, feeding the farm every second as its
/// heartbeats and ticks do: the drain's hold on the nodes is applied on a farm input, not
/// when the drain begins, so a test that never feeds the farm sees a farm that has not
/// yet held anything.
async fn pass_feeding(rig: &Rig, secs: u64) {
    for _ in 0..secs {
        pass(Duration::from_secs(1)).await;
        rig.farm.tick().await;
    }
    // Let the tasks the last input woke run.
    pass(Duration::from_millis(1)).await;
}

/// Lets `secs` seconds of paused time pass as [`pass_feeding`] does, with the node's daemon
/// heartbeating on `stream` every second and listing `lease` as running, so that the lease
/// is not given up as a silent node's would be.
async fn pass_beating(
    rig: &Rig,
    stream: crate::farm::StreamId,
    lease: kbf_types::LeaseId,
    secs: u64,
) {
    for beat in 1..=secs {
        pass(Duration::from_secs(1)).await;
        let worker = WorkerId::new("node-a");
        assert!(rig.farm.heartbeat(&worker, stream, beat, vec![lease]).await);
        rig.farm.tick().await;
    }
    pass(Duration::from_millis(1)).await;
}

/// The errors in `inbox`: the streams a drain ended.
fn errors(inbox: &mut Inbox) -> Vec<Code> {
    std::iter::from_fn(|| inbox.try_recv().ok())
        .filter_map(|message| message.err())
        .map(|status| status.code())
        .collect()
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
        job_outcome: None,
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

    pass_feeding(&rig, 9).await;
    assert!(!queued.is_finished(), "{how:?}: ended in the wait");
    assert!(!running.is_finished());

    pass_feeding(&rig, 2).await;
    assert!(queued.is_finished(), "{how:?}: the queued stream is open");
    let ended = queued.await.expect("task").expect_err("ended");
    assert_eq!(ended.code(), Code::Unavailable, "{how:?}: {ended:?}");
    assert!(ended.message().contains("operations/"), "{ended:?}");
    assert!(
        began.elapsed() < Duration::from_secs(12),
        "{how:?}: {:?}",
        began.elapsed()
    );

    pass_feeding(&rig, 40).await;
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

/// Asserts that an operation a worker can take but that is only queued (the node is full)
/// keeps its stream through the drain, which no longer ends it: the stream is open past the
/// load balancer wait and the deadline, until a second signal ends it with the stop. Buck2
/// does not recover a stream that ends after the call opened, so a queued operation is not
/// ended early.
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
    let stop = begin(&rig, how, Duration::MAX);

    pass_feeding(&rig, 40).await;
    assert!(
        !stream.is_finished(),
        "{how:?}: a queued stream was ended early"
    );
    force_stop(&rig, stop).await;
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
/// behind a cordoned worker keeps its stream: it reads `WaitingKept`, not `Waiting`,
/// and the stream lasts past the deadline, as that of any operation that has run.
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
        matches!(*ticket.stage.borrow(), kbf_front::Stage::WaitingKept(_)),
        "{how:?}: {:?}",
        *ticket.stage.borrow()
    );
    let stream = follow(&rig, &ticket).await;
    let stop = begin(&rig, how, Duration::MAX);

    pass_feeding(&rig, 40).await;
    assert!(
        !stream.is_finished(),
        "{how:?}: an operation that ran was ended early"
    );
    force_stop(&rig, stop).await;
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

/// Asserts that an operation no worker could ever run (none is connected) is ended at the
/// end of the load balancer wait, with the stop, whichever way the drain began: it would
/// only fail at the deadline.
async fn an_operation_no_worker_could_run_ends_at_the_lb_wait(how: Begin) {
    let rig = Rig::new(timing(10, 900));
    let ticket = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.tick().await;
    assert!(
        matches!(*ticket.stage.borrow(), kbf_front::Stage::Waiting(_)),
        "{how:?}: {:?}",
        *ticket.stage.borrow()
    );
    let stream = follow(&rig, &ticket).await;
    let began = Instant::now();
    let stop = begin(&rig, how, Duration::MAX);

    pass_feeding(&rig, 9).await;
    assert!(!stream.is_finished(), "{how:?}: ended in the wait");
    pass_feeding(&rig, 2).await;
    let ended = stream.await.expect("task").expect_err("ended");
    assert_eq!(ended.code(), Code::Unavailable, "{how:?}: {ended:?}");
    tokio::time::timeout(Duration::from_secs(60), stop)
        .await
        .expect("the stop ends")
        .expect("task");
    assert!(began.elapsed() < Duration::from_secs(15), "{how:?}");
}

#[tokio::test(start_paused = true)]
async fn a_stop_ends_the_stream_of_an_operation_no_worker_could_run_at_the_lb_wait() {
    an_operation_no_worker_could_run_ends_at_the_lb_wait(Begin::Signal).await;
}

#[tokio::test(start_paused = true)]
async fn the_endpoints_drain_ends_the_stream_of_an_operation_no_worker_could_run_at_the_lb_wait() {
    an_operation_no_worker_could_run_ends_at_the_lb_wait(Begin::Endpoint).await;
}

/// The cases where an operator's cordon is part of what holds the operation. The
/// operation can never start on this frontend once the drain is under way, whether the
/// capable workers are all operator-cordoned (the sole worker, or both of two) or only
/// some are (the others being held by the drain): the operator's cordon is a reason the
/// operation would also have waited for after the drain, so its stream ends at the end of
/// the load balancer wait and does not delay the stop. Only an operation whose every
/// capable worker is held by the drain alone stays queued for the other frontends.
async fn operator_cordoned_workers_end_the_stream_at_the_lb_wait(how: Begin, two_nodes: bool) {
    let rig = Rig::new(timing(10, 900));
    let mut inbox = register(&rig);
    rig.farm
        .place(&WorkerId::new("node-a"), crate::farm::NodeAction::Cordon)
        .expect("cordoned");
    if two_nodes {
        let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
            .expect("caps")
            .with_drivers(["container"]);
        let (outbound, _inbox_b) = mpsc::unbounded_channel();
        rig.farm.register(
            &WorkerId::new("node-b"),
            DaemonInstance::new("daemon-b"),
            DaemonInstance::new(""),
            Reported::serving(Resources::new(4_000, 8 << 30), caps),
            outbound,
            Default::default(),
        );
        for tag in 1..=4 {
            let _running = submit(&rig.farm, request(tag)).expect("queued");
        }
        rig.farm.tick().await;
    }
    let ticket = submit(&rig.farm, request(9)).expect("queued");
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 0, "a cordoned node was given work");
    let stream = follow(&rig, &ticket).await;
    let stop = begin(&rig, how, Duration::MAX);

    pass_feeding(&rig, 9).await;
    assert!(!stream.is_finished(), "{how:?}: ended in the wait");
    pass_feeding(&rig, 2).await;
    assert!(
        stream.is_finished(),
        "{how:?}: two_nodes {two_nodes}: still open"
    );
    let ended = stream.await.expect("task").expect_err("ended");
    assert_eq!(ended.code(), Code::Unavailable, "{how:?}: {ended:?}");
    if !two_nodes {
        tokio::time::timeout(Duration::from_secs(60), stop)
            .await
            .expect("the stop ends")
            .expect("task");
    }
}

#[tokio::test(start_paused = true)]
async fn an_operator_cordoned_only_worker_ends_the_stream_at_the_lb_wait() {
    operator_cordoned_workers_end_the_stream_at_the_lb_wait(Begin::Signal, false).await;
    operator_cordoned_workers_end_the_stream_at_the_lb_wait(Begin::Endpoint, false).await;
}

#[tokio::test(start_paused = true)]
async fn an_operator_cordoned_worker_beside_a_drain_held_one_ends_the_stream_at_the_lb_wait() {
    operator_cordoned_workers_end_the_stream_at_the_lb_wait(Begin::Signal, true).await;
    operator_cordoned_workers_end_the_stream_at_the_lb_wait(Begin::Endpoint, true).await;
}

/// A second frontend over the rig's state store with a node of its own, `node-x`, whose
/// session row the first frontend finds at its intake poll. Returned with the node's
/// stream, which is kept open.
fn other_frontend(rig: &Rig) -> (Farm, Inbox) {
    let farm = Farm::new(
        Backend::from(Arc::new(Cache::memory())),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_state(Some(StateStore {
        store: Arc::clone(&rig.shared) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }))
    .expect("a second farm over the store");
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, inbox) = mpsc::unbounded_channel();
    farm.register(
        &WorkerId::new("node-x"),
        DaemonInstance::new("daemon-x"),
        DaemonInstance::new(""),
        Reported::serving(Resources::new(4_000, 8 << 30), caps),
        outbound,
        Default::default(),
    );
    (farm, inbox)
}

/// Asserts what the ruling says for an operation no worker of this frontend could run: with
/// another frontend that has a worker that could, its stream is kept through the load
/// balancer wait (that frontend takes the operation), past the deadline, until a second
/// signal ends it with the stop; with no other frontend it ends at the end of the wait.
async fn a_waiting_stream_follows_whether_another_frontend_could_take_it(how: Begin, other: bool) {
    let rig = Rig::new(timing(10, 30));
    let _other = other.then(|| other_frontend(&rig));
    let ticket = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.tick().await;
    rig.farm.intake();
    let kept = matches!(*ticket.stage.borrow(), kbf_front::Stage::WaitingKept(_));
    assert_eq!(kept, other, "{how:?}: {:?}", *ticket.stage.borrow());
    let stream = follow(&rig, &ticket).await;
    let stop = begin(&rig, how, Duration::MAX);

    pass_feeding(&rig, 40).await;
    assert_eq!(
        stream.is_finished(),
        !other,
        "{how:?}: another frontend {other}"
    );
    if other {
        force_stop(&rig, stop).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_stop_keeps_a_waiting_stream_when_another_frontend_could_take_it() {
    a_waiting_stream_follows_whether_another_frontend_could_take_it(Begin::Signal, true).await;
}

#[tokio::test(start_paused = true)]
async fn the_endpoints_drain_keeps_a_waiting_stream_when_another_frontend_could_take_it() {
    a_waiting_stream_follows_whether_another_frontend_could_take_it(Begin::Endpoint, true).await;
}

#[tokio::test(start_paused = true)]
async fn a_stop_ends_a_waiting_stream_when_no_other_frontend_could_take_it() {
    a_waiting_stream_follows_whether_another_frontend_could_take_it(Begin::Signal, false).await;
}

#[tokio::test(start_paused = true)]
async fn the_endpoints_drain_ends_a_waiting_stream_when_no_other_frontend_could_take_it() {
    a_waiting_stream_follows_whether_another_frontend_could_take_it(Begin::Endpoint, false).await;
}

/// Asserts that an operation that waited for a worker and went back to `Queued` when a
/// worker that could run it appeared (the node is full) keeps its stream through the drain:
/// its stage reads `Queued`, not `Waiting`, once the reason is cleared.
async fn an_operation_that_stopped_waiting_keeps_its_stream(how: Begin) {
    let rig = Rig::new(timing(10, 30));
    let tickets: Vec<_> = (1..=5)
        .map(|tag| submit(&rig.farm, request(tag)).expect("queued"))
        .collect();
    rig.farm.tick().await;
    let last = &tickets[4];
    assert!(
        matches!(*last.stage.borrow(), kbf_front::Stage::Waiting(_)),
        "{how:?}: {:?}",
        *last.stage.borrow()
    );
    let mut inbox = register(&rig);
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 4, "{how:?}: the node did not fill");
    assert!(
        matches!(*last.stage.borrow(), kbf_front::Stage::Queued),
        "{how:?}: {:?}",
        *last.stage.borrow()
    );
    let stream = follow(&rig, last).await;
    let stop = begin(&rig, how, Duration::MAX);

    pass_feeding(&rig, 40).await;
    assert!(!stream.is_finished(), "{how:?}: ended");
    force_stop(&rig, stop).await;
}

#[tokio::test(start_paused = true)]
async fn a_stop_keeps_the_stream_of_an_operation_that_stopped_waiting() {
    an_operation_that_stopped_waiting_keeps_its_stream(Begin::Signal).await;
}

#[tokio::test(start_paused = true)]
async fn the_endpoints_drain_keeps_the_stream_of_an_operation_that_stopped_waiting() {
    an_operation_that_stopped_waiting_keeps_its_stream(Begin::Endpoint).await;
}

/// Asserts that another frontend's node that cannot run the operation (too small for its
/// request, or of another platform) does not keep its stream: the operation still reads
/// `Waiting`, and the stream ends at the end of the load balancer wait.
async fn another_frontends_node_that_cannot_run_it_does_not_keep_the_stream(
    how: Begin,
    request: Request,
) {
    let rig = Rig::new(timing(10, 900));
    let _other = other_frontend(&rig);
    let ticket = submit(&rig.farm, request).expect("queued");
    rig.farm.tick().await;
    rig.farm.intake();
    assert!(
        matches!(*ticket.stage.borrow(), kbf_front::Stage::Waiting(_)),
        "{how:?}: {:?}",
        *ticket.stage.borrow()
    );
    let stream = follow(&rig, &ticket).await;
    let stop = begin(&rig, how, Duration::MAX);

    pass_feeding(&rig, 9).await;
    assert!(!stream.is_finished(), "{how:?}: ended in the wait");
    pass_feeding(&rig, 2).await;
    let ended = stream.await.expect("task").expect_err("ended");
    assert_eq!(ended.code(), Code::Unavailable, "{how:?}: {ended:?}");
    tokio::time::timeout(Duration::from_secs(60), stop)
        .await
        .expect("the stop ends with the wait")
        .expect("task");
}

#[tokio::test(start_paused = true)]
async fn another_frontends_node_too_small_for_the_operation_does_not_keep_its_stream() {
    for how in [Begin::Signal, Begin::Endpoint] {
        let big = Request {
            resources: Resources::new(64_000, 1 << 30),
            ..request(1)
        };
        another_frontends_node_that_cannot_run_it_does_not_keep_the_stream(how, big).await;
    }
}

#[tokio::test(start_paused = true)]
async fn another_frontends_node_of_another_platform_does_not_keep_the_stream() {
    for how in [Begin::Signal, Begin::Endpoint] {
        let mac = Request {
            needs: kbf_caps::Request::from_platform([("OSFamily", "macos")]).expect("platform"),
            ..request(1)
        };
        another_frontends_node_that_cannot_run_it_does_not_keep_the_stream(how, mac).await;
    }
}

mod deadline;
mod skips;
