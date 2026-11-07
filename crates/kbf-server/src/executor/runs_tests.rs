use std::collections::BTreeMap;
use std::time::Duration;

use js_proto::v1::{
    CancelJobRequest, GetResultResponse, JobResult, JobSpec, JobState, Needs, NetworkLevel,
    ResultReason, StartFailureCode,
};
use kbf_job_client::fake::{Call, Fault};
use kbf_proto::google::rpc::ErrorInfo;
use kbf_proto::worker::{Cancel, Lost, ResultAck, ServerMessage, Start, server_message};
use kbf_types::{LeaseId, LeaseKind};
use prost::Message;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;
use tonic::Code;

use super::{Reporting, Stage};
use crate::executor::host::HostWorker;
use crate::executor::test_support::{GIB, HOST, Harness, Log, start};
use crate::executor::{job_id, twin_message};

type Messages = UnboundedSender<Result<ServerMessage, tonic::Status>>;

fn tell(messages: &Messages, message: server_message::Message) {
    messages
        .send(Ok(ServerMessage {
            message: Some(message),
        }))
        .expect("sent");
}

fn tell_start(messages: &Messages, start: Start) {
    tell(messages, server_message::Message::Start(start));
}

fn tell_cancel(messages: &Messages, term: u64, seq: u64) {
    let lease_id = Some(kbf_proto::worker::LeaseId { term, seq });
    tell(
        messages,
        server_message::Message::Cancel(Cancel { lease_id }),
    );
}

async fn step(harness: &Harness, worker: &mut HostWorker) {
    worker.step(&harness.ctx(), Instant::now()).await;
}

/// The value of the counter `name`, as the metrics page shows it.
fn counted(harness: &Harness, name: &str) -> u64 {
    let mut text = String::new();
    harness.deps.metrics.render(&mut text);
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
        .unwrap_or_else(|| panic!("no counter {name} in\n{text}"))
}

/// The calls the job service has handled since there were `before` of them.
fn calls_since(harness: &Harness, before: usize) -> Vec<Call> {
    harness.fake.calls()[before..].to_vec()
}

fn cancel_of(id: &str) -> Call {
    Call::CancelJob(CancelJobRequest {
        job_id: id.to_owned(),
    })
}

fn submitted_specs(harness: &Harness) -> Vec<JobSpec> {
    harness
        .fake
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::SubmitJob(request) => request.spec,
            _ => None,
        })
        .collect()
}

/// The result of the task of `lease`, which must be waiting for the farm to acknowledge it.
fn reported(worker: &HostWorker, lease: LeaseId) -> &kbf_proto::worker::Result {
    let Some(Stage::Reporting(report)) = worker.tasks.get(&lease).map(|task| &task.stage) else {
        panic!("the task of {lease} is not reporting");
    };
    &report.result
}

/// The code and message of a result's status.
fn status_of(result: &kbf_proto::worker::Result) -> (i32, &str) {
    let status = result.status.as_ref().expect("a status");
    (status.code, status.message.as_str())
}

/// Starts lease 1.1 and steps once: its job is submitted, and then on the service, placed,
/// starting and running. The job's id.
async fn running_job(harness: &Harness, worker: &mut HostWorker, messages: &Messages) -> String {
    tell_start(messages, start(1, 1));
    step(harness, worker).await;
    let lease = LeaseId::new(1, 1);
    let Some(Stage::Running(running)) = worker.tasks.get(&lease).map(|task| &task.stage) else {
        panic!("the task is not running");
    };
    let id = running.job.clone();
    assert!(harness.fake.place(&id, HOST));
    assert!(harness.fake.starting(&id));
    assert!(harness.fake.run(&id));
    id
}

/// Catches a result being forgotten that the farm did not acknowledge: `report` answers
/// `None` for a result its record is not written for (and for one it dropped over its
/// cap), and the send that panicked answers nothing at all. Neither is an acknowledgement:
/// the result is sent again, and only a real acknowledgement ends the sending.
#[tokio::test(start_paused = true)]
async fn a_report_the_farm_did_not_acknowledge_is_sent_again() {
    let harness = Harness::new().await;
    let worker = kbf_types::WorkerId::new("js:node1");
    let held = tokio::spawn(async { None::<ResultAck> });
    let broken = tokio::spawn(async { panic!("the report broke") });
    let acked = tokio::spawn(async { Some(ResultAck::default()) });
    for (what, in_flight, done) in [
        ("held", held, false),
        ("broken", broken, false),
        ("acknowledged", acked, true),
    ] {
        let mut report = Reporting {
            result: kbf_proto::worker::Result::default(),
            in_flight: Some(in_flight),
            retry_at: Instant::now(),
        };
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        let said = report.drive(
            &harness.farm,
            &worker,
            Instant::now(),
            Duration::from_secs(5),
        );
        assert_eq!(said, done, "{what}");
        assert_eq!(report.in_flight.is_some(), !done, "{what}: sent again");
    }
}

/// Catches a submit whose answer was lost making a second job, or giving up the first: the
/// task keeps its prepared job and sends the same one again (even when the farm sends the
/// `Start` again meanwhile), the service holds one job, the task follows it, and the ending of
/// that job is the one the farm is told. The resubmit is counted.
#[tokio::test(start_paused = true)]
async fn a_start_is_submitted_once_even_when_the_answer_is_lost() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let lease = LeaseId::new(1, 1);
    let id = job_id("front", lease, 0);
    harness.fake.lose_next_answer();
    tell_start(&messages, start(1, 1));
    step(&harness, &mut worker).await;
    let Some(Stage::Waiting(waiting)) = worker.tasks.get(&lease).map(|task| &task.stage) else {
        panic!("the task is not waiting");
    };
    assert!(waiting.sent);
    let (deadline, spec) = (
        waiting.deadline,
        waiting.prepared.as_ref().expect("prepared").spec.clone(),
    );
    assert_eq!(harness.fake.jobs().len(), 1);

    // The farm sends the same `Start` again, naming a heartbeat the worker has sent.
    assert!(worker.sent_at(1).is_some());
    worker.accept(&Start {
        heartbeat_seq: 1,
        ..start(1, 1)
    });
    assert_eq!(worker.tasks.len(), 1);
    let Some(Stage::Waiting(again)) = worker.tasks.get(&lease).map(|task| &task.stage) else {
        panic!("the task is not waiting");
    };
    assert!(again.sent);
    assert_eq!(again.deadline, deadline);
    assert_eq!(again.prepared.as_ref().expect("prepared").spec, spec);

    step(&harness, &mut worker).await;
    let sent = submitted_specs(&harness);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1]);
    assert_eq!(sent[1].job_id, id);
    assert_eq!(sent[1], spec);
    assert_eq!(harness.fake.jobs().len(), 1);
    let Some(Stage::Running(running)) = worker.tasks.get(&lease).map(|task| &task.stage) else {
        panic!("the task is not running");
    };
    assert_eq!(running.job, id);
    assert_eq!(counted(&harness, "kbf_executor_jobs_submitted_total"), 1);

    assert!(harness.fake.place(&id, HOST));
    assert!(harness.fake.starting(&id));
    assert!(harness.fake.run(&id));
    assert!(harness.fake.finish(&id, 1));
    step(&harness, &mut worker).await;
    let (code, message) = status_of(reported(&worker, lease));
    assert_eq!(code, Code::Unavailable as i32);
    assert_eq!(message, "the task tool exited nonzero (exit code Some(1))");
    assert_eq!(
        counted(
            &harness,
            "kbf_executor_jobs_resubmitted_after_lost_answer_total"
        ),
        1
    );
}

/// Catches a farm `Cancel` that leaves a job going or forgets the lease before the service has
/// answered: a running job is cancelled with one call, nothing else is asked about it, and
/// the task is dropped once the service answered; a cancel whose answer was lost is sent
/// again; a lease whose `Start` was never submitted is dropped with no call at all.
#[tokio::test(start_paused = true)]
async fn a_cancel_is_a_cancel_job() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let lease = LeaseId::new(1, 1);
    let id = running_job(&harness, &mut worker, &messages).await;
    let before = harness.fake.calls().len();
    tell_cancel(&messages, 1, 1);
    step(&harness, &mut worker).await;
    assert_eq!(calls_since(&harness, before), [cancel_of(&id)]);
    assert!(!worker.tasks.contains_key(&lease));
    assert!(harness.fake.host_stopped(&id));
    assert_eq!(harness.fake.jobs()[0].state(), JobState::Cancelled);
    assert_eq!(counted(&harness, "kbf_executor_jobs_cancelled_total"), 1);

    // The answer to the cancel is lost: the task stays, and asks again.
    let other = LeaseId::new(1, 2);
    tell_start(&messages, start(1, 2));
    step(&harness, &mut worker).await;
    let Some(Stage::Running(running)) = worker.tasks.get(&other).map(|task| &task.stage) else {
        panic!("the task is not running");
    };
    let id = running.job.clone();
    let before = harness.fake.calls().len();
    tell_cancel(&messages, 1, 2);
    harness.fake.lose_next_answer();
    step(&harness, &mut worker).await;
    assert_eq!(calls_since(&harness, before), [cancel_of(&id)]);
    let Some(Stage::Withdrawing(job)) = worker.tasks.get(&other).map(|task| &task.stage) else {
        panic!("the task is not withdrawing");
    };
    assert_eq!(*job, id);
    step(&harness, &mut worker).await;
    assert_eq!(
        calls_since(&harness, before),
        [cancel_of(&id), cancel_of(&id)]
    );
    assert!(worker.tasks.is_empty());
}

/// Catches a `Cancel` of a lease whose `Start` was never submitted calling the job service:
/// there is no job to cancel, so the task is dropped and nothing is asked.
#[tokio::test(start_paused = true)]
async fn a_cancel_of_a_start_never_submitted_makes_no_call() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    tell_start(&messages, start(1, 1));
    tell_cancel(&messages, 1, 1);
    tell_cancel(&messages, 1, 77);
    step(&harness, &mut worker).await;
    assert!(worker.tasks.is_empty());
    assert!(harness.fake.calls().is_empty());
    assert!(harness.fake.jobs().is_empty());
}

/// Catches a lost job told to the farm as anything but the host's fault, and a job the service
/// forgot told as the same: a lost job is `fenced` even when a cancel was asked for (the cancel
/// is not the cause), and a job the service no longer knows after it was seen is `daemon
/// restarted`.
#[tokio::test(start_paused = true)]
async fn a_lost_job_maps_to_the_node_lost_class() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    harness
        .deps
        .client
        .cancel_job(&id)
        .await
        .expect("a cancel of another client");
    assert!(harness.fake.lose(&id));
    assert!(harness.fake.jobs()[0].cancel_requested);
    step(&harness, &mut worker).await;
    let result = reported(&worker, LeaseId::new(1, 1));
    assert_eq!(result.lost, Lost::Fenced as i32);
    assert_eq!(
        status_of(result),
        (Code::Unavailable as i32, "the job was lost")
    );

    tell_start(&messages, start(1, 2));
    step(&harness, &mut worker).await;
    let other = LeaseId::new(1, 2);
    assert!(matches!(worker.tasks[&other].stage, Stage::Running(_)));
    harness.fake.inject_on(
        "GetJob",
        Fault::Status {
            code: Code::NotFound,
            message: "no such job".to_owned(),
        },
    );
    step(&harness, &mut worker).await;
    let result = reported(&worker, other);
    assert_eq!(result.lost, Lost::DaemonRestarted as i32);
    assert_ne!(result.lost, Lost::Fenced as i32);
    assert_eq!(
        status_of(result),
        (Code::Unavailable as i32, "the job was lost")
    );
}

/// Catches a job id taken by another job being answered as the user's fault or retried, and a
/// twin going unnoticed: the refusal is infrastructure (the farm runs the action again), the
/// job that held the id is left alone, and the frontend is flagged to stop.
#[tokio::test(start_paused = true)]
async fn a_job_conflict_on_resubmit_is_infra_and_flags_a_twin() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let lease = LeaseId::new(1, 1);
    let stranger = JobSpec {
        job_id: job_id("front", lease, 0),
        image: format!("registry.test/app@sha256:{}", "ab".repeat(32)),
        needs: Some(Needs {
            cpu_millis: 1_000,
            memory_bytes: GIB,
            ..Needs::default()
        }),
        wall_limit_seconds: Some(60),
        // As the service records an unspecified network.
        network: NetworkLevel::None.into(),
        ..JobSpec::default()
    };
    harness.fake.record(stranger.clone()).expect("recorded");
    tell_start(&messages, start(1, 1));
    step(&harness, &mut worker).await;
    let (code, message) = status_of(reported(&worker, lease));
    assert_eq!(code, Code::Unavailable as i32);
    assert_eq!(
        message,
        "the job service holds another job under this job's id (job-id-conflict)"
    );
    assert_eq!(harness.twin.take(), Some(twin_message("front")));
    assert_eq!(submitted_specs(&harness).len(), 1);
    assert_eq!(harness.fake.submitted(&stranger.job_id), Some(stranger));
    assert_eq!(harness.fake.jobs().len(), 1);
}

/// Catches a whole-machine action that the executor's workers are never given, or that is
/// submitted as a share of a host. The `Hello` a worker registers with lists `container` and
/// `native-whole-machine` and no other driver, so the scheduler places `action` and
/// `whole_machine` leases on it and no VM lease. A `whole_machine` `Start` is one submit, of a
/// job that needs the whole host and nothing beside it (the CPU and memory the `Start` carries
/// are the worker's whole capacity, not a host's), and is otherwise the job a container
/// action's `Start` makes: the same image, command, environment, constraints, network, wall
/// limit and labels, and as many arguments (theirs name each its own lease). The task follows
/// the job as any other's.
#[tokio::test(start_paused = true)]
async fn a_whole_machine_action_submits_a_whole_host_job() {
    let harness = Harness::new().await;
    let hello = harness.cfg.hello("js:front-0", "inst", "");
    let drivers: Vec<&str> = hello
        .capabilities
        .iter()
        .filter(|entry| entry.key == "drivers")
        .map(|entry| entry.value.as_str())
        .collect();
    assert_eq!(drivers, ["container", "native-whole-machine"]);
    let caps = crate::worker::reported(&hello, crate::worker::HostFloor::DEFAULT)
        .expect("the farm reads the hello")
        .caps;
    let served = |kind: LeaseKind| caps.drivers.iter().any(|driver| kind.served_by(driver));
    assert_eq!(
        LeaseKind::ALL.map(|kind| (kind, served(kind))),
        [
            (LeaseKind::Action, true),
            (LeaseKind::WholeMachine, true),
            (LeaseKind::Vm, false),
        ]
    );

    let (mut worker, messages) = harness.worker();
    tell_start(
        &messages,
        Start {
            kind: "whole_machine".to_owned(),
            millicpus: 2_000_000,
            memory_bytes: 4_096 * GIB,
            ..start(1, 1)
        },
    );
    tell_start(&messages, start(1, 2));
    step(&harness, &mut worker).await;
    let specs = submitted_specs(&harness);
    assert_eq!(specs.len(), 2, "{specs:?}");
    let (whole, shared) = (&specs[0], &specs[1]);
    assert_eq!(whole.job_id, job_id("front", LeaseId::new(1, 1), 0));
    assert_eq!(
        whole.needs,
        Some(Needs {
            whole_host: true,
            ..Needs::default()
        })
    );
    assert!(shared.needs.as_ref().is_some_and(|needs| !needs.whole_host));
    let without_needs = |spec: &JobSpec| JobSpec {
        job_id: String::new(),
        needs: None,
        // The arguments and files name the lease, the grant and the pack url, which are the
        // lease's own: they are compared without the lease's values.
        args: Vec::new(),
        files: Vec::new(),
        ..spec.clone()
    };
    assert_eq!(without_needs(whole), without_needs(shared));
    assert_eq!(whole.args.len(), shared.args.len());
    let Some(Stage::Running(running)) = worker
        .tasks
        .get(&LeaseId::new(1, 1))
        .map(|task| &task.stage)
    else {
        panic!("the whole-host task is not running");
    };
    assert_eq!(running.job, whole.job_id);
    assert_eq!(counted(&harness, "kbf_executor_jobs_submitted_total"), 2);
}

/// Catches a whole-host job that the service could not ready a host for being answered as
/// the action's failure, or followed by a second job of the same lease: it ends the lease as
/// an infrastructure failure, by the outcome table's row for `exclusive-not-ready`, and the
/// farm runs the action again as a new lease.
#[tokio::test(start_paused = true)]
async fn a_whole_host_job_whose_host_was_not_readied_is_an_infrastructure_failure() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    tell_start(
        &messages,
        Start {
            kind: "whole_machine".to_owned(),
            ..start(1, 1)
        },
    );
    step(&harness, &mut worker).await;
    let id = job_of(&worker, LeaseId::new(1, 1));
    assert!(harness.fake.place(&id, HOST));
    assert!(
        harness
            .fake
            .fail_to_start(&id, StartFailureCode::ExclusiveNotReady)
    );
    step(&harness, &mut worker).await;
    let (code, message) = status_of(reported(&worker, LeaseId::new(1, 1)));
    assert_eq!(code, Code::Unavailable as i32);
    assert_eq!(message, "the job failed to start: exclusive-not-ready");
    assert_eq!(submitted_specs(&harness).len(), 1);
}

/// Catches a queued job being submitted again on each pass, and the silence of a host being
/// acted on or counted more than once: a job that waits for a host is read each pass and
/// submitted once; a placed job whose host has missed its grace is counted once, whatever the
/// number of passes, and its task is left as it was (the service ends the job as lost if the
/// host does not come back).
#[tokio::test(start_paused = true)]
async fn a_queued_job_is_not_resubmitted_and_host_silence_is_counted_once() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let lease = LeaseId::new(1, 1);
    tell_start(&messages, start(1, 1));
    for _ in 0..3 {
        step(&harness, &mut worker).await;
    }
    assert_eq!(submitted_specs(&harness).len(), 1);
    assert_eq!(harness.fake.jobs()[0].state(), JobState::Queued);
    let Some(Stage::Running(running)) = worker.tasks.get(&lease).map(|task| &task.stage) else {
        panic!("the task is not running");
    };
    let id = running.job.clone();
    assert_eq!(counted(&harness, "kbf_executor_host_silence_seen_total"), 0);

    assert!(harness.fake.place(&id, HOST));
    assert!(harness.fake.silence_host(HOST));
    let before = harness.fake.calls().len();
    for _ in 0..2 {
        step(&harness, &mut worker).await;
    }
    assert_eq!(counted(&harness, "kbf_executor_host_silence_seen_total"), 1);
    let Some(Stage::Running(running)) = worker.tasks.get(&lease).map(|task| &task.stage) else {
        panic!("the task is not running");
    };
    assert_eq!(running.job, id);
    assert!(
        calls_since(&harness, before)
            .iter()
            .all(|call| matches!(call, Call::GetJob(_)))
    );
    assert_eq!(harness.fake.jobs()[0].state(), JobState::Placed);
}

/// Catches a `Start` whose window ended after a submit whose answer was lost being forgotten
/// with its job left on the service (it would run for nothing), or forgotten before the
/// service answered the cancel: the job that may exist is cancelled, the task stays until the
/// cancel is answered, and the late `Start` is counted.
#[tokio::test(start_paused = true)]
async fn a_start_whose_window_ends_after_a_lost_submit_is_withdrawn() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let lease = LeaseId::new(1, 1);
    let id = job_id("front", lease, 0);
    harness.fake.lose_next_answer();
    tell_start(&messages, start(1, 1));
    step(&harness, &mut worker).await;
    assert!(matches!(worker.tasks[&lease].stage, Stage::Waiting(_)));
    assert_eq!(counted(&harness, "kbf_executor_late_starts_total"), 0);

    // Past the `Start`'s window, and short of the fence.
    tokio::time::advance(Duration::from_secs(15)).await;
    let before = harness.fake.calls().len();
    harness.fake.lose_next_answer();
    step(&harness, &mut worker).await;
    assert_eq!(counted(&harness, "kbf_executor_late_starts_total"), 1);
    assert_eq!(calls_since(&harness, before), [cancel_of(&id)]);
    let Some(Stage::Withdrawing(job)) = worker.tasks.get(&lease).map(|task| &task.stage) else {
        panic!("the task is not withdrawing");
    };
    assert_eq!(*job, id);

    step(&harness, &mut worker).await;
    assert_eq!(
        calls_since(&harness, before),
        [cancel_of(&id), cancel_of(&id)]
    );
    assert!(worker.tasks.is_empty());
    assert_eq!(submitted_specs(&harness).len(), 1);
    assert_eq!(harness.fake.jobs()[0].state(), JobState::Cancelled);
    assert_eq!(counted(&harness, "kbf_executor_late_starts_total"), 1);
}

/// A status of `code` for the next call of `method`.
fn fail_next(harness: &Harness, method: &str, code: Code) {
    let message = "injected".to_owned();
    harness
        .fake
        .inject_on(method, Fault::Status { code, message });
}

/// The job of the task of `lease`, which must be running.
fn job_of(worker: &HostWorker, lease: LeaseId) -> String {
    let Some(Stage::Running(running)) = worker.tasks.get(&lease).map(|task| &task.stage) else {
        panic!("the task of {lease} is not running");
    };
    running.job.clone()
}

/// Catches a result that could not be read being taken as no result, and a job the service
/// forgot between its end and the read of its result being left for ever: a read whose
/// answer is lost leaves the task running, to be read again on the next pass, and one the
/// service answers `NOT_FOUND` is the job lost to a restart of the service, rerun.
#[tokio::test(start_paused = true)]
async fn a_result_that_cannot_be_read_is_read_again_and_one_forgotten_is_lost() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let lease = LeaseId::new(1, 1);
    let id = running_job(&harness, &mut worker, &messages).await;
    assert!(harness.fake.finish(&id, 1));
    fail_next(&harness, "GetResult", Code::Unavailable);
    step(&harness, &mut worker).await;
    assert_eq!(job_of(&worker, lease), id, "the task is kept");
    step(&harness, &mut worker).await;
    let (code, message) = status_of(reported(&worker, lease));
    assert_eq!(code, Code::Unavailable as i32);
    assert_eq!(message, "the task tool exited nonzero (exit code Some(1))");

    let other = LeaseId::new(1, 2);
    tell_start(&messages, start(1, 2));
    step(&harness, &mut worker).await;
    let id = job_of(&worker, other);
    assert!(harness.fake.place(&id, HOST));
    assert!(harness.fake.starting(&id));
    assert!(harness.fake.run(&id));
    assert!(harness.fake.finish(&id, 0));
    fail_next(&harness, "GetResult", Code::NotFound);
    step(&harness, &mut worker).await;
    let result = reported(&worker, other);
    assert_eq!(result.lost, Lost::DaemonRestarted as i32);
    assert_eq!(status_of(result).0, Code::Unavailable as i32);
}

/// Catches a cancel being taken as done when the service would not serve it (a token it
/// refuses), and one the service does not know being asked for ever: the first leaves the
/// task withdrawing, to be cancelled again on the next pass, and the second drops the task,
/// since a job the service does not have needs no cancel.
#[tokio::test(start_paused = true)]
async fn a_cancel_the_service_will_not_serve_is_sent_again_and_one_it_does_not_know_is_done() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let lease = LeaseId::new(1, 1);
    let id = running_job(&harness, &mut worker, &messages).await;
    tell_cancel(&messages, 1, 1);
    fail_next(&harness, "CancelJob", Code::Unauthenticated);
    step(&harness, &mut worker).await;
    let Some(Stage::Withdrawing(job)) = worker.tasks.get(&lease).map(|task| &task.stage) else {
        panic!("the task is not withdrawing");
    };
    assert_eq!(*job, id);
    step(&harness, &mut worker).await;
    assert!(worker.tasks.is_empty());
    assert_eq!(counted(&harness, "kbf_executor_jobs_cancelled_total"), 1);

    let other = LeaseId::new(1, 2);
    tell_start(&messages, start(1, 2));
    step(&harness, &mut worker).await;
    let before = harness.fake.calls().len();
    tell_cancel(&messages, 1, 2);
    fail_next(&harness, "CancelJob", Code::NotFound);
    step(&harness, &mut worker).await;
    assert_eq!(calls_since(&harness, before).len(), 1);
    assert!(!worker.tasks.contains_key(&other));
    assert_eq!(counted(&harness, "kbf_executor_jobs_cancelled_total"), 1);
}

/// Catches a pass that reads every job however long the service takes: a service that holds
/// each read to the call's bound would hold the pass for a bound per job, past the fence and
/// the farm's grace. The reads stop once the pass budget is spent, and the next pass begins
/// with the job after the last one read, so none is left unread.
#[tokio::test(start_paused = true)]
async fn a_pass_reads_no_more_jobs_once_its_budget_is_spent() {
    let mut harness = Harness::new().await;
    harness.cfg.pass_budget = Duration::from_secs(1);
    let (mut worker, messages) = harness.worker();
    for seq in 1..=3 {
        tell_start(&messages, start(1, seq));
    }
    step(&harness, &mut worker).await;
    let ids: Vec<String> = (1..=3)
        .map(|seq| job_of(&worker, LeaseId::new(1, seq)))
        .collect();
    let read = |calls: Vec<Call>| -> Vec<String> {
        calls
            .into_iter()
            .filter_map(|call| match call {
                Call::GetJob(request) => Some(request.job_id),
                _ => None,
            })
            .collect()
    };

    harness.fake.inject_on("GetJob", Fault::Hang);
    let before = harness.fake.calls().len();
    let passing = async {
        harness.fake.hung().await;
        tokio::time::advance(harness.cfg.call_timeout + Duration::from_secs(1)).await;
    };
    tokio::join!(step(&harness, &mut worker), passing);
    assert_eq!(read(calls_since(&harness, before)), [ids[0].clone()]);

    let before = harness.fake.calls().len();
    step(&harness, &mut worker).await;
    assert_eq!(
        read(calls_since(&harness, before)),
        [ids[1].clone(), ids[2].clone(), ids[0].clone()]
    );
}

/// The output attached to a result's status (`executor::output`), as its fields; `None` when
/// none is.
fn output_of(result: &kbf_proto::worker::Result) -> Option<BTreeMap<String, String>> {
    let status = result.status.as_ref()?;
    let detail = status.details.iter().find(|detail| {
        detail.type_url == "type.googleapis.com/google.rpc.ErrorInfo"
            && ErrorInfo::decode(detail.value.as_slice())
                .is_ok_and(|info| info.reason == "JOB_OUTPUT_TAIL")
    })?;
    let info = ErrorInfo::decode(detail.value.as_slice()).expect("an ErrorInfo");
    Some(info.metadata.into_iter().collect())
}

fn fields(stdout: &str, stderr: &str, truncated: bool) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("stdout".to_owned(), stdout.to_owned()),
        ("stderr".to_owned(), stderr.to_owned()),
        ("truncated".to_owned(), truncated.to_string()),
    ])
}

fn logs_calls(calls: &[Call]) -> usize {
    calls
        .iter()
        .filter(|call| matches!(call, Call::GetLogs(_)))
        .count()
}

fn result_calls(calls: &[Call]) -> usize {
    calls
        .iter()
        .filter(|call| matches!(call, Call::GetResult(_)))
        .count()
}

/// The value after `flag` in the arguments the job `id` was submitted with.
fn arg_of(harness: &Harness, id: &str, flag: &str) -> String {
    let spec = harness.fake.submitted(id).expect("the job was submitted");
    let at = spec
        .args
        .iter()
        .position(|arg| arg == flag)
        .expect("the flag");
    spec.args[at + 1].clone()
}

/// Catches a failed job's output not reaching its result, or reaching the log: the tails
/// `GetResult` answered go with the result as its stdout and stderr, beside the unchanged
/// reason, the log holds their lengths only, and no `GetLogs` is made for output already in
/// hand.
#[tokio::test(start_paused = true)]
async fn a_failed_job_s_output_goes_with_its_result() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    assert!(
        harness
            .fake
            .write_logs(&id, b"compiling\n", b"error: disk full\n")
    );
    assert!(harness.fake.finish(&id, 1));
    let before = harness.fake.calls().len();
    let log = Log::default();
    let guard = log.install();
    step(&harness, &mut worker).await;
    drop(guard);
    let text = log.text();
    assert!(
        text.contains("stdout_bytes=10 stderr_bytes=17 truncated=false unread=false"),
        "{text}"
    );
    assert!(
        !text.contains("compiling") && !text.contains("disk full"),
        "{text}"
    );
    let result = reported(&worker, LeaseId::new(1, 1));
    assert_eq!(
        status_of(result),
        (
            Code::Unavailable as i32,
            "the task tool exited nonzero (exit code Some(1))"
        )
    );
    assert_eq!(
        output_of(result),
        Some(fields("compiling\n", "error: disk full\n", false))
    );
    let calls = calls_since(&harness, before);
    assert_eq!((result_calls(&calls), logs_calls(&calls)), (1, 0));
}

/// Catches a grant of the job reaching its attached output: the pack url and the read token
/// the job was handed, written at the start, in the middle, and where the cap cuts, are each
/// replaced, and no piece of either longer than the marker is left.
#[tokio::test(start_paused = true)]
async fn the_grants_of_a_job_never_appear_in_its_output() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    let url = arg_of(&harness, &id, "--pack-url");
    let token = arg_of(&harness, &id, "--grant-token");
    // stdout over half the cap, so each stream keeps its last 4096 bytes; redacted, stderr
    // is 16 bytes over that, which cuts five bytes into the marker of its url.
    let stdout = format!("{url} {token}{} {url}", "o".repeat(4096));
    let stderr = format!("{token}-{url}{}{token}-mid-{token}", "e".repeat(4066));
    assert!(
        harness
            .fake
            .write_logs(&id, stdout.as_bytes(), stderr.as_bytes())
    );
    assert!(harness.fake.finish(&id, 1));
    step(&harness, &mut worker).await;
    let result = reported(&worker, LeaseId::new(1, 1));
    let output = output_of(result).expect("an output");
    let window = "[redacted]".len() + 1;
    for secret in [&url, &token] {
        for text in output.values().chain([&status_of(result).1.to_owned()]) {
            assert!(!text.contains(secret.as_str()));
            for at in 0..=secret.len().saturating_sub(window) {
                let piece = &secret[at..(at + window).min(secret.len())];
                assert!(!text.contains(piece), "{piece:?} is in the output");
            }
        }
    }
    assert!(output["stderr"].starts_with("cted]e"));
    assert!(output["stderr"].ends_with("e[redacted]-mid-[redacted]"));
    assert!(output["stdout"].ends_with("o [redacted]"));
    assert_eq!(output["truncated"], "true");
}

/// Catches output over the cap being attached whole or by its head, and a cut inside a
/// character: each stream keeps its end, the two within 8 KiB, on a character boundary,
/// marked truncated.
#[tokio::test(start_paused = true)]
async fn output_over_the_cap_is_its_tail_and_marked_truncated() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    let stdout = format!("{}x", "é".repeat(3000));
    assert!(
        harness
            .fake
            .write_logs(&id, stdout.as_bytes(), "e".repeat(6000).as_bytes())
    );
    assert!(harness.fake.finish(&id, 1));
    step(&harness, &mut worker).await;
    let output = output_of(reported(&worker, LeaseId::new(1, 1))).expect("an output");
    let expected = fields(&format!("{}x", "é".repeat(2047)), &"e".repeat(4096), true);
    assert_eq!(output, expected);
    assert!(output["stdout"].len() + output["stderr"].len() <= 8 << 10);
}

/// Catches the fallback not being made when the result holds no output: one `GetLogs` is,
/// and its tails are attached.
#[tokio::test(start_paused = true)]
async fn a_result_without_output_falls_back_to_one_read_of_the_logs() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    assert!(harness.fake.write_logs(&id, b"from the logs\n", b""));
    assert!(harness.fake.finish(&id, 1));
    let bare = GetResultResponse {
        result: Some(JobResult {
            state: JobState::Failed.into(),
            exit_code: Some(1),
            reason: ResultReason::Exit.into(),
            ..JobResult::default()
        }),
    };
    harness
        .fake
        .inject_on("GetResult", Fault::Answer(bare.encode_to_vec().into()));
    let before = harness.fake.calls().len();
    step(&harness, &mut worker).await;
    let result = reported(&worker, LeaseId::new(1, 1));
    assert_eq!(
        output_of(result),
        Some(fields("from the logs\n", "", false))
    );
    assert_eq!(logs_calls(&calls_since(&harness, before)), 1);
}

/// Catches a failed read of the output replacing the failure's reason or going unsaid: with
/// `GetResult` and `GetLogs` both failing, the result is the lost job it was, and its output
/// is a note of why it could not be read.
#[tokio::test(start_paused = true)]
async fn when_no_output_can_be_read_the_reason_stays_and_a_note_says_so() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    assert!(harness.fake.finish(&id, 1));
    fail_next(&harness, "GetResult", Code::NotFound);
    fail_next(&harness, "GetLogs", Code::PermissionDenied);
    let before = harness.fake.calls().len();
    step(&harness, &mut worker).await;
    let result = reported(&worker, LeaseId::new(1, 1));
    assert_eq!(result.lost, Lost::DaemonRestarted as i32);
    assert_eq!(
        status_of(result),
        (Code::Unavailable as i32, "the job was lost")
    );
    let mut expected = fields("", "", false);
    expected.insert(
        "note".to_owned(),
        "the job's output could not be read: the job service answered PermissionDenied".to_owned(),
    );
    assert_eq!(output_of(result), Some(expected));
    assert_eq!(logs_calls(&calls_since(&harness, before)), 1);
}

/// Catches a job that wrote nothing getting an empty detail: the logs are read once, and the
/// result is as it was.
#[tokio::test(start_paused = true)]
async fn a_job_that_wrote_nothing_has_no_output_attached() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    tell_start(&messages, start(1, 1));
    step(&harness, &mut worker).await;
    let id = job_of(&worker, LeaseId::new(1, 1));
    assert!(
        harness
            .fake
            .fail_to_start(&id, StartFailureCode::NoCapacity)
    );
    let before = harness.fake.calls().len();
    step(&harness, &mut worker).await;
    let result = reported(&worker, LeaseId::new(1, 1));
    assert_eq!(output_of(result), None);
    assert!(result.status.as_ref().is_some_and(|s| s.details.is_empty()));
    assert_eq!(logs_calls(&calls_since(&harness, before)), 1);
}

/// Catches the success path paying for the failure's output: a job that ended well is read
/// with its one `GetResult`, as before, and `GetLogs` is never called.
#[tokio::test(start_paused = true)]
async fn a_job_that_ended_well_makes_no_call_for_its_output() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    assert!(harness.fake.write_logs(&id, b"all good\n", b""));
    assert!(harness.fake.finish(&id, 0));
    let before = harness.fake.calls().len();
    step(&harness, &mut worker).await;
    assert!(matches!(
        worker.tasks[&LeaseId::new(1, 1)].stage,
        Stage::Ingesting(_)
    ));
    let calls = calls_since(&harness, before);
    assert_eq!((result_calls(&calls), logs_calls(&calls)), (1, 0));
}

#[path = "runs_submit_tests.rs"]
mod submit;
