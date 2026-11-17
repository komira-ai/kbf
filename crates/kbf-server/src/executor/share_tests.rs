//! The rows path of a worker under shared ownership, over the in-memory store.

use std::time::Duration;

use js_proto::v1::{CancelJobRequest, JobState};
use kbf_job_client::fake::{Call, Fault};
use kbf_proto::worker::{ResultAck, ServerMessage, Start, server_message};
use kbf_state::Store;
use kbf_types::LeaseId;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;
use tonic::Code;

use super::Own;
use crate::executor::host::HostWorker;
use crate::executor::rows::{ExecRows, Held, JobEntry, JobRow, Shape, WorkerRow};
use crate::executor::test_support::{HOST, Harness, INSTANCE, Log, start};
use crate::executor::{PackKey, build_id, job_id};

type Messages = UnboundedSender<Result<ServerMessage, tonic::Status>>;

fn lease() -> LeaseId {
    LeaseId::new(1, 1)
}

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
        server_message::Message::Cancel(kbf_proto::worker::Cancel { lease_id }),
    );
}

fn tell_ack(messages: &Messages, term: u64, seq: u64) {
    let lease_id = Some(kbf_proto::worker::LeaseId { term, seq });
    let ack = ResultAck {
        lease_id,
        accepted: true,
        reason: 0,
    };
    tell(messages, server_message::Message::ResultAck(ack));
}

/// The farm ends the stream with a status, as a handover does.
fn end_stream(messages: &Messages) {
    messages
        .send(Err(tonic::Status::unavailable("the farm moved")))
        .expect("sent");
}

async fn step(harness: &Harness, worker: &mut HostWorker) {
    worker.step(&harness.ctx(), Instant::now()).await;
}

/// The name of the first worker of the shared process.
fn name(harness: &Harness) -> String {
    harness.cfg.worker_name(0)
}

/// The job rows under the first worker, as another process of the group reads them.
fn job_rows(harness: &Harness) -> Vec<JobEntry> {
    harness
        .rig()
        .rows()
        .jobs(&name(harness))
        .expect("the job rows")
}

fn held(worker: &HostWorker) -> Held {
    worker.held().expect("the worker has a row")
}

/// Another process of the group writes the worker row: the holder's version is stale for good.
fn take_over(harness: &Harness, worker: &HostWorker) {
    let own = worker.own.as_ref().expect("a row");
    harness
        .rig()
        .rows()
        .release(&own.held, &own.row)
        .expect("released");
}

fn submits(harness: &Harness) -> usize {
    harness
        .fake
        .calls()
        .iter()
        .filter(|call| matches!(call, Call::SubmitJob(_)))
        .count()
}

fn cancels(harness: &Harness) -> Vec<String> {
    harness
        .fake
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::CancelJob(CancelJobRequest { job_id }) => Some(job_id),
            _ => None,
        })
        .collect()
}

fn txns(harness: &Harness) -> u64 {
    harness.rig().fake.commit_stats().txns
}

/// The id of the job whose row the task of `lease` wrote.
fn row_of(worker: &HostWorker, lease: LeaseId) -> Option<String> {
    worker.tasks.get(&lease).and_then(|task| task.row.clone())
}

/// Starts lease 1.1 and steps once: its job is on the service, placed, starting and running.
/// The job id.
async fn running_job(harness: &Harness, worker: &mut HostWorker, messages: &Messages) -> String {
    tell_start(messages, start(1, 1));
    step(harness, worker).await;
    let id = harness.fake.jobs()[0].id.clone();
    assert!(harness.fake.place(&id, HOST));
    assert!(harness.fake.starting(&id));
    assert!(harness.fake.run(&id));
    id
}

fn unavailable(harness: &Harness, method: &str) {
    let fault = Fault::Status {
        code: Code::Unavailable,
        message: "injected".to_owned(),
    };
    harness.fake.inject_on(method, fault);
}

fn worker_row(owner: &str, farm_instance: &str) -> WorkerRow {
    WorkerRow {
        owner_instance: owner.to_owned(),
        farm_instance: farm_instance.to_owned(),
        shape: Shape {
            cpu_millis: 1,
            memory_bytes: 1,
            slots: 1,
            arch: "x86_64".to_owned(),
        },
    }
}

fn liveness(rows: &ExecRows) -> crate::executor::rows::Liveness {
    rows.liveness(INSTANCE).expect("read").expect("a row")
}

/// Catches a row written after the submit, or one that does not say what the job is: when the
/// job service has not taken the submit, the job row is already in the store, with the
/// lease's term and seq, the action, and the key of the pack, and no job exists. The next pass
/// writes no second row and the job it submits is the one the row names.
#[tokio::test(start_paused = true)]
async fn a_start_writes_the_row_before_the_submit() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    unavailable(&harness, "SubmitJob");
    tell_start(&messages, start(1, 1));
    step(&harness, &mut worker).await;
    assert!(harness.fake.jobs().is_empty(), "no job was made");
    let rows = job_rows(&harness);
    assert_eq!(rows.len(), 1);
    let row = &rows[0].row;
    assert_eq!((row.lease_term, row.lease_seq), (1, 1));
    assert_eq!(row.action, format!("{}/3", "cd".repeat(32)));
    let key = PackKey::new(format!("cas/b/{}/aa-0", build_id(lease())));
    assert_eq!(row.pack_key, key.to_string());
    assert_eq!(
        row_of(&worker, lease()).as_deref(),
        Some(rows[0].job.as_str())
    );

    let written = txns(&harness);
    step(&harness, &mut worker).await;
    assert_eq!(txns(&harness), written, "the row was written again");
    assert_eq!(harness.fake.jobs().len(), 1);
    assert_eq!(harness.fake.jobs()[0].id, rows[0].job);
}

/// Catches a row that is not taken as written when an earlier try left it: the store holds the
/// row of the job already (an insert whose answer was lost) and the worker still holds its
/// row, so the start goes on to its submit and the task knows its row, and the row is not
/// rewritten.
#[tokio::test(start_paused = true)]
async fn a_row_an_earlier_try_left_is_taken_as_written() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    let id = job_id("front", lease(), 0);
    let left = JobRow {
        lease_term: 1,
        lease_seq: 1,
        action: "left".to_owned(),
        pack_key: "left".to_owned(),
        submitted_ms: 1,
    };
    harness
        .rig()
        .rows()
        .insert_job(&held(&worker), &id, &left)
        .expect("inserted");
    tell_start(&messages, start(1, 1));
    step(&harness, &mut worker).await;
    assert_eq!(row_of(&worker, lease()), Some(id.clone()));
    assert_eq!(harness.fake.jobs()[0].id, id);
    assert_eq!(job_rows(&harness)[0].row, left, "the row was rewritten");
}

/// Catches a row deleted before the farm has the result, or kept after it: a job that ended
/// and whose result is reported keeps its row until the `ResultAck`, and the pass that takes
/// the `ResultAck` deletes it.
#[tokio::test(start_paused = true)]
async fn the_row_is_deleted_after_the_ack_not_before() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    assert!(harness.fake.finish(&id, 1));
    step(&harness, &mut worker).await;
    assert!(worker.tasks.contains_key(&lease()), "the result is owed");
    assert_eq!(job_rows(&harness).len(), 1, "deleted before the ack");
    step(&harness, &mut worker).await;
    assert!(worker.tasks.is_empty(), "the farm acknowledged the result");
    assert!(job_rows(&harness).is_empty());
    assert!(worker.unrow.is_empty());
}

/// Catches a fence that cancels: with shared ownership a worker the farm has not acknowledged
/// for the fence time drops its tasks and falls silent, and the job keeps running on the
/// service (no `CancelJob` is sent), with its row in the store for the process that takes the
/// worker up.
#[tokio::test(start_paused = true)]
async fn a_fenced_worker_cancels_nothing() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    tokio::time::advance(harness.cfg.fence_after + Duration::from_secs(1)).await;
    harness.renew(&[held(&worker)], false);
    let log = Log::default();
    let guard = log.install();
    step(&harness, &mut worker).await;
    drop(guard);
    assert!(worker.info().silent);
    assert!(worker.tasks.is_empty());
    assert!(cancels(&harness).is_empty());
    assert_ne!(harness.fake.jobs()[0].state(), JobState::Cancelled);
    assert_eq!(job_rows(&harness)[0].job, id);
    let text = log.text();
    assert!(text.contains("without a cancel"), "{text}");
}

/// Catches a fence that counts only the farm and the job service: a process whose liveness
/// renewals the store does not take, while the farm acknowledges every heartbeat, still fences
/// once the fence time has passed since the last renewal the store took, and not before, and
/// cancels nothing.
#[tokio::test(start_paused = true)]
async fn the_stores_silence_fences_too() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    running_job(&harness, &mut worker, &messages).await;
    harness.rig().flaky.fail_writes(true);
    let log = Log::default();
    let guard = log.install();
    let (beat, fence) = (harness.cfg.heartbeat, harness.cfg.fence_after);
    let mut advanced = Duration::ZERO;
    for _ in 0..64 {
        tokio::time::advance(beat).await;
        advanced += beat;
        harness.renew(&[held(&worker)], false);
        step(&harness, &mut worker).await;
        assert!(
            worker.info().silent || advanced <= fence,
            "not fenced at {advanced:?}"
        );
        if worker.info().silent {
            break;
        }
    }
    drop(guard);
    assert!(worker.info().silent);
    assert!(advanced > fence && advanced <= fence + beat, "{advanced:?}");
    assert!(cancels(&harness).is_empty());
    assert_eq!(job_rows(&harness).len(), 1);
    let text = log.text();
    assert!(text.contains("the liveness row was not renewed"), "{text}");
}

/// Catches a submit sent over a row the store did not take: while the store fails, nothing is
/// submitted and no row exists; once it takes writes the same start is submitted, with its row.
#[tokio::test(start_paused = true)]
async fn a_row_the_store_cannot_take_is_retried_before_the_submit() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    harness.rig().flaky.fail_writes(true);
    tell_start(&messages, start(1, 1));
    let log = Log::default();
    let guard = log.install();
    step(&harness, &mut worker).await;
    drop(guard);
    let text = log.text();
    assert!(text.contains("the row of a job was not written"), "{text}");
    assert_eq!(submits(&harness), 0);
    assert!(worker.tasks.contains_key(&lease()));
    assert!(row_of(&worker, lease()).is_none());
    harness.rig().flaky.fail_writes(false);
    step(&harness, &mut worker).await;
    assert_eq!(submits(&harness), 1);
    assert_eq!(job_rows(&harness).len(), 1);
    assert_eq!(
        row_of(&worker, lease()),
        Some(harness.fake.jobs()[0].id.clone())
    );
}

/// Catches a worker that submits after another process took its row: the job row insert
/// fails on the worker row, the task is dropped, nothing is submitted, and no job row is
/// written.
#[tokio::test(start_paused = true)]
async fn a_worker_row_another_process_wrote_drops_the_task_unsubmitted() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    take_over(&harness, &worker);
    tell_start(&messages, start(1, 1));
    let log = Log::default();
    let guard = log.install();
    step(&harness, &mut worker).await;
    drop(guard);
    let text = log.text();
    assert!(text.contains("another process holds the worker"), "{text}");
    assert!(worker.tasks.is_empty());
    assert_eq!(submits(&harness), 0);
    assert!(job_rows(&harness).is_empty());
}

/// Catches a delete lost for good when the store failed once: the job row stays and the job's
/// id stays queued, the next pass that the store takes deletes it, and the queue is empty.
#[tokio::test(start_paused = true)]
async fn a_delete_the_store_cannot_take_is_retried_on_the_next_pass() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    assert!(harness.fake.finish(&id, 1));
    step(&harness, &mut worker).await;
    harness.rig().flaky.fail_writes(true);
    tell_ack(&messages, 1, 1);
    let log = Log::default();
    let guard = log.install();
    step(&harness, &mut worker).await;
    drop(guard);
    assert!(worker.tasks.is_empty());
    assert_eq!(job_rows(&harness).len(), 1);
    assert_eq!(worker.unrow, [id]);
    assert!(log.text().contains("the row of a job was not deleted"));
    harness.rig().flaky.fail_writes(false);
    step(&harness, &mut worker).await;
    assert!(job_rows(&harness).is_empty());
    assert!(worker.unrow.is_empty());
}

/// Catches a delete retried for ever against a worker another process holds: the row is that
/// process's to settle, so the delete is dropped, and the row stays.
#[tokio::test(start_paused = true)]
async fn a_delete_refused_by_a_worker_row_conflict_is_dropped() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    assert!(harness.fake.finish(&id, 1));
    step(&harness, &mut worker).await;
    take_over(&harness, &worker);
    tell_ack(&messages, 1, 1);
    let log = Log::default();
    let guard = log.install();
    step(&harness, &mut worker).await;
    drop(guard);
    assert!(worker.unrow.is_empty());
    assert_eq!(job_rows(&harness).len(), 1, "another process's row");
    let text = log.text();
    assert!(text.contains("the row of the job is its own"), "{text}");
}

/// Catches a row left behind by a withdrawn task, or deleted before the service answered the
/// cancel: the farm's `Cancel` of a running job keeps its row while the cancel is unanswered
/// and deletes it on the pass the cancel is answered.
#[tokio::test(start_paused = true)]
async fn a_withdrawn_task_deletes_its_row_once_the_cancel_is_answered() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    tell_cancel(&messages, 1, 1);
    harness.fake.lose_next_answer();
    step(&harness, &mut worker).await;
    assert!(worker.tasks.contains_key(&lease()), "still withdrawing");
    assert_eq!(job_rows(&harness).len(), 1);
    step(&harness, &mut worker).await;
    assert!(worker.tasks.is_empty());
    assert!(job_rows(&harness).is_empty());
    assert_eq!(cancels(&harness), [id.clone(), id]);
}

/// Catches a store write for a task that never had a row: a `Start` cancelled before its
/// submit is dropped with no call to the job service and no transaction in the store.
#[tokio::test(start_paused = true)]
async fn a_task_dropped_before_its_submit_writes_nothing() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    let written = txns(&harness);
    tell_start(&messages, start(1, 1));
    tell_cancel(&messages, 1, 1);
    step(&harness, &mut worker).await;
    assert!(worker.tasks.is_empty());
    assert!(harness.fake.calls().is_empty());
    assert_eq!(txns(&harness), written);
    assert!(job_rows(&harness).is_empty());
}

/// Catches a reconnect that leaves rows or the old farm instance: the farm ends the stream, the
/// jobs are cancelled and their rows deleted, the worker row names the instance of the new
/// registration, and the holder holds the rewritten row (the next start is submitted, which
/// its row insert checks).
#[tokio::test(start_paused = true)]
async fn a_reconnect_cancels_the_jobs_deletes_their_rows_and_renames_the_worker() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    end_stream(&messages);
    step(&harness, &mut worker).await;
    assert_eq!(cancels(&harness), [id]);
    assert!(job_rows(&harness).is_empty());
    let entries = harness.rig().rows().workers().expect("workers");
    assert_eq!(entries.len(), 1);
    let new = HostWorker::farm_instance(&harness.ctx(), &name(&harness), 1);
    assert_eq!(entries[0].row.farm_instance, new);
    assert_eq!(entries[0].row.owner_instance, INSTANCE);

    let (messages, inbox) = tokio::sync::mpsc::unbounded_channel();
    worker.replace_inbox(inbox);
    tell_start(&messages, start(1, 2));
    step(&harness, &mut worker).await;
    assert_eq!(harness.fake.jobs().len(), 2);
    assert_eq!(job_rows(&harness)[0].row.lease_seq, 2);
}

/// Catches a reconnect that deletes the row of a job it could not cancel: the job service does
/// not answer the cancel, so the job may still run, and its row stays after the passes that
/// follow.
#[tokio::test(start_paused = true)]
async fn a_reconnect_whose_cancel_is_not_answered_keeps_the_jobs_row() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    unavailable(&harness, "CancelJob");
    end_stream(&messages);
    step(&harness, &mut worker).await;
    step(&harness, &mut worker).await;
    let rows = job_rows(&harness);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].job, id);
    assert!(harness.fake.jobs().iter().any(|job| job.id == id));
}

/// Catches a reconnect that gives up the worker when its row cannot be rewritten: the row keeps
/// the old farm instance, and the holder still holds it (a start is submitted afterwards).
#[tokio::test(start_paused = true)]
async fn a_reconnect_the_store_cannot_record_keeps_the_row() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    let before = held(&worker);
    harness.rig().flaky.fail_writes(true);
    end_stream(&messages);
    let log = Log::default();
    let guard = log.install();
    step(&harness, &mut worker).await;
    drop(guard);
    let text = log.text();
    assert!(
        text.contains("registered again was not rewritten"),
        "{text}"
    );
    assert_eq!(held(&worker), before);
    let old = HostWorker::farm_instance(&harness.ctx(), &name(&harness), 0);
    let entries = harness.rig().rows().workers().expect("workers");
    assert_eq!(entries[0].row.farm_instance, old);

    harness.rig().flaky.fail_writes(false);
    let (messages, inbox) = tokio::sync::mpsc::unbounded_channel();
    worker.replace_inbox(inbox);
    tell_start(&messages, start(1, 1));
    step(&harness, &mut worker).await;
    assert_eq!(harness.fake.jobs().len(), 1);
}

/// Catches a rename made by a process that has no liveness row: the worker row is not written
/// and the holder keeps what it had.
#[tokio::test(start_paused = true)]
async fn a_rename_before_the_first_renewal_writes_nothing() {
    let harness = Harness::shared().await;
    let rows = harness.rig().rows();
    let row = worker_row(INSTANCE, "old");
    let held = rows
        .create_worker("js:front.inst0001-0", &row)
        .expect("a row");
    let mut worker = HostWorker::start(&harness.ctx(), 0, Instant::now());
    worker.own = Some(Own {
        held: held.clone(),
        row,
    });
    let written = txns(&harness);
    worker.renamed(&harness.ctx(), "new");
    assert_eq!(txns(&harness), written);
    let own = worker.own.as_ref().expect("a row");
    assert_eq!((&own.held, own.row.farm_instance.as_str()), (&held, "old"));
}

/// Catches a worker opened before its process is known alive: nothing is opened and no row is
/// written until the liveness row is, then every worker the config asks for is opened, each
/// with its row naming the process, its farm instance and its shape, and a pass after that
/// opens no more.
#[tokio::test(start_paused = true)]
async fn nothing_is_opened_before_the_liveness_row_is_written() {
    let mut harness = Harness::shared().await;
    harness.cfg.hosts = 2;
    let mut workers = Vec::new();
    harness.open(&mut workers);
    assert!(workers.is_empty());
    assert!(harness.rig().rows().workers().expect("workers").is_empty());
    harness.renew(&[], false);
    harness.open(&mut workers);
    assert_eq!(workers.len(), 2);
    let entries = harness.rig().rows().workers().expect("workers");
    let names: Vec<_> = entries.iter().map(|e| e.worker.as_str()).collect();
    assert_eq!(names, ["js:front.inst0001-0", "js:front.inst0001-1"]);
    let row = &entries[1].row;
    assert_eq!(row.owner_instance, INSTANCE);
    assert_eq!(
        row.farm_instance,
        HostWorker::farm_instance(&harness.ctx(), "js:front.inst0001-1", 0)
    );
    let shape = (
        row.shape.cpu_millis,
        row.shape.memory_bytes,
        row.shape.slots,
    );
    let asked = (
        harness.cfg.cpu_millis,
        harness.cfg.memory_bytes,
        harness.cfg.slots,
    );
    assert_eq!(shape, asked);
    assert_eq!(row.shape.arch, harness.cfg.arch);
    let written = txns(&harness);
    harness.open(&mut workers);
    assert_eq!(workers.len(), 2);
    assert_eq!(txns(&harness), written, "opened again");
}

/// Catches the Macs' workers opened without a row, under a name another process of the group
/// could take, or with the Linux arch: with one Linux worker and two Macs, the Linux one is
/// opened first and the Macs after it, each named by the instance under `js-mac:`, each with
/// its row (owned by this process, its farm instance, arch `arm64`), and a pass after that
/// opens no more.
#[tokio::test(start_paused = true)]
async fn the_macs_are_opened_after_the_linux_workers_each_with_its_row() {
    use crate::executor::mac::WorkerOs;
    let mut harness = Harness::shared().await;
    harness.cfg.hosts = 1;
    harness.cfg.mac = Some(crate::executor::MacPool {
        hosts: 2,
        bundle: format!("kbf-task@sha256:{}", "ab".repeat(32)),
        os_version: 26,
        roots: None,
    });
    harness.renew(&[], false);
    let mut workers = Vec::new();
    harness.open(&mut workers);
    let opened: Vec<_> = workers
        .iter()
        .map(|w| (w.info().worker.to_string(), w.os()))
        .collect();
    assert_eq!(
        opened,
        [
            ("js:front.inst0001-0".to_owned(), WorkerOs::Linux),
            ("js-mac:front.inst0001-0".to_owned(), WorkerOs::Mac),
            ("js-mac:front.inst0001-1".to_owned(), WorkerOs::Mac),
        ]
    );
    let entries = harness.rig().rows().workers().expect("workers");
    let mac = entries
        .iter()
        .find(|e| e.worker == "js-mac:front.inst0001-1")
        .expect("the row of the second Mac");
    assert_eq!(mac.row.owner_instance, INSTANCE);
    assert_eq!(
        mac.row.farm_instance,
        HostWorker::farm_instance(&harness.ctx(), "js-mac:front.inst0001-1", 0)
    );
    assert_eq!(mac.row.shape.arch, "arm64");
    let linux = entries
        .iter()
        .find(|e| e.worker == "js:front.inst0001-0")
        .expect("the row of the Linux worker");
    assert_eq!(linux.row.shape.arch, harness.cfg.arch);
    assert_eq!(entries.len(), 3);
    harness.open(&mut workers);
    assert_eq!(workers.len(), 3);
}

/// Catches a failed row write that loses the workers after it or registers the one it could not
/// record: with the row of the second worker held by another process, the first is opened, the
/// second is not registered, and a later pass opens the rest once the row is free.
#[tokio::test(start_paused = true)]
async fn a_worker_row_that_cannot_be_created_leaves_the_rest_for_a_later_pass() {
    let mut harness = Harness::shared().await;
    harness.cfg.hosts = 3;
    harness.renew(&[], false);
    let rows = harness.rig().rows();
    let blocker = rows
        .create_worker("js:front.inst0001-1", &worker_row("other", "x"))
        .expect("a row");
    let mut workers = Vec::new();
    let log = Log::default();
    let guard = log.install();
    harness.open(&mut workers);
    drop(guard);
    assert_eq!(workers.len(), 1);
    let text = log.text();
    assert!(text.contains("it is not registered yet"), "{text}");
    rows.retire(&blocker).expect("retired");
    harness.open(&mut workers);
    let ids: Vec<_> = workers.iter().map(|w| w.id.to_string()).collect();
    assert_eq!(
        ids,
        [
            "js:front.inst0001-0",
            "js:front.inst0001-1",
            "js:front.inst0001-2"
        ]
    );
}

/// Catches a worker row left behind by a process that ends with no job: it is deleted, with no
/// call to the job service.
#[tokio::test(start_paused = true)]
async fn a_worker_with_no_job_row_is_retired_at_the_end() {
    let mut harness = Harness::shared().await;
    let (worker, _messages) = harness.shared_worker();
    worker.hand_back(&harness.ctx());
    assert!(harness.rig().rows().workers().expect("workers").is_empty());
    assert!(harness.fake.calls().is_empty());
}

/// Catches a worker handed back with jobs going being cancelled or deleted: its row is released
/// (no owner), its job row stays, and the job is not cancelled.
#[tokio::test(start_paused = true)]
async fn a_worker_with_job_rows_is_released_at_the_end() {
    let mut harness = Harness::shared().await;
    let (mut worker, messages) = harness.shared_worker();
    running_job(&harness, &mut worker, &messages).await;
    worker.hand_back(&harness.ctx());
    let entries = harness.rig().rows().workers().expect("workers");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].row.owner_instance, "");
    assert_eq!(job_rows(&harness).len(), 1);
    assert!(cancels(&harness).is_empty());
}

/// Catches a hand-back that deletes or releases a row it could not check: when the job rows
/// cannot be read the worker row stays as it was, and the failure is logged.
#[tokio::test(start_paused = true)]
async fn job_rows_that_cannot_be_read_at_the_end_leave_the_worker_row() {
    let mut harness = Harness::shared().await;
    let (worker, _messages) = harness.shared_worker();
    harness.rig().flaky.fail_reads(true);
    let log = Log::default();
    let guard = log.install();
    worker.hand_back(&harness.ctx());
    drop(guard);
    harness.rig().flaky.fail_reads(false);
    let entries = harness.rig().rows().workers().expect("workers");
    assert_eq!(entries[0].row.owner_instance, INSTANCE);
    let text = log.text();
    assert!(text.contains("was not deleted or handed back"), "{text}");
}

/// Catches a hand-back that reports a delete the store did not take: when the retire cannot be
/// written the worker row stays as it was, and the failure is logged.
#[tokio::test(start_paused = true)]
async fn a_retire_the_store_cannot_take_leaves_the_worker_row() {
    let mut harness = Harness::shared().await;
    let (worker, _messages) = harness.shared_worker();
    harness.rig().flaky.fail_writes(true);
    let log = Log::default();
    let guard = log.install();
    worker.hand_back(&harness.ctx());
    drop(guard);
    harness.rig().flaky.fail_writes(false);
    let entries = harness.rig().rows().workers().expect("workers");
    assert_eq!(entries[0].row.owner_instance, INSTANCE);
    let text = log.text();
    assert!(text.contains("was not deleted or handed back"), "{text}");
}

/// Catches a leave that fails loudly or leaves the row when the store works: the liveness row
/// is deleted; when the store fails it stays, to expire, and the failure is logged.
#[tokio::test(start_paused = true)]
async fn leaving_deletes_the_liveness_row_and_a_failure_is_logged() {
    let mut harness = Harness::shared().await;
    harness.renew(&[], false);
    let rows = harness.rig().rows();
    assert!(rows.liveness(INSTANCE).expect("read").is_some());
    let share = harness.share.as_ref().expect("a share");
    let log = Log::default();
    let guard = log.install();
    harness.rig().flaky.fail_writes(true);
    share.leave();
    assert!(rows.liveness(INSTANCE).expect("read").is_some());
    harness.rig().flaky.fail_writes(false);
    share.leave();
    drop(guard);
    assert!(rows.liveness(INSTANCE).expect("read").is_none());
    let text = log.text();
    assert!(text.contains("it expires on its own"), "{text}");
}

/// Catches a renewal sent every pass, one that claims success it did not have, or one that does
/// not say the process drains: a renewal is due once each heartbeat; the row says whether the
/// process drains; a refused renewal (the store down, or a worker row another process wrote)
/// leaves the row and the time of the last renewal as they were.
#[tokio::test(start_paused = true)]
async fn a_renewal_is_due_once_a_heartbeat_and_a_refused_one_moves_nothing() {
    let mut harness = Harness::shared().await;
    let (worker, _messages) = harness.shared_worker();
    let rows = harness.rig().rows();
    let renewed = |harness: &Harness| harness.share.as_ref().expect("a share").renewed();
    assert_eq!(renewed(&harness), Instant::now());
    assert_eq!(liveness(&rows).group, "front");
    let written = txns(&harness);
    harness.renew(&[held(&worker)], true);
    assert_eq!(txns(&harness), written, "renewed before it was due");
    assert!(!liveness(&rows).draining);

    tokio::time::advance(harness.cfg.heartbeat).await;
    harness.renew(&[held(&worker)], true);
    assert_eq!(txns(&harness), written + 1);
    assert!(liveness(&rows).draining);
    let last = renewed(&harness);
    assert_eq!(last, Instant::now());

    tokio::time::advance(harness.cfg.heartbeat).await;
    harness.rig().flaky.fail_writes(true);
    harness.renew(&[held(&worker)], false);
    harness.rig().flaky.fail_writes(false);
    assert_eq!(renewed(&harness), last);
    assert!(liveness(&rows).draining);

    tokio::time::advance(harness.cfg.heartbeat).await;
    take_over(&harness, &worker);
    harness.renew(&[held(&worker)], false);
    assert_eq!(renewed(&harness), last);
    assert!(liveness(&rows).draining);
}

/// Catches shared-mode machinery run for a worker of a single frontend: a worker with no row
/// holds nothing, a reconnect writes no row, its queued deletes are discarded, and handing it
/// back makes no call to the job service.
#[tokio::test(start_paused = true)]
async fn a_worker_of_a_single_frontend_has_no_row() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    assert!(worker.held().is_none());
    worker.unrow.push("queued".to_owned());
    end_stream(&messages);
    step(&harness, &mut worker).await;
    assert!(worker.own.is_none());
    assert!(worker.unrow.is_empty());
    worker.hand_back(&harness.ctx());
    assert!(harness.fake.calls().is_empty());
}
