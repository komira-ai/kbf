//! The task of one operation: its runs, one job each, until one answers its callers.
//!
//! A run is prepared (the action read, its key minted and, for a put directory, its tree
//! named), its row written to the state store when there is one, and its job submitted; a
//! submit whose answer was lost is sent again, the same spec under the same id, every
//! [`Config::poll`](super::Config::poll), so the service makes one job.
//!
//! The tree's writes go on while the row is written ([`Ingest::stored`](super::Ingest::stored)):
//! its name is a digest of what it holds, known before any write. Every attempt waits for the
//! writes before it submits its job, since the job's host opens the tree as soon as the job is
//! placed and a tree not yet in the store fails the job's start; a failed write ends the run
//! with no job submitted, and a cancel then ends it with nothing to withdraw. The job is then read
//! back every poll until it ends ([`outcome::judge`](super::outcome)): a job that ended well has
//! its pack ingested and answers the callers with the result; an ending of the farm's submits the
//! operation's next attempt, while it has one ([`RESUBMITS`]); any other ending, and the last
//! attempt, answers them with its status, the job's output attached.
//!
//! A cancel asked for by a caller cancels the job (or, before the submit, stops the run) and
//! answers the callers `CANCELLED`.
//!
//! An operation taken up from the state store ([`Start::Attached`]) begins where its row says:
//! its job is read back as if this process had submitted it, and finished from its put
//! directory. A row that names no put directory has no pack to read, so its job is cancelled and
//! the operation run again as its next attempt.
//!
//! When the state store shows another process runs the operation now (a write of its row is
//! refused), this process stops running it, and its callers follow the answer that process
//! writes ([`follow`]). When the first attempt's row finds another frontend running an operation
//! of the same action ([`Rows::open_op`]), no job is submitted: the operation follows that one
//! and answers its callers with that answer, so one action runs once whichever frontend its
//! callers reached. If the operation followed was cancelled by its own callers, this one runs on
//! its own.

use std::future::pending;
use std::sync::Arc;

use js_proto::v1::{JobResult, JobSpec, JobState};
use kbf_front::{Finished, Stage};
use kbf_proto::google::rpc;
use kbf_proto::reapi;
use kbf_types::{ActionKey, OperationId};
use tokio::sync::watch;
use tokio::time::{Instant, sleep, timeout};
use tonic::Code;

use super::Inner;
use super::api::{answered, not_found, note};
use super::metrics::{IngestEnd, RunEnd};
use super::outcome::{RESUBMITS, Settled, Verdict, forgotten, judge, refused};
use super::output::{Output, Tail};
use super::phases::{Mark, Phase, Timeline};
use super::record;
use super::rows::{Look, OpRow, Opened, Rows, RowsError};
use super::steps::Step;
use super::submit::{Booking, Preparer, not_minted, prepare};
use super::task::{PackKey, RunId};
use super::traits::{Ingested, UntrustedLimits};

/// An operation, as its task holds it.
pub(crate) struct Op {
    id: OperationId,
    key: ActionKey,
    whole_machine: bool,
    /// Whether a recorded call waits on it.
    recorded: bool,
    /// When the Execute that opened it came.
    created: Instant,
}

impl Op {
    /// Operation `id` of `key`, opened now.
    pub(crate) fn new(
        id: OperationId,
        key: ActionKey,
        whole_machine: bool,
        recorded: bool,
    ) -> Self {
        Self {
            id,
            key,
            whole_machine,
            recorded,
            created: Instant::now(),
        }
    }
}

/// Where an operation's task begins.
pub(crate) enum Start {
    /// With its first attempt, nothing submitted yet.
    Fresh,
    /// With the attempt its row in the state store names, already submitted by a process that
    /// is gone.
    Attached(OpRow),
}

/// A run's job, as the reads of it need it.
struct Submitted {
    /// Its id.
    job: String,
    /// The put directory its pack is committed into.
    key: PackKey,
}

/// How one run ended.
enum Ended {
    /// It answers the callers.
    Answer(Finished),
    /// It produced no result: what decides whether another attempt follows, and the job's output
    /// for the callers if none does.
    Again(Settled, Option<Output>),
    /// A caller cancelled it.
    Cancelled,
    /// Another process runs the operation now.
    HandedOver,
    /// Another frontend runs an operation of the same action: this one follows it.
    Joined(OperationId),
}

/// `digest` as REAPI spells it.
fn reapi_digest(digest: &kbf_types::Digest) -> reapi::Digest {
    reapi::Digest {
        hash: digest.hash_hex(),
        size_bytes: i64::try_from(digest.size_bytes).unwrap_or(i64::MAX),
    }
}

/// What the callers of a cancelled operation are answered.
fn cancelled() -> Finished {
    Finished::Failed(rpc::Status {
        code: Code::Cancelled as i32,
        message: "the operation was cancelled".to_owned(),
        details: Vec::new(),
    })
}

/// Completes when a caller has cancelled the operation; never, if none does.
async fn cancel_asked(cancel: &mut watch::Receiver<bool>) {
    if cancel.wait_for(|asked| *asked).await.is_err() {
        pending::<()>().await;
    }
}

/// Runs `f` on the state store's rows off the runtime's threads: the store's calls block.
async fn on_rows<T: Send + 'static>(
    inner: &Arc<Inner>,
    f: impl FnOnce(&Rows) -> T + Send + 'static,
) -> Option<T> {
    inner.rows.as_ref()?;
    let inner = Arc::clone(inner);
    let done = tokio::task::spawn_blocking(move || inner.rows.as_ref().map(f)).await;
    done.ok().flatten()
}

/// Runs `op` from `start` on a task of its own, and answers its callers INTERNAL if that task
/// breaks, so no operation is left open with nothing to run it.
pub(crate) fn spawn(inner: Arc<Inner>, op: Op, start: Start, cancel: watch::Receiver<bool>) {
    let (operation, key) = (op.id, op.key.clone());
    tokio::spawn(async move {
        let task = tokio::spawn(run(Arc::clone(&inner), op, start, cancel));
        if let Err(error) = task.await {
            tracing::error!(%operation, %error, "the task of an operation broke");
            let broke = Finished::Failed(rpc::Status {
                code: Code::Internal as i32,
                message: "the executor's task of the operation broke".to_owned(),
                details: Vec::new(),
            });
            inner.ops().finish(operation, broke.clone(), Instant::now());
            write_later(inner, operation, key, broke).await;
        }
    });
}

/// Writes the answer `finished` of `operation` of `key`, which its callers have, to the state
/// store until the store takes it, every poll: its rows, the action's among them, go with it,
/// so no later Execute of the action follows an operation no one answers in the store. Ends
/// at once without a store, and when another process answered or runs the operation.
async fn write_later(
    inner: Arc<Inner>,
    operation: OperationId,
    key: ActionKey,
    finished: Finished,
) {
    let retention = inner.cfg.finished_retention;
    loop {
        let (key, answer) = (key.clone(), finished.clone());
        let written = on_rows(&inner, move |rows| {
            rows.finish(operation, &key, &answer, retention)
        })
        .await;
        match written {
            Some(Err(RowsError::Store(error))) => {
                tracing::warn!(%operation, %error, "the answer is not in the state store yet: writing it again");
                sleep(inner.cfg.poll).await;
            }
            _ => return,
        }
    }
}

/// Drives `op` from `start` until a run answers its callers.
pub(crate) async fn run(
    inner: Arc<Inner>,
    op: Op,
    start: Start,
    mut cancel: watch::Receiver<bool>,
) {
    let (mut attempt, mut attached) = match start {
        Start::Fresh => (0, None),
        Start::Attached(row) => (row.attempt, Some(row)),
    };
    let metrics = Arc::clone(&inner.deps.metrics);
    let mut began = op.created;
    let answer = loop {
        let run = RunId::new(op.id, attempt);
        let mut timeline = Timeline::default();
        timeline.stamp(Mark::Seen, began, &metrics);
        let ended = match attached.take() {
            Some(row) => resume(&inner, &op, run, row, &mut cancel, &mut timeline).await,
            None => fresh(&inner, &op, run, &mut cancel, &mut timeline).await,
        };
        match ended {
            Ended::Answer(finished) => break Some((run, finished)),
            Ended::Cancelled => break Some((run, cancelled())),
            Ended::HandedOver => break None,
            Ended::Joined(other) => {
                tracing::info!(operation = %op.id, %other, "another frontend runs the action: the operation follows its answer");
                metrics.joined();
                match follow_other(&inner, other, &op.key, &mut cancel).await {
                    None => break Some((run, cancelled())),
                    Some(Some(finished)) => break Some((run, finished)),
                    Some(None) => {
                        began = Instant::now();
                        continue;
                    }
                }
            }
            Ended::Again(settled, output) => {
                metrics.run_ended(RunEnd::of(&settled));
                if settled.retried() && attempt < RESUBMITS {
                    tracing::warn!(operation = %op.id, %run, ?settled, "the run ended for a reason of the farm's: the operation runs again");
                    metrics.attempt_resubmitted();
                    if op.recorded {
                        record::requeued(&inner.deps.recorder, run);
                    }
                    attempt += 1;
                    began = Instant::now();
                    continue;
                }
                let mut status = settled.status();
                if let Some(output) = output {
                    output.attach(&mut status);
                }
                tracing::warn!(operation = %op.id, %run, code = status.code, message = %status.message, "the operation finished failed");
                break Some((run, Finished::Failed(status)));
            }
        }
    };
    match answer {
        Some((run, finished)) => finish(&inner, &op, run, finished).await,
        None => {
            tracing::info!(operation = %op.id, "another frontend runs the operation now: its callers follow its answer");
            inner.ops().hand_over(op.id);
            tokio::spawn(follow_task(inner, op.id));
        }
    }
}

/// Writes the answer of `op` to the state store, when there is one, and answers its callers.
async fn finish(inner: &Arc<Inner>, op: &Op, run: RunId, finished: Finished) {
    let (id, key, retention) = (op.id, op.key.clone(), inner.cfg.finished_retention);
    let answer = finished.clone();
    let written = on_rows(inner, move |rows| rows.finish(id, &key, &answer, retention)).await;
    let finished = match written {
        None => finished,
        Some(Ok(answer)) => answer,
        Some(Err(RowsError::Conflict { .. })) => {
            inner.ops().hand_over(op.id);
            tokio::spawn(follow_task(Arc::clone(inner), op.id));
            return;
        }
        Some(Err(error)) => {
            tracing::warn!(operation = %op.id, %error, "the answer is not in the state store: it is written again until it is");
            let later = write_later(Arc::clone(inner), op.id, op.key.clone(), finished.clone());
            tokio::spawn(later);
            finished
        }
    };
    let metrics = &inner.deps.metrics;
    metrics.phase_took(Phase::Execute, op.created.elapsed());
    if op.recorded {
        record::finished(&inner.deps.recorder, op.id, Some(run), &finished);
    }
    inner.ops().finish(op.id, finished, Instant::now());
}

/// Runs `run` from its preparation.
async fn fresh(
    inner: &Arc<Inner>,
    op: &Op,
    run: RunId,
    cancel: &mut watch::Receiver<bool>,
    timeline: &mut Timeline,
) -> Ended {
    let metrics = &inner.deps.metrics;
    let action = reapi_digest(&op.key.action);
    let preparer = Preparer {
        cfg: &inner.cfg,
        inputs: &inner.deps.inputs,
        ingest: &inner.deps.ingest,
        metrics,
    };
    let booking = Booking::of(op.whole_machine);
    let prepared = tokio::select! {
        biased;
        () = cancel_asked(cancel) => return Ended::Cancelled,
        prepared = timeout(inner.cfg.prepare_limit, prepare(&preparer, run, &action, booking)) => prepared,
    };
    let prepared = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(settled)) => return Ended::Again(settled, None),
        Err(_) => {
            let why = "the run's preparation took longer than its limit".to_owned();
            return Ended::Again(Settled::Infra(why), None);
        }
    };
    timeline.stamp(Mark::Prepared, Instant::now(), metrics);
    let job = prepared.spec.job_id.clone();
    let row = OpRow {
        owner: String::new(),
        instance: op.key.instance.clone(),
        hash: op.key.action.hash_hex(),
        size: op.key.action.size_bytes,
        attempt: run.attempt,
        job: job.clone(),
        key: Some(prepared.key.as_str().to_owned()),
        whole_machine: op.whole_machine,
    };
    let id = op.id;
    let first = run.attempt == 0;
    let put_row = async {
        let started = Instant::now();
        let written = on_rows(inner, move |rows| {
            if first {
                rows.open_op(id, &row)
            } else {
                rows.put_op(id, &row).map(|()| Opened::Ours)
            }
        })
        .await;
        if written.is_some() {
            metrics.step_took(Step::Row, started.elapsed());
        }
        written
    };
    // The row is written while the tree's writes end; the job is submitted only once the tree
    // is stored, so its host never opens a tree the store does not have yet.
    let (tree, written) = tokio::join!(tree_written(inner, run, cancel), put_row);
    match written {
        Some(Err(RowsError::Conflict { .. })) => return Ended::HandedOver,
        Some(Err(error)) => {
            tracing::warn!(operation = %op.id, %error, "the operation's row is not in the state store: no other frontend can take it up");
        }
        Some(Ok(Opened::Joined(other))) => return Ended::Joined(other),
        Some(Ok(Opened::Ours)) | None => {}
    }
    if let Err(ended) = tree {
        return ended;
    }
    let answer = tokio::select! {
        biased;
        () = cancel_asked(cancel) => {
            // The submit may have been taken with its answer lost.
            withdraw(inner, &job).await;
            return Ended::Cancelled;
        }
        answer = submit_job(inner, &prepared.spec) => answer,
    };
    if let Err(settled) = answer {
        return Ended::Again(settled, None);
    }
    metrics.job_submitted();
    timeline.stamp(Mark::Posted, Instant::now(), metrics);
    tracing::debug!(operation = %op.id, %run, %job, "submitted");
    if op.recorded {
        record::granted(&inner.deps.recorder, run, &job);
    }
    let submitted = Submitted {
        job,
        key: prepared.key,
    };
    watch_job(inner, op, run, &submitted, cancel, timeline).await
}

/// Submits `spec` until the job service answers with the job, sending it again every poll
/// while the answer is lost: the same spec under the same id, so the service makes one job.
/// Each call is timed as [`Step::Rpc`] and each wait as [`Step::Backoff`].
///
/// # Errors
/// What the service's refusal of the job settles the run as.
async fn submit_job(inner: &Inner, spec: &JobSpec) -> Result<(), Settled> {
    let (api, metrics) = (inner.api(), &inner.deps.metrics);
    loop {
        let started = Instant::now();
        let answer = api.submit_job(spec.clone()).await;
        metrics.step_took(Step::Rpc, started.elapsed());
        let Err(error) = answer else {
            return Ok(());
        };
        if let Some(settled) = refused(&error) {
            return Err(settled);
        }
        note("submitting a job", &Err::<(), _>(error));
        metrics.job_resubmitted();
        let started = Instant::now();
        sleep(inner.cfg.poll).await;
        metrics.step_took(Step::Backoff, started.elapsed());
    }
}

/// Waits for the writes of `run`'s action tree ([`Ingest::stored`](super::Ingest::stored)),
/// timed as [`Step::TreeWait`] and held to the run's preparation limit. A failed write ends the
/// run as the mint's failure would have; a caller's cancel ends it cancelled.
async fn tree_written(
    inner: &Arc<Inner>,
    run: RunId,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), Ended> {
    let started = Instant::now();
    let stored = timeout(inner.cfg.prepare_limit, inner.deps.ingest.stored(run));
    let stored = tokio::select! {
        biased;
        () = cancel_asked(cancel) => return Err(Ended::Cancelled),
        stored = stored => stored,
    };
    inner
        .deps
        .metrics
        .step_took(Step::TreeWait, started.elapsed());
    let settled = match stored {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(error)) => not_minted(error),
        Err(_) => Settled::Infra(
            "the run's action tree was not written within the preparation's limit".to_owned(),
        ),
    };
    Err(Ended::Again(settled, None))
}

/// Runs `run` from the row a gone process left: its job read back where it is.
async fn resume(
    inner: &Arc<Inner>,
    op: &Op,
    run: RunId,
    row: OpRow,
    cancel: &mut watch::Receiver<bool>,
    timeline: &mut Timeline,
) -> Ended {
    let Some(key) = row.key else {
        withdraw(inner, &row.job).await;
        let why = "the operation's row names no put directory to read the job's pack from";
        return Ended::Again(Settled::Lost(why), None);
    };
    tracing::info!(operation = %op.id, %run, job = %row.job, "taken up from the state store");
    let submitted = Submitted {
        job: row.job,
        key: PackKey::new(key),
    };
    watch_job(inner, op, run, &submitted, cancel, timeline).await
}

/// Cancels `job` for its operation's callers, a few times while the service does not answer.
async fn withdraw(inner: &Inner, job: &str) {
    for _ in 0..3 {
        let answer = inner.api().cancel_job(job).await;
        note("cancelling a job", &answer);
        if answer.is_ok() {
            inner.deps.metrics.job_cancelled();
        }
        if answered(&answer) {
            return;
        }
    }
}

/// Reads the job back every poll until it ends, and makes what it ended as of it.
async fn watch_job(
    inner: &Arc<Inner>,
    op: &Op,
    run: RunId,
    submitted: &Submitted,
    cancel: &mut watch::Receiver<bool>,
    timeline: &mut Timeline,
) -> Ended {
    let metrics = &inner.deps.metrics;
    let job = submitted.job.as_str();
    let (mut running, mut silence) = (false, false);
    let (read, result) = loop {
        tokio::select! {
            biased;
            () = cancel_asked(cancel) => {
                withdraw(inner, job).await;
                return Ended::Cancelled;
            }
            () = sleep(inner.cfg.poll) => {}
        }
        let read = match inner.api().get_job(job).await {
            Ok(read) => read,
            Err(error) if not_found(&error) => {
                tracing::warn!(operation = %op.id, %run, %job, "the job service no longer knows the job");
                return Ended::Again(forgotten(), None);
            }
            Err(error) => {
                tracing::warn!(operation = %op.id, %run, %error, "reading the job failed");
                continue;
            }
        };
        if read.host_silent_since_ms.is_some() && !std::mem::replace(&mut silence, true) {
            tracing::warn!(operation = %op.id, %run, host = %read.host_id, "the host of a job has missed its grace");
            metrics.host_silence();
        }
        match read.state() {
            JobState::Queued | JobState::Placed | JobState::Starting => continue,
            JobState::Running => {
                if !std::mem::replace(&mut running, true) {
                    timeline.stamp(Mark::Running, Instant::now(), metrics);
                    inner.ops().show(op.id, Stage::Executing);
                    if op.recorded {
                        record::started(&inner.deps.recorder, run);
                    }
                }
                continue;
            }
            // A state this server does not know may be live or ended: the result says which.
            _ => {}
        }
        match inner.api().get_result(job).await {
            Ok(result) => break (read, result),
            Err(error) if not_found(&error) => {
                let output = output(inner, run, job, None).await;
                return Ended::Again(forgotten(), Some(output));
            }
            // Asked again at the next poll.
            Err(error) => {
                tracing::warn!(operation = %op.id, %run, %error, "reading the result of a job failed");
            }
        }
    };
    timeline.stamp(Mark::Ended, Instant::now(), metrics);
    match judge(&read, &result) {
        Verdict::Settled(settled) => {
            let output = output(inner, run, job, Some(&result)).await;
            Ended::Again(settled, Some(output))
        }
        Verdict::StoreLost { object, settled } => {
            // Before the next attempt is prepared: its tree is then written checked.
            let action = reapi_digest(&op.key.action);
            inner.deps.ingest.lost(&action, &object).await;
            let output = output(inner, run, job, Some(&result)).await;
            Ended::Again(settled, Some(output))
        }
        Verdict::Ingest => {
            let ingested = ingest(inner, run, &op.key, &submitted.key).await;
            timeline.stamp(Mark::Done, Instant::now(), metrics);
            match ingested {
                Ingested::Ran { result, .. } => {
                    metrics.ingested(IngestEnd::Ok);
                    metrics.run_ended(RunEnd::Ran);
                    Ended::Answer(Finished::Ran(result))
                }
                Ingested::Reject(why) => {
                    metrics.ingested(IngestEnd::Rejected(why));
                    Ended::Again(Settled::Infra(format!("the pack was refused: {why}")), None)
                }
                Ingested::Unavailable(why) => {
                    metrics.ingested(IngestEnd::Unavailable);
                    let why = format!("the pack could not be read: {why}");
                    Ended::Again(Settled::Infra(why), None)
                }
            }
        }
    }
}

/// The frontend's read of the pack `key` of `run` of `action`.
async fn ingest(inner: &Inner, run: RunId, action: &ActionKey, key: &PackKey) -> Ingested {
    let cfg = &inner.cfg;
    let limits = UntrustedLimits {
        max_pack_bytes: cfg.max_pack_bytes,
        max_blob_bytes: cfg.max_blob_bytes,
        max_object_entries: cfg.max_object_entries,
        max_object_manifest_bytes: cfg.max_object_manifest_bytes,
        object_bytes_per_entry: cfg.object_bytes_per_entry,
    };
    let digest = reapi_digest(&action.action);
    inner.deps.ingest.ingest(run, key, &digest, &limits).await
}

/// The output of `job`, which did not end well: the tails of its `result` if they hold any,
/// else one `GetLogs`; cut to the cap. The lengths are logged, and the end of the standard
/// error as a second line at WARN.
async fn output(inner: &Inner, run: RunId, job: &str, result: Option<&JobResult>) -> Output {
    let tail = match result.and_then(Tail::of_result) {
        Some(tail) => Ok(tail),
        None => inner.api().logs_tail(job).await,
    };
    let output = match tail {
        Ok(tail) => Output::of(&tail),
        Err(error) => Output::unread(&error),
    };
    tracing::info!(
        %run,
        stdout_bytes = output.stdout.len(),
        stderr_bytes = output.stderr.len(),
        truncated = output.truncated,
        unread = output.note.is_some(),
        "the output of a job that did not end well goes with its answer"
    );
    if let Some(excerpt) = output.stderr_excerpt(inner.cfg.failed_stderr_log_bytes) {
        // Debug-formatted, so a line break in the job's text cannot start a log line.
        tracing::warn!(
            %run,
            job,
            stderr = ?excerpt,
            cut = excerpt.len() < output.stderr.trim().len(),
            "the end of the standard error of a job that did not end well"
        );
    }
    output
}

/// The stage of a caller that asked by name for `operation` of `key`, which another frontend
/// runs: it shows the answer that frontend writes to the state store.
pub(crate) fn follow(
    inner: &Arc<Inner>,
    operation: OperationId,
    key: ActionKey,
) -> watch::Receiver<Stage> {
    let (stage, new) = inner.ops().follower(operation, key);
    if new {
        tokio::spawn(follow_task(Arc::clone(inner), operation));
    }
    stage
}

/// Follows `other`, the operation of the same action another frontend runs, until its answer:
/// `Some(Some(answer))` for this operation's callers; `Some(None)` when that answer is none this
/// operation can give (its own callers cancelled it), so this one runs the action itself; `None`
/// when a caller of this operation cancels it first, which leaves `other` running for its own.
async fn follow_other(
    inner: &Arc<Inner>,
    other: OperationId,
    key: &ActionKey,
    cancel: &mut watch::Receiver<bool>,
) -> Option<Option<Finished>> {
    let mut stage = follow(inner, other, key.clone());
    let answer = tokio::select! {
        biased;
        () = cancel_asked(cancel) => return None,
        done = stage.wait_for(|stage| matches!(stage, Stage::Done(_))) => done.ok().and_then(|stage| match &*stage {
            Stage::Done(finished) => Some(finished.clone()),
            _ => None,
        }),
    };
    let cancelled = |status: &rpc::Status| status.code == Code::Cancelled as i32;
    Some(
        answer
            .filter(|finished| !matches!(finished, Finished::Failed(status) if cancelled(status))),
    )
}

/// Reads the state store every poll until it has `operation`'s answer, and shows it to the
/// operation's followers. An operation whose row and answer are both gone is answered
/// UNAVAILABLE: its answer is past its retention.
async fn follow_task(inner: Arc<Inner>, operation: OperationId) {
    loop {
        sleep(inner.cfg.poll).await;
        if !inner.ops().is_followed(operation) {
            return;
        }
        let looked = on_rows(&inner, move |rows| rows.look(operation)).await;
        let unavailable = |message: &str| {
            Finished::Failed(rpc::Status {
                code: Code::Unavailable as i32,
                message: message.to_owned(),
                details: Vec::new(),
            })
        };
        let finished = match looked {
            Some(Ok(Look::Done(_, finished))) => finished,
            Some(Ok(Look::Gone)) => unavailable("the operation's answer is no longer kept"),
            Some(Ok(Look::Open(_))) => continue,
            Some(Err(error)) => {
                tracing::warn!(%operation, %error, "reading an operation another frontend runs failed");
                continue;
            }
            // The read did not come back (its task broke): the followers are not left waiting.
            None => unavailable("the state store could not be read for the operation"),
        };
        inner.ops().follow_done(operation, finished, Instant::now());
        return;
    }
}

#[cfg(test)]
#[path = "run_end_tests.rs"]
mod end_tests;
#[cfg(test)]
#[path = "run_store_tests.rs"]
mod store_tests;
#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "run_tree_tests.rs"]
mod tree_tests;
